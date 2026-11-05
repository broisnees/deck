//! AI usage for the usage panel and the status-bar meter: the plan limits
//! Claude Code and Codex report, and the tokens Claude Code's transcripts
//! record. Everything here blocks, so call it from a background thread.
//!
//! - `claude_limits() -> Result<Limits, String>`: Claude's plan windows
//!   ("5-hour", "Weekly", "Weekly Opus", ...) from Anthropic's OAuth usage
//!   endpoint, signed in as Claude Code is. Kept for 2 minutes; after a
//!   network or server failure it waits 5 minutes before asking again.
//! - `codex_limits() -> Option<Limits>`: Codex's windows, from the last
//!   `token_count` event in its newest session logs. `None` when Codex has
//!   never reported any.
//! - `claude_session(cwd, id) -> Option<SessionUse>`: one conversation's
//!   context fill (from the end of its transcript), and its tokens and cost
//!   (the whole transcript plus its sub-agents' transcripts).
//! - `claude_latest_session(cwd) -> Option<String>`: the most recently
//!   written conversation in `cwd`.
//! - `claude_today() -> DayUse`: tokens and cost across every project since
//!   local midnight.
//!
//! Percentages run 0-100 (a window can run past 100). Costs are dollars at
//! Anthropic's API prices: what the tokens would cost without a plan.
//! Transcripts are scanned once and then only their new lines are read, so
//! repeat calls stay cheap even on 200 MB files.

#![allow(dead_code)]

use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One rate-limit window.
#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    /// "5-hour", "Weekly", "Weekly Opus", "Monthly".
    pub label: String,
    /// Share of the window used, 0-100.
    pub used_pct: f32,
    pub resets_at: Option<SystemTime>,
}

/// An agent's plan and its windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    /// "claude" or "codex".
    pub agent: &'static str,
    /// "Max 5x", "Team (Max 5x)", "Pro", "Plus", "Free".
    pub plan: Option<String>,
    pub limits: Vec<Limit>,
    /// When the numbers were read: the request for Claude, the agent's own
    /// event for Codex.
    pub fetched: SystemTime,
}

/// One Claude Code conversation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionUse {
    /// The model of the last reply, e.g. "claude-opus-5-5".
    pub model: Option<String>,
    /// What the last reply read: its input, cache reads and cache writes.
    pub context_used: u64,
    /// 1,000,000 or 200,000.
    pub context_window: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Prompts you typed.
    pub turns: u32,
    /// `None` when no reply came from a model with a known price.
    pub cost_usd: Option<f64>,
}

/// Claude Code across every project since local midnight.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DayUse {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: Option<f64>,
    /// Conversations with a reply today.
    pub sessions: u32,
}

/// An agent with no plan limits to read (OpenCode, Gemini CLI): what it
/// used since local midnight, from its own records.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentDay {
    /// "opencode", "gemini".
    pub agent: &'static str,
    pub tokens: u64,
    /// What the agent itself priced it at; free models count nothing.
    pub cost_usd: f64,
    pub replies: u64,
    /// The model most of the tokens went to.
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Claude plan limits

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const EXPIRED: &str = "Claude Code's sign-in expired; open claude to refresh it";
const SIGNED_OUT: &str = "Claude Code isn't signed in to a Claude plan; run claude and log in";
const FRESH: Duration = Duration::from_secs(120);
const BACKOFF: Duration = Duration::from_secs(300);

/// The last answer, when it was got, and how long it stands.
type Kept = (Instant, Result<Limits, String>, Duration);
static CLAUDE_LIMITS: Mutex<Option<Kept>> = Mutex::new(None);

enum Failure {
    /// Sign-in trouble: checked again as often as a good answer.
    Sign(String),
    /// The network or the server: backed off.
    Net(String),
}

/// Claude's plan windows, as Claude Code's `/usage` shows them.
pub fn claude_limits() -> Result<Limits, String> {
    // Held across the request, so callers on other threads wait for it
    // rather than sending their own.
    let mut slot = CLAUDE_LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, answer, keep)) = slot.as_ref() {
        if at.elapsed() < *keep {
            return answer.clone();
        }
    }
    let (answer, keep) = match fetch_claude_limits() {
        Ok(l) => (Ok(l), FRESH),
        Err(Failure::Sign(e)) => (Err(e), FRESH),
        Err(Failure::Net(e)) => (Err(e), BACKOFF),
    };
    *slot = Some((Instant::now(), answer.clone(), keep));
    answer
}

fn fetch_claude_limits() -> Result<Limits, Failure> {
    let creds = claude_creds().ok_or_else(|| Failure::Sign(SIGNED_OUT.into()))?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if creds.expires_ms.is_some_and(|e| e <= now_ms) {
        return Err(Failure::Sign(EXPIRED.into()));
    }
    let (status, body) = get_usage(&creds.token)?;
    drop(creds.token);
    match status {
        200 => {}
        401 => return Err(Failure::Sign(EXPIRED.into())),
        429 => {
            return Err(Failure::Net(
                "Claude's usage service is busy; trying again soon".into(),
            ))
        }
        s => return Err(Failure::Net(format!("Claude's usage service answered {s}"))),
    }
    let limits = parse_claude_usage(&body)
        .ok_or_else(|| Failure::Net("Couldn't read Claude's usage answer".into()))?;
    Ok(Limits {
        agent: "claude",
        plan: creds.plan,
        limits,
        fetched: SystemTime::now(),
    })
}

/// Claude Code's OAuth sign-in. The token lives only as long as this does.
struct Creds {
    token: String,
    expires_ms: Option<u64>,
    plan: Option<String>,
}

/// Claude Code keeps its sign-in in the keychain, or in
/// `.credentials.json` in its config folder where there is no keychain.
fn claude_creds() -> Option<Creds> {
    keychain_creds()
        .and_then(|raw| parse_creds(&raw))
        .or_else(|| {
            let raw = std::fs::read(claude_home().join(".credentials.json")).ok()?;
            parse_creds(&raw)
        })
}

/// Off macOS Claude Code keeps no keychain item; the file is all there is.
#[cfg(not(target_os = "macos"))]
fn keychain_creds() -> Option<Vec<u8>> {
    None
}

/// Read with `security`, as Claude Code does, so no keychain prompt shows.
#[cfg(target_os = "macos")]
fn keychain_creds() -> Option<Vec<u8>> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", &keychain_service(), "-w"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// "Claude Code-credentials", with "-" and the first 8 hex digits of the
/// SHA-256 of the config folder after it when `CLAUDE_CONFIG_DIR` (or
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR`) moves it, as Claude Code names it.
#[cfg(target_os = "macos")]
fn keychain_service() -> String {
    let dir = match std::env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        Ok(d) => Some(d).filter(|d| !d.is_empty()),
        Err(_) => std::env::var("CLAUDE_CONFIG_DIR")
            .ok()
            .filter(|d| !d.is_empty()),
    };
    match dir {
        Some(d) => format!("Claude Code-credentials-{}", sha256_hex8(&d)),
        None => "Claude Code-credentials".into(),
    }
}

/// The first 4 bytes of a SHA-256, in hex, through CommonCrypto.
#[cfg(target_os = "macos")]
fn sha256_hex8(s: &str) -> String {
    extern "C" {
        fn CC_SHA256(data: *const std::ffi::c_void, len: u32, md: *mut u8) -> *mut u8;
    }
    let mut md = [0u8; 32];
    // SAFETY: CC_SHA256 reads `len` bytes and writes 32 into `md`.
    unsafe { CC_SHA256(s.as_ptr().cast(), s.len() as u32, md.as_mut_ptr()) };
    md[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_creds(raw: &[u8]) -> Option<Creds> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let o = v.get("claudeAiOauth")?;
    let token = o
        .get("accessToken")?
        .as_str()
        .filter(|t| !t.is_empty())?
        .to_string();
    Some(Creds {
        token,
        expires_ms: o.get("expiresAt").and_then(Value::as_u64),
        plan: claude_plan(
            o.get("subscriptionType").and_then(Value::as_str),
            o.get("rateLimitTier").and_then(Value::as_str),
        ),
    })
}

/// "max" with tier "default_claude_max_20x" is "Max 20x"; "team" with a
/// Max tier is "Team (Max 5x)".
fn claude_plan(sub: Option<&str>, tier: Option<&str>) -> Option<String> {
    let sub = sub.filter(|s| !s.is_empty())?;
    let name = capitalize(sub);
    let tier = tier.unwrap_or("");
    let mult = tier.split('_').find(|w| {
        w.len() > 1 && w.ends_with('x') && w[..w.len() - 1].bytes().all(|b| b.is_ascii_digit())
    });
    Some(match mult {
        Some(m) if sub == "max" => format!("Max {m}"),
        Some(m) if tier.contains("max") => format!("{name} (Max {m})"),
        _ => name,
    })
}

/// One GET through `/usr/bin/curl`, the token handed over on its stdin as
/// a config file so it never shows on a command line, nor lands in a URL
/// cache on disk. The status, and the body.
fn get_usage(token: &str) -> Result<(u16, Vec<u8>), Failure> {
    use std::process::{Command, Stdio};
    if token
        .bytes()
        .any(|b| !b.is_ascii_graphic() || b == b'"' || b == b'\\')
    {
        return Err(Failure::Sign(EXPIRED.into()));
    }
    let unreachable = || Failure::Net("Couldn't reach Claude's usage service".into());
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--max-time",
            "15",
            "--proto",
            "=https",
            "--config",
            "-",
            "--write-out",
            "\n%{http_code}",
            USAGE_URL,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| unreachable())?;
    if let Some(mut stdin) = child.stdin.take() {
        let config = format!(
            "header = \"Authorization: Bearer {token}\"\n\
             header = \"anthropic-beta: oauth-2025-04-20\"\n\
             header = \"Accept: application/json\"\n\
             user-agent = \"Deck\"\n"
        );
        let _ = stdin.write_all(config.as_bytes());
    }
    let out = child.wait_with_output().map_err(|_| unreachable())?;
    let text = out.stdout;
    let cut = text
        .iter()
        .rposition(|&b| b == b'\n')
        .ok_or_else(unreachable)?;
    let status: u16 = std::str::from_utf8(&text[cut + 1..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(unreachable)?;
    if status == 0 {
        return Err(unreachable());
    }
    Ok((status, text[..cut].to_vec()))
}

/// The usage answer's windows. Its `limits` rows are what `/usage` draws
/// (`kind` "session", "weekly_all", "weekly_scoped" with a model); older
/// answers only have `five_hour`, `seven_day`, `seven_day_opus` and
/// `seven_day_sonnet`, each `{utilization, resets_at}`.
fn parse_claude_usage(body: &[u8]) -> Option<Vec<Limit>> {
    let v: Value = serde_json::from_slice(body).ok()?;
    if !v.is_object() {
        return None;
    }
    if let Some(rows) = v.get("limits").and_then(Value::as_array) {
        let out: Vec<Limit> = rows.iter().filter_map(limit_row).collect();
        if !out.is_empty() {
            return Some(out);
        }
    }
    let named = [
        ("five_hour", "5-hour"),
        ("seven_day", "Weekly"),
        ("seven_day_opus", "Weekly Opus"),
        ("seven_day_sonnet", "Weekly Sonnet"),
    ];
    Some(
        named
            .iter()
            .filter_map(|(key, label)| {
                let w = v.get(key)?;
                Some(Limit {
                    label: label.to_string(),
                    used_pct: w.get("utilization")?.as_f64()? as f32,
                    resets_at: w
                        .get("resets_at")
                        .and_then(Value::as_str)
                        .and_then(parse_iso),
                })
            })
            .collect(),
    )
}

fn limit_row(row: &Value) -> Option<Limit> {
    let kind = row.get("kind")?.as_str()?;
    let scoped = ["model", "surface"].iter().find_map(|k| {
        row.get("scope")?
            .get(k)?
            .get("display_name")?
            .as_str()
            .filter(|n| !n.is_empty())
    });
    let label = match (kind, scoped) {
        ("session", _) => "5-hour".to_string(),
        ("weekly_all", _) => "Weekly".to_string(),
        (k, Some(name)) if k.starts_with("weekly") => format!("Weekly {name}"),
        (k, Some(name)) => format!("{} {name}", kind_label(k)),
        (k, None) => kind_label(k),
    };
    Some(Limit {
        label,
        used_pct: row.get("percent")?.as_f64()? as f32,
        resets_at: row
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_iso),
    })
}

/// "monthly_all" reads "Monthly".
fn kind_label(kind: &str) -> String {
    let words: Vec<&str> = kind
        .split('_')
        .filter(|w| !w.is_empty() && *w != "all" && *w != "scoped")
        .collect();
    capitalize(&words.join(" "))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Codex limits

/// Codex's windows from the newest session log that has them.
pub fn codex_limits() -> Option<Limits> {
    let root = codex_home()?.join("sessions");
    newest_logs(&root, 12)
        .iter()
        .find_map(|f| last_codex_limits(f, SystemTime::now()))
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

/// Up to `n` of `sessions/YYYY/MM/DD/*.jsonl`, the most recently written
/// first, walking back from the newest day.
fn newest_logs(root: &Path, n: usize) -> Vec<PathBuf> {
    fn dirs_desc(p: &Path) -> Vec<PathBuf> {
        let mut d: Vec<PathBuf> = std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        d.sort();
        d.reverse();
        d
    }
    let mut out = Vec::new();
    for year in dirs_desc(root) {
        for month in dirs_desc(&year) {
            for day in dirs_desc(&month) {
                let mut files = jsonl_in(&day);
                files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
                out.extend(files.into_iter().map(|(p, _)| p));
                if out.len() >= n {
                    out.truncate(n);
                    return out;
                }
            }
        }
    }
    out
}

/// The last `token_count` event with rate limits in a Codex log: its tail
/// first, the whole log when the tail has none.
fn last_codex_limits(path: &Path, now: SystemTime) -> Option<Limits> {
    let len = std::fs::metadata(path).ok()?.len();
    for size in [1u64 << 20, 64 << 20] {
        let (text, cut) = read_tail(path, size)?;
        let lines: Vec<&str> = text.lines().skip(cut as usize).collect();
        let found = lines
            .iter()
            .rev()
            .filter(|l| l.contains("\"token_count\"") && l.contains("\"rate_limits\""))
            .find_map(|l| parse_codex_event(l, now));
        if found.is_some() || size >= len {
            return found;
        }
    }
    None
}

/// `{"timestamp", "payload": {"type": "token_count", "rate_limits":
/// {"primary"|"secondary": {used_percent, window_minutes, resets_at |
/// resets_in_seconds}, "plan_type"}}}`. A window whose reset has passed
/// reads 0%, since nothing has been used in the new one.
fn parse_codex_event(line: &str, now: SystemTime) -> Option<Limits> {
    let v: Value = serde_json::from_str(line).ok()?;
    let p = v.get("payload")?;
    if p.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let rl = p.get("rate_limits")?.as_object()?;
    let at = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_iso);
    let limits: Vec<Limit> = ["primary", "secondary"]
        .iter()
        .filter_map(|key| {
            let w = rl.get(*key)?.as_object()?;
            let pct = w.get("used_percent")?.as_f64()?;
            let resets = w
                .get("resets_at")
                .and_then(Value::as_f64)
                .map(|s| UNIX_EPOCH + Duration::from_secs_f64(s.max(0.)))
                .or_else(|| {
                    let s = w.get("resets_in_seconds")?.as_f64()?;
                    Some(at? + Duration::from_secs_f64(s.max(0.)))
                });
            let label = match w.get("window_minutes").and_then(Value::as_u64) {
                Some(m) => window_label(m),
                None if *key == "primary" => "5-hour".into(),
                None => "Weekly".into(),
            };
            let (pct, resets) = match resets {
                Some(r) if r <= now => (0., None),
                r => (pct, r),
            };
            Some(Limit {
                label,
                used_pct: pct as f32,
                resets_at: resets,
            })
        })
        .collect();
    if limits.is_empty() {
        return None;
    }
    Some(Limits {
        agent: "codex",
        plan: rl
            .get("plan_type")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(capitalize),
        limits,
        fetched: at.unwrap_or(now),
    })
}

/// 300 minutes reads "5-hour", a week "Weekly", 30 days "Monthly".
fn window_label(minutes: u64) -> String {
    match minutes {
        300 => "5-hour".into(),
        1440 => "Daily".into(),
        10080 => "Weekly".into(),
        40320..=44640 => "Monthly".into(),
        m if m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-minute"),
    }
}

// ---------------------------------------------------------------------------
// Claude Code transcripts

/// The newest conversation in `cwd`'s project folder.
pub fn claude_latest_session(cwd: &Path) -> Option<String> {
    crate::session::claude_sessions(cwd).into_iter().next()
}

/// One conversation's context fill, tokens and cost.
pub fn claude_session(cwd: &Path, session_id: &str) -> Option<SessionUse> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let dir = crate::session::claude_dir(cwd);
    session_use(&dir, session_id)
}

fn session_use(dir: &Path, id: &str) -> Option<SessionUse> {
    let path = dir.join(format!("{id}.jsonl"));
    let mut t = with_scan(&path, |s| s.totals(i64::MIN))?;
    for (sub, _) in jsonl_in(&dir.join(id).join("subagents")) {
        // A sub-agent's prompts came from the main agent, not from you.
        if let Some(st) = with_scan(&sub, |s| s.totals(i64::MIN)) {
            t.merge(&Tally { turns: 0, ..st });
        }
    }
    let ctx = tail_context(&path, TAIL).or_else(|| {
        with_scan(&path, |s| {
            s.last_main.as_ref().map(|r| context_of(r, s.wide))
        })
        .flatten()
    });
    let (model, context_used, context_window) = match ctx {
        Some(c) => (c.model, c.used, c.window),
        None => (None, 0, 200_000),
    };
    Some(SessionUse {
        model,
        context_used,
        context_window,
        input: t.input,
        output: t.output,
        cache_read: t.cache_read,
        cache_write: t.cache_write,
        turns: t.turns,
        cost_usd: t.cost_usd(),
    })
}

static TODAY: Mutex<Option<(Instant, i64, DayUse)>> = Mutex::new(None);

/// Claude Code's use across every project since local midnight. Kept for
/// 20 seconds.
pub fn claude_today() -> DayUse {
    let midnight = local_midnight();
    let mut slot = TODAY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, m, day)) = slot.as_ref() {
        if *m == midnight && at.elapsed() < Duration::from_secs(20) {
            return day.clone();
        }
    }
    let day = day_use(&claude_home().join("projects"), midnight);
    *slot = Some((Instant::now(), midnight, day.clone()));
    day
}

/// Seconds since the epoch at the start of the local day.
fn local_midnight() -> i64 {
    let (epoch, (h, m, s)) = crate::retry::now();
    epoch as i64 - (h * 3600 + m * 60 + s) as i64
}

/// Every transcript under `projects` written since `midnight`, counting the
/// replies stamped since then. Sub-agents count toward their conversation.
fn day_use(projects: &Path, midnight: i64) -> DayUse {
    let since = UNIX_EPOCH + Duration::from_secs(midnight.max(0) as u64);
    let from = midnight.div_euclid(BUCKET);
    let mut total = Tally::default();
    let mut sessions: HashSet<String> = HashSet::new();
    let mut count = |path: &Path, key: &str| {
        if let Some(t) = with_scan(path, |s| s.totals(from)) {
            if t.messages > 0 {
                sessions.insert(key.to_string());
            }
            total.merge(&t);
        }
    };
    for project in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        for entry in std::fs::read_dir(project.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_file() {
                let Some(stem) = name.strip_suffix(".jsonl") else {
                    continue;
                };
                if meta.modified().is_ok_and(|m| m >= since) {
                    count(&path, stem);
                }
            } else if meta.is_dir() {
                for (sub, modified) in jsonl_in(&path.join("subagents")) {
                    if modified >= since {
                        count(&sub, &name);
                    }
                }
            }
        }
    }
    DayUse {
        input: total.input,
        output: total.output,
        cache_read: total.cache_read,
        cache_write: total.cache_write,
        cost_usd: total.cost_usd(),
        sessions: sessions.len() as u32,
    }
}

/// Claude Code's config folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().unwrap_or_default().join(".claude"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The `*.jsonl` files in `dir` and when each was last written.
fn jsonl_in(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "jsonl" {
                return None;
            }
            let meta = e.metadata().ok()?;
            meta.is_file().then_some((p, meta.modified().ok()?))
        })
        .collect()
}

/// Tokens of one reply. Cache writes are split by how long they last,
/// since the hour-long cache costs more to write.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tokens {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_5m: u64,
    cache_1h: u64,
    fast: bool,
}

impl Tokens {
    fn cache_write(&self) -> u64 {
        self.cache_5m + self.cache_1h
    }

    /// What the reply read: the context it had.
    fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write()
    }
}

/// Sums over replies.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Tally {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    priced: u32,
    unpriced: u32,
    messages: u32,
    turns: u32,
}

impl Tally {
    fn add(&mut self, model: Option<&str>, t: &Tokens) {
        self.input += t.input;
        self.output += t.output;
        self.cache_read += t.cache_read;
        self.cache_write += t.cache_write();
        self.messages += 1;
        match model.and_then(|m| cost(m, t)) {
            Some(c) => {
                self.cost += c;
                self.priced += 1;
            }
            None => self.unpriced += 1,
        }
    }

    fn merge(&mut self, o: &Tally) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
        self.priced += o.priced;
        self.unpriced += o.unpriced;
        self.messages += o.messages;
        self.turns += o.turns;
    }

    /// The cost of the replies with a known price; `None` when there were
    /// replies and none had one.
    fn cost_usd(&self) -> Option<f64> {
        (self.priced > 0 || self.unpriced == 0).then_some(self.cost)
    }
}

/// One assistant reply in a transcript.
#[derive(Clone, Debug, PartialEq)]
struct Reply {
    /// Its quarter hour since the epoch, for counting by local day.
    bucket: i64,
    model: Option<String>,
    tokens: Tokens,
    /// In the conversation itself, not a sub-agent's side thread.
    main: bool,
}

/// Seconds in a bucket: a quarter hour, so every time zone's midnight
/// falls on a bucket's edge.
const BUCKET: i64 = 900;
/// Replies kept apart until no later line can repeat them.
const RECENT: usize = 64;
/// How much of a transcript's end to read for its context.
const TAIL: u64 = 2 << 20;

/// What is known of a transcript after reading it up to `offset`.
#[derive(Default)]
struct Scan {
    len: u64,
    modified: Option<SystemTime>,
    /// Just past the last whole line read.
    offset: u64,
    buckets: BTreeMap<i64, Tally>,
    /// The latest replies by message id. Claude Code writes one line per
    /// content block, each with the reply's usage so far, so a reply counts
    /// once, as its last line has it.
    recent: VecDeque<(String, Reply)>,
    /// The settled replies again, split by model, for the usage page.
    models: BTreeMap<i64, HashMap<String, Tally>>,
    last_main: Option<Reply>,
    /// A model id with `[1m]` in it.
    wide: bool,
    /// The folder the conversation started in, from its first `cwd`.
    cwd: Option<String>,
}

impl Scan {
    /// Sums over the replies from `from_bucket` on.
    fn totals(&self, from_bucket: i64) -> Tally {
        let mut t = Tally::default();
        for (_, b) in self.buckets.range(from_bucket..) {
            t.merge(b);
        }
        for (_, r) in &self.recent {
            if r.bucket >= from_bucket {
                t.add(r.model.as_deref(), &r.tokens);
            }
        }
        t
    }

    fn reply(&mut self, id: Option<String>, reply: Reply) {
        if reply.main {
            self.last_main = Some(reply.clone());
        }
        let Some(id) = id else {
            self.settle(reply);
            return;
        };
        if let Some(slot) = self.recent.iter_mut().find(|(k, _)| *k == id) {
            slot.1 = reply;
            return;
        }
        self.recent.push_back((id, reply));
        if self.recent.len() > RECENT {
            if let Some((_, old)) = self.recent.pop_front() {
                self.settle(old);
            }
        }
    }

    fn settle(&mut self, r: Reply) {
        self.buckets
            .entry(r.bucket)
            .or_default()
            .add(r.model.as_deref(), &r.tokens);
        self.models
            .entry(r.bucket)
            .or_default()
            .entry(r.model.clone().unwrap_or_default())
            .or_default()
            .add(r.model.as_deref(), &r.tokens);
    }

    /// (bucket, model, tally) for every reply from `from_bucket` on.
    fn by_model(&self, from_bucket: i64) -> Vec<(i64, String, Tally)> {
        let mut out: Vec<(i64, String, Tally)> = self
            .models
            .range(from_bucket..)
            .flat_map(|(b, m)| m.iter().map(move |(k, t)| (*b, k.clone(), *t)))
            .collect();
        for (_, r) in &self.recent {
            if r.bucket >= from_bucket {
                let mut t = Tally::default();
                t.add(r.model.as_deref(), &r.tokens);
                out.push((r.bucket, r.model.clone().unwrap_or_default(), t));
            }
        }
        out
    }

    fn line(&mut self, line: &str) {
        if self.cwd.is_none() {
            self.cwd = json_str(line, "cwd").filter(|c| !c.is_empty());
        }
        if line.contains("\"usage\"") && line.contains("\"assistant\"") {
            if let Some(p) = parse_reply(line) {
                self.wide |= p.wide;
                self.reply(p.id, p.reply);
                return;
            }
        }
        if line.contains("\"type\":\"user\"") && !line.contains("\"tool_result\"") {
            if let Some(bucket) = prompt_bucket(line) {
                self.buckets.entry(bucket).or_default().turns += 1;
            }
        }
    }

    /// Reads the lines written since the last call. A line still being
    /// written is left for next time.
    fn extend(&mut self, path: &Path) -> std::io::Result<()> {
        let mut f = std::fs::File::open(path)?;
        f.seek(SeekFrom::Start(self.offset))?;
        let mut r = std::io::BufReader::with_capacity(1 << 20, f);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf)?;
            if n == 0 || buf.last() != Some(&b'\n') {
                return Ok(());
            }
            self.offset += n as u64;
            if let Ok(line) = std::str::from_utf8(&buf) {
                self.line(line);
            }
        }
    }
}

static SCANS: LazyLock<Mutex<HashMap<PathBuf, Scan>>> = LazyLock::new(Default::default);

/// `f` over the transcript at `path`, read up to its end. The scan is kept
/// between calls, so only new lines are read again.
fn with_scan<R>(path: &Path, f: impl FnOnce(&Scan) -> R) -> Option<R> {
    let meta = std::fs::metadata(path).ok()?;
    let (len, modified) = (meta.len(), meta.modified().ok());
    let lock = || SCANS.lock().unwrap_or_else(|e| e.into_inner());
    // Taken out while it reads, so other files aren't held up.
    let mut scan = lock().remove(path).unwrap_or_default();
    if scan.len != len || scan.modified != modified {
        if len < scan.offset {
            scan = Scan::default();
        }
        if scan.extend(path).is_err() {
            scan = Scan::default();
            scan.extend(path).ok()?;
        }
        scan.len = len;
        scan.modified = modified;
    }
    let out = f(&scan);
    let mut scans = lock();
    if scans.len() > 2048 {
        scans.clear();
    }
    scans.insert(path.to_path_buf(), scan);
    Some(out)
}

/// The string value of the first `"key":"..."` in a JSON line, found
/// without parsing the whole line.
fn json_str(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len() - 1;
    let rest = &line.as_bytes()[start + 1..];
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'\\' => i += 2,
            b'"' => return serde_json::from_str(&line[start..start + i + 2]).ok(),
            _ => i += 1,
        }
    }
    None
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    #[serde(rename = "isMeta")]
    meta: Option<bool>,
    #[serde(rename = "isCompactSummary")]
    summary: Option<bool>,
    timestamp: Option<String>,
    #[serde(rename = "requestedModel")]
    requested: Option<String>,
    origin: Option<Origin>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Origin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<CacheSplit>,
    speed: Option<String>,
}

#[derive(Deserialize)]
struct CacheSplit {
    ephemeral_1h_input_tokens: Option<u64>,
}

struct Parsed {
    id: Option<String>,
    reply: Reply,
    wide: bool,
}

/// An assistant line with usage. Claude Code's own placeholder replies
/// (model `<synthetic>`) made no request, so they are skipped.
fn parse_reply(line: &str) -> Option<Parsed> {
    let l: Line = serde_json::from_str(line).ok()?;
    if l.kind.as_deref() != Some("assistant") {
        return None;
    }
    let m = l.message?;
    let u = m.usage?;
    if m.model.as_deref() == Some("<synthetic>") {
        return None;
    }
    let write = u.cache_creation_input_tokens.unwrap_or(0);
    let hour = u
        .cache_creation
        .and_then(|c| c.ephemeral_1h_input_tokens)
        .unwrap_or(0)
        .min(write);
    let wide = [&m.model, &l.requested]
        .iter()
        .any(|s| s.as_deref().is_some_and(|s| s.contains("[1m]")));
    Some(Parsed {
        id: m.id,
        wide,
        reply: Reply {
            bucket: l
                .timestamp
                .as_deref()
                .and_then(iso_secs)
                .map_or(i64::MIN, |s| s.div_euclid(BUCKET)),
            model: m.model.or(l.requested),
            tokens: Tokens {
                input: u.input_tokens.unwrap_or(0),
                output: u.output_tokens.unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                cache_5m: write - hour,
                cache_1h: hour,
                fast: u.speed.as_deref() == Some("fast"),
            },
            main: !l.sidechain.unwrap_or(false),
        },
    })
}

/// A prompt you typed: a user line from a person (`origin.kind` "human"),
/// or on older Claude Codes with no origin, one whose content is plain
/// text and not a note Claude Code added or the summary that continues a
/// compacted conversation. The bucket it falls in.
fn prompt_bucket(line: &str) -> Option<i64> {
    let l: Line = serde_json::from_str(line).ok()?;
    let aside = [l.meta, l.sidechain, l.summary].contains(&Some(true));
    if l.kind.as_deref() != Some("user") || aside {
        return None;
    }
    let typed = match &l.origin {
        Some(o) => o.kind.as_deref() == Some("human"),
        None => line.contains("\"role\":\"user\",\"content\":\""),
    };
    if !typed {
        return None;
    }
    Some(
        l.timestamp
            .as_deref()
            .and_then(iso_secs)
            .map_or(i64::MIN, |s| s.div_euclid(BUCKET)),
    )
}

struct Context {
    model: Option<String>,
    used: u64,
    window: u64,
}

fn context_of(r: &Reply, wide: bool) -> Context {
    let used = r.tokens.context();
    Context {
        window: window_for(r.model.as_deref(), wide, used),
        model: r.model.clone(),
        used,
    }
}

/// The context of the last reply in the conversation itself, read from
/// the last `size` bytes, or four times that while the tail holds none.
fn tail_context(path: &Path, size: u64) -> Option<Context> {
    let len = std::fs::metadata(path).ok()?.len();
    let mut size = size.max(1);
    loop {
        let (text, cut) = read_tail(path, size)?;
        let mut wide = false;
        let mut last = None;
        for line in text.lines().skip(cut as usize) {
            if line.contains("\"usage\"") && line.contains("\"assistant\"") {
                if let Some(p) = parse_reply(line) {
                    wide |= p.wide;
                    if p.reply.main {
                        last = Some(p.reply);
                    }
                }
            }
        }
        if let Some(r) = last {
            return Some(context_of(&r, wide));
        }
        if size >= len || size >= 64 << 20 {
            return None;
        }
        size *= 4;
    }
}

/// The last `size` bytes of a file as text, and how many lines to skip
/// at its start (1 when it starts mid-line).
fn read_tail(path: &Path, size: u64) -> Option<(String, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(size);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(size).read_to_end(&mut buf).ok()?;
    Some((
        String::from_utf8_lossy(&buf).into_owned(),
        (start > 0) as u64,
    ))
}

/// 1,000,000 for a `[1m]` model, a model whose window is a million, or a
/// reply that read more than 200,000; else 200,000.
fn window_for(model: Option<&str>, wide: bool, used: u64) -> u64 {
    // Claude's models with a 1M window by default, per Anthropic's model
    // table of 2026-10-06.
    const MILLION: &[&str] = &[
        "fable",
        "mythos",
        "opus-5",
        "sonnet-5",
        "haiku-5",
        "opus-4-8",
        "opus-4-7",
        "opus-4-6",
        "sonnet-4-6",
    ];
    let native = model.is_some_and(|m| MILLION.iter().any(|k| m.contains(k)));
    if wide || native || used > 200_000 {
        1_000_000
    } else {
        200_000
    }
}

/// Dollars per million tokens: input, output, cache read, matched as a
/// part of the model id, the more specific first. Writing to the cache
/// costs 1.25x input for five minutes and 2x for an hour.
///
/// Anthropic's API prices as of 2026-10-06, from the model and pricing
/// tables of the claude-api skill bundled with Claude Code 2.1.301, which
/// mirror platform.claude.com/docs/en/about-claude/pricing. Haiku 5.5's
/// cache read is assumed at the usual 0.1x of input.
/// OpenAI's list prices, for Codex's history on the usage page only: an
/// estimate, as for Claude, since plans don't bill by the token.
const OPENAI_PRICES: &[(&str, [f64; 3])] = &[
    ("gpt-5-mini", [0.25, 2., 0.025]),
    ("gpt-5-nano", [0.05, 0.4, 0.005]),
    ("gpt-5", [1.25, 10., 0.125]),
    ("codex-mini", [1.5, 6., 0.375]),
];

const PRICES: &[(&str, [f64; 3])] = &[
    ("fable-5-1", [10., 50., 0.25]),
    ("mythos-5-1", [10., 50., 0.25]),
    ("fable-5", [10., 50., 1.]),
    ("mythos-5", [10., 50., 1.]),
    ("opus-5-5", [4., 20., 0.2]),
    ("opus-5", [5., 25., 0.5]),
    ("opus-4-8", [5., 25., 0.5]),
    ("opus-4-7", [5., 25., 0.5]),
    ("opus-4-6", [5., 25., 0.5]),
    ("opus-4-5", [5., 25., 0.5]),
    ("opus-4", [15., 75., 1.5]),
    ("3-opus", [15., 75., 1.5]),
    ("sonnet-5", [2., 10., 0.2]),
    ("sonnet-4", [3., 15., 0.3]),
    ("3-7-sonnet", [3., 15., 0.3]),
    ("3-5-sonnet", [3., 15., 0.3]),
    ("haiku-5-5", [0.1, 0.5, 0.01]),
    ("haiku-4-5", [1., 5., 0.1]),
    ("3-5-haiku", [0.8, 4., 0.08]),
    ("3-haiku", [0.25, 1.25, 0.03]),
];

/// One reply's cost in dollars, or `None` for a model with no price.
/// Fast mode costs twice as much, and Haiku 5.5 five times as much once a
/// prompt passes 100,000 tokens.
fn cost(model: &str, t: &Tokens) -> Option<f64> {
    let [input, output, read] = PRICES.iter().find(|(k, _)| model.contains(k))?.1;
    let mut scale = if t.fast { 2. } else { 1. };
    if model.contains("haiku-5-5") && t.context() > 100_000 {
        scale *= 5.;
    }
    let dollars = t.input as f64 * input
        + t.output as f64 * output
        + t.cache_read as f64 * read
        + t.cache_5m as f64 * input * 1.25
        + t.cache_1h as f64 * input * 2.;
    Some(scale * dollars / 1e6)
}

/// `claude-opus-4-1-20250805` as "Opus 4.1", `claude-sonnet-4-5[1m]` as
/// "Sonnet 4.5 (1M)"; anything else as it came.
pub fn model_name(id: &str) -> String {
    let (id, long) = match id.strip_suffix("[1m]") {
        Some(base) => (base, true),
        None => (id, false),
    };
    let Some(rest) = id.strip_prefix("claude-") else {
        return id.to_string();
    };
    let mut parts = rest.split('-');
    let Some(family) = parts.next().filter(|f| !f.is_empty()) else {
        return id.to_string();
    };
    // A trailing date is a snapshot, not part of the name.
    let version: Vec<&str> = parts
        .filter(|p| !(p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit())))
        .collect();
    let mut name = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name = format!("{name} {}", version.join("."));
    }
    if long {
        name.push_str(" (1M)");
    }
    name
}

// ---------------------------------------------------------------------------
// Time

/// An ISO 8601 time such as "2026-10-16T12:30:00.348202+00:00".
fn parse_iso(s: &str) -> Option<SystemTime> {
    let (secs, nanos) = iso_parts(s)?;
    let secs = u64::try_from(secs).ok()?;
    Some(UNIX_EPOCH + Duration::new(secs, nanos))
}

/// Whole seconds since the epoch.
fn iso_secs(s: &str) -> Option<i64> {
    iso_parts(s).map(|p| p.0)
}

fn iso_parts(s: &str) -> Option<(i64, u32)> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut nanos = 0u32;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let mut n = 0u64;
        for (i, c) in frac[..digits].bytes().enumerate().take(9) {
            n += (c - b'0') as u64 * 10u64.pow(8 - i as u32);
        }
        nanos = n as u32;
        rest = &frac[digits..];
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let z: String = rest[1..].chars().filter(|c| *c != ':').collect();
            if z.len() != 4 || !z.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let mins = z[..2].parse::<i64>().ok()? * 60 + z[2..].parse::<i64>().ok()?;
            if sign == b'+' {
                mins * 60
            } else {
                -mins * 60
            }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec - offset, nanos))
}

/// Days since 1970-01-01 of a Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_read_like_the_product() {
        assert_eq!(model_name("claude-opus-4-1-20250805"), "Opus 4.1");
        assert_eq!(model_name("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(model_name("claude-sonnet-4-5[1m]"), "Sonnet 4.5 (1M)");
        assert_eq!(model_name("gpt-5-codex"), "gpt-5-codex");
        assert_eq!(model_name("claude-"), "claude-");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("deck-usage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn assistant(
        id: &str,
        ts: &str,
        model: &str,
        inp: u64,
        out: u64,
        read: u64,
        write: u64,
    ) -> String {
        format!(
            r#"{{"parentUuid":"p","isSidechain":false,"message":{{"id":"{id}","model":"{model}","role":"assistant","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":{inp},"cache_creation_input_tokens":{write},"cache_read_input_tokens":{read},"output_tokens":{out},"service_tier":"standard","cache_creation":{{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":{write}}}}}}},"requestedModel":"{model}","type":"assistant","uuid":"u","timestamp":"{ts}"}}"#
        )
    }

    fn prompt(ts: &str, text: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"promptId":"x","type":"user","message":{{"role":"user","content":"{text}"}},"uuid":"u","timestamp":"{ts}","origin":{{"kind":"human"}}}}"#
        )
    }

    const TOOL_RESULT: &str = r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"usage assistant"}]},"timestamp":"2026-10-16T08:00:05.000Z"}"#;

    #[test]
    fn iso_times_parse() {
        assert_eq!(iso_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_secs("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(
            iso_secs("2026-10-16T12:30:00.348202+00:00"),
            Some(1_792_153_800)
        );
        assert_eq!(iso_secs("2026-10-21T14:00:00+05:45"), Some(1_792_570_500));
        assert_eq!(iso_parts("2026-10-16T08:14:30.069Z").unwrap().1, 69_000_000);
        assert_eq!(iso_secs("yesterday"), None);
        assert_eq!(iso_secs("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn duplicate_message_ids_count_once_as_their_last_line() {
        let dir = temp("dupes");
        let path = dir.join("s.jsonl");
        let lines = [
            prompt("2026-10-16T08:00:00.000Z", "fix it"),
            // One reply split over three lines, its output growing.
            assistant(
                "msg_a",
                "2026-10-16T08:00:01.000Z",
                "claude-opus-5-5",
                10,
                1,
                1000,
                50,
            ),
            assistant(
                "msg_a",
                "2026-10-16T08:00:02.000Z",
                "claude-opus-5-5",
                10,
                5,
                1000,
                50,
            ),
            TOOL_RESULT.to_string(),
            assistant(
                "msg_a",
                "2026-10-16T08:00:03.000Z",
                "claude-opus-5-5",
                10,
                9,
                1000,
                50,
            ),
            assistant(
                "msg_b",
                "2026-10-16T08:00:04.000Z",
                "claude-opus-5-5",
                2,
                3,
                1060,
                7,
            ),
        ];
        let summary = prompt(
            "2026-10-16T08:00:00.500Z",
            "This session is being continued",
        )
        .replace(
            r#","origin":{"kind":"human"}"#,
            r#","isCompactSummary":true"#,
        );
        std::fs::write(&path, lines.join("\n") + "\n" + &summary + "\n").unwrap();
        let s = session_use(&dir, "s").unwrap();
        assert_eq!(
            (s.input, s.output, s.cache_read, s.cache_write),
            (12, 12, 2060, 57)
        );
        assert_eq!(s.turns, 1);
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.context_used, 2 + 1060 + 7);
        assert_eq!(s.context_window, 1_000_000);

        // Appended lines are read on their own, and a reply that goes on
        // across the old end still counts once.
        let more = [
            assistant(
                "msg_b",
                "2026-10-16T08:00:05.000Z",
                "claude-opus-5-5",
                2,
                30,
                1060,
                7,
            ),
            prompt("2026-10-16T08:01:00.000Z", "again"),
        ];
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{}", more.join("\n")).unwrap();
        // A half-written line waits.
        write!(
            f,
            "{}",
            &assistant(
                "msg_c",
                "2026-10-16T08:01:01.000Z",
                "claude-opus-5-5",
                1,
                1,
                1,
                1
            )[..40]
        )
        .unwrap();
        drop(f);
        let s = session_use(&dir, "s").unwrap();
        assert_eq!(
            (s.input, s.output, s.cache_read, s.cache_write),
            (12, 39, 2060, 57)
        );
        assert_eq!(s.turns, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sub_agents_count_toward_their_session_but_not_its_context() {
        let dir = temp("subs");
        std::fs::write(
            dir.join("s.jsonl"),
            assistant(
                "m1",
                "2026-10-16T08:00:00Z",
                "claude-sonnet-4-5",
                100,
                10,
                0,
                0,
            ) + "\n",
        )
        .unwrap();
        let subs = dir.join("s").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        let side = assistant(
            "m2",
            "2026-10-16T08:00:01Z",
            "claude-haiku-4-5",
            5000,
            50,
            0,
            0,
        )
        .replace("\"isSidechain\":false", "\"isSidechain\":true");
        let ask = prompt("2026-10-16T08:00:00Z", "look around")
            .replace(r#","origin":{"kind":"human"}"#, "");
        std::fs::write(subs.join("agent-1.jsonl"), format!("{ask}\n{side}\n")).unwrap();
        let s = session_use(&dir, "s").unwrap();
        assert_eq!((s.input, s.output), (5100, 60));
        assert_eq!(s.turns, 0);
        assert_eq!(s.context_used, 100);
        assert_eq!(s.context_window, 200_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_comes_from_the_tail() {
        let dir = temp("tail");
        let path = dir.join("big.jsonl");
        let mut body = assistant(
            "old",
            "2026-10-16T07:00:00Z",
            "claude-haiku-4-5",
            1,
            1,
            1,
            1,
        ) + "\n";
        let filler = format!(r#"{{"type":"progress","data":"{}"}}"#, "x".repeat(10_000));
        for _ in 0..300 {
            body.push_str(&filler);
            body.push('\n');
        }
        body.push_str(&assistant(
            "new",
            "2026-10-16T08:00:00Z",
            "claude-haiku-4-5",
            3,
            4,
            150_000,
            2_000,
        ));
        body.push('\n');
        // A sub-agent's reply after it is not the conversation's context.
        body.push_str(
            &assistant(
                "side",
                "2026-10-16T08:00:01Z",
                "claude-haiku-4-5",
                9,
                9,
                9,
                9,
            )
            .replace("\"isSidechain\":false", "\"isSidechain\":true"),
        );
        body.push('\n');
        std::fs::write(&path, &body).unwrap();
        let c = tail_context(&path, 64 << 10).unwrap();
        assert_eq!(c.used, 152_003);
        assert_eq!(c.window, 200_000);
        // A tail with no reply in it looks further back.
        let only_old = format!(
            "{}\n{}\n",
            assistant("o", "2026-10-16T07:00:00Z", "claude-haiku-4-5", 7, 0, 0, 0),
            filler
        );
        std::fs::write(&path, only_old).unwrap();
        assert_eq!(tail_context(&path, 1024).unwrap().used, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn million_token_windows_are_recognised() {
        assert_eq!(
            window_for(Some("claude-haiku-4-5"), false, 150_000),
            200_000
        );
        assert_eq!(
            window_for(Some("claude-haiku-4-5"), false, 200_001),
            1_000_000
        );
        assert_eq!(
            window_for(Some("claude-sonnet-4-5[1m]"), true, 10),
            1_000_000
        );
        assert_eq!(window_for(Some("claude-opus-5-5"), false, 10), 1_000_000);
        assert_eq!(window_for(None, false, 10), 200_000);
        // `[1m]` in the requested model marks the session.
        let line = assistant("m", "2026-10-16T08:00:00Z", "claude-sonnet-4-5", 1, 1, 1, 1).replace(
            "\"requestedModel\":\"claude-sonnet-4-5\"",
            "\"requestedModel\":\"claude-sonnet-4-5[1m]\"",
        );
        assert!(parse_reply(&line).unwrap().wide);
        let synthetic = assistant("m", "2026-10-16T08:00:00Z", "<synthetic>", 0, 0, 0, 0);
        assert!(parse_reply(&synthetic).is_none());
    }

    #[test]
    fn prices_add_up() {
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_5m: 1_000_000,
            cache_1h: 1_000_000,
            fast: false,
        };
        // 4 + 20 + 0.2 + 5 + 8
        assert!((cost("claude-opus-5-5", &t).unwrap() - 37.2).abs() < 1e-9);
        // 3 + 15 + 0.3 + 3.75 + 6
        assert!((cost("claude-sonnet-4-5-20250929", &t).unwrap() - 28.05).abs() < 1e-9);
        // 15 + 75 + 1.5 + 18.75 + 30
        assert!((cost("claude-opus-4-1-20250805", &t).unwrap() - 140.25).abs() < 1e-9);
        assert!(
            (cost("claude-opus-5-5", &Tokens { fast: true, ..t }).unwrap() - 74.4).abs() < 1e-9
        );
        let small = Tokens {
            input: 50_000,
            output: 1_000,
            ..Default::default()
        };
        assert!((cost("claude-haiku-5-5", &small).unwrap() - 0.0055).abs() < 1e-12);
        let big = Tokens {
            input: 200_000,
            ..Default::default()
        };
        assert!((cost("claude-haiku-5-5", &big).unwrap() - 0.1).abs() < 1e-12);
        assert_eq!(cost("gpt-5", &t), None);

        let mut tally = Tally::default();
        tally.add(Some("mystery-1"), &small);
        assert_eq!(tally.cost_usd(), None);
        tally.add(Some("claude-haiku-4-5"), &small);
        assert!((tally.cost_usd().unwrap() - 0.055).abs() < 1e-12);
        assert_eq!(Tally::default().cost_usd(), Some(0.));
    }

    #[test]
    fn hour_long_cache_writes_cost_more() {
        let line = r#"{"type":"assistant","timestamp":"2026-10-16T08:00:00Z","message":{"id":"m","model":"claude-opus-5-5","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":1000000,"cache_creation":{"ephemeral_5m_input_tokens":250000,"ephemeral_1h_input_tokens":750000}}}}"#;
        let r = parse_reply(line).unwrap().reply;
        assert_eq!((r.tokens.cache_5m, r.tokens.cache_1h), (250_000, 750_000));
        // 0.25 * 5 + 0.75 * 8
        assert!((cost("claude-opus-5-5", &r.tokens).unwrap() - 7.25).abs() < 1e-9);
    }

    #[test]
    fn today_counts_only_replies_since_midnight() {
        let root = temp("today");
        let proj = root.join("-Users-me-app");
        std::fs::create_dir_all(proj.join("two/subagents")).unwrap();
        let one = [
            assistant(
                "y",
                "2026-10-15T23:59:00Z",
                "claude-opus-5-5",
                1000,
                1000,
                0,
                0,
            ),
            assistant("t", "2026-10-16T00:00:00Z", "claude-opus-5-5", 1, 2, 3, 4),
        ];
        std::fs::write(proj.join("one.jsonl"), one.join("\n") + "\n").unwrap();
        std::fs::write(
            proj.join("two.jsonl"),
            prompt("2026-10-16T01:00:00Z", "hi") + "\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("two/subagents/agent-a.jsonl"),
            assistant(
                "s",
                "2026-10-16T01:00:01Z",
                "claude-haiku-4-5",
                10,
                20,
                30,
                40,
            ) + "\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("three.jsonl"),
            prompt("2026-10-16T02:00:00Z", "nothing yet") + "\n",
        )
        .unwrap();
        let midnight = iso_secs("2026-10-16T00:00:00Z").unwrap();
        let d = day_use(&root, midnight);
        assert_eq!(
            (d.input, d.output, d.cache_read, d.cache_write),
            (11, 22, 33, 44)
        );
        assert_eq!(d.sessions, 2);
        assert!(d.cost_usd.unwrap() > 0.);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn codex_events_parse() {
        let now = UNIX_EPOCH + Duration::from_secs(1_792_100_000);
        let monthly = r#"{"timestamp":"2026-10-16T05:40:55.170Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":62432},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":3.0,"window_minutes":43200,"resets_at":1794024686},"secondary":null,"credits":{"has_credits":false},"plan_type":"free","rate_limit_reached_type":null}}}"#;
        let l = parse_codex_event(monthly, now).unwrap();
        assert_eq!(l.agent, "codex");
        assert_eq!(l.plan.as_deref(), Some("Free"));
        assert_eq!(
            l.limits,
            vec![Limit {
                label: "Monthly".into(),
                used_pct: 3.,
                resets_at: Some(UNIX_EPOCH + Duration::from_secs(1_794_024_686)),
            }]
        );
        assert_eq!(l.fetched, parse_iso("2026-10-16T05:40:55.170Z").unwrap());

        let relative = r#"{"timestamp":"2026-10-16T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":42.5,"window_minutes":300,"resets_in_seconds":600},"secondary":{"used_percent":12.0,"window_minutes":10080,"resets_in_seconds":86400},"plan_type":"plus"}}}"#;
        let l = parse_codex_event(relative, now).unwrap();
        let start = parse_iso("2026-10-16T00:00:00Z").unwrap();
        assert_eq!(l.plan.as_deref(), Some("Plus"));
        assert_eq!(l.limits[0].label, "5-hour");
        assert_eq!(l.limits[0].used_pct, 42.5);
        assert_eq!(
            l.limits[0].resets_at,
            Some(start + Duration::from_secs(600))
        );
        assert_eq!(l.limits[1].label, "Weekly");
        // Read after the 5-hour window reset: nothing used in the new one.
        let later = start + Duration::from_secs(3600);
        let l = parse_codex_event(relative, later).unwrap();
        assert_eq!((l.limits[0].used_pct, l.limits[0].resets_at), (0., None));
        assert_eq!(l.limits[1].used_pct, 12.);

        let none = r#"{"timestamp":"2026-10-16T00:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":null}}"#;
        assert!(parse_codex_event(none, now).is_none());
        assert_eq!(window_label(120), "2-hour");
        assert_eq!(window_label(2880), "2-day");

        // The newest log with limits wins over a newer one without.
        let root = temp("codex");
        let day = root.join("2026/10/16");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-a.jsonl"), format!("{monthly}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(day.join("rollout-b.jsonl"), format!("{none}\n")).unwrap();
        let logs = newest_logs(&root, 12);
        assert!(logs[0].ends_with("rollout-b.jsonl"), "{logs:?}");
        let found = logs.iter().find_map(|f| last_codex_limits(f, now)).unwrap();
        assert_eq!(found.limits[0].label, "Monthly");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A real answer from the usage endpoint (2026-10-16), trimmed of the
    /// fields that are always null.
    const USAGE_SAMPLE: &str = r#"{
        "five_hour": {"utilization": 14.0, "resets_at": "2026-10-16T12:30:00.348202+00:00", "limit_dollars": null, "used_dollars": null, "remaining_dollars": null, "locked_reason": null},
        "seven_day": {"utilization": 58.0, "resets_at": "2026-10-21T14:00:00.348225+00:00", "limit_dollars": null, "used_dollars": null, "remaining_dollars": null, "locked_reason": null},
        "seven_day_oauth_apps": null, "seven_day_opus": null, "seven_day_sonnet": null,
        "extra_usage": {"is_enabled": false, "monthly_limit": null, "used_credits": null, "utilization": null, "currency": null},
        "limits": [
            {"kind": "session", "group": "session", "percent": 14, "severity": "normal", "resets_at": "2026-10-16T12:30:00.348202+00:00", "scope": null, "is_active": false},
            {"kind": "weekly_all", "group": "weekly", "percent": 58, "severity": "normal", "resets_at": "2026-10-21T14:00:00.348225+00:00", "scope": null, "is_active": true},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal", "resets_at": "2026-10-21T14:00:00+00:00", "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": false}
        ],
        "member_dashboard_available": false
    }"#;

    #[test]
    fn usage_answers_parse() {
        let l = parse_claude_usage(USAGE_SAMPLE.as_bytes()).unwrap();
        let labels: Vec<_> = l.iter().map(|l| (l.label.as_str(), l.used_pct)).collect();
        assert_eq!(
            labels,
            vec![("5-hour", 14.), ("Weekly", 58.), ("Weekly Fable", 0.)]
        );
        assert_eq!(
            l[0].resets_at
                .map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs()),
            Some(1_792_153_800)
        );

        // Older answers without `limits`.
        let old = r#"{"five_hour":{"utilization":3.5,"resets_at":"2026-10-16T12:30:00Z"},"seven_day":{"utilization":40,"resets_at":null},"seven_day_opus":{"utilization":7,"resets_at":"2026-10-21T14:00:00Z"},"seven_day_sonnet":null}"#;
        let l = parse_claude_usage(old.as_bytes()).unwrap();
        let labels: Vec<_> = l
            .iter()
            .map(|l| (l.label.as_str(), l.used_pct, l.resets_at.is_some()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("5-hour", 3.5, true),
                ("Weekly", 40., false),
                ("Weekly Opus", 7., true)
            ]
        );
        assert!(parse_claude_usage(b"<html>").is_none());
        assert_eq!(kind_label("monthly_all"), "Monthly");
    }

    #[test]
    fn credentials_and_plans_read() {
        let raw = br#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-REDACTED","refreshToken":"x","expiresAt":1792146830662,"scopes":["user:inference"],"subscriptionType":"team","rateLimitTier":"default_claude_max_5x"},"mcpOAuth":{}}"#;
        let c = parse_creds(raw).unwrap();
        assert_eq!(c.expires_ms, Some(1_792_146_830_662));
        assert_eq!(c.plan.as_deref(), Some("Team (Max 5x)"));
        assert_eq!(
            claude_plan(Some("max"), Some("default_claude_max_20x")).as_deref(),
            Some("Max 20x")
        );
        assert_eq!(claude_plan(Some("pro"), None).as_deref(), Some("Pro"));
        assert!(parse_creds(br#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
        #[cfg(target_os = "macos")]
        assert_eq!(sha256_hex8("abc"), "ba7816bf");
    }
}

// ---------------------------------------------------------------------------
// Other agents' days

/// Every reply OpenCode stored today, from its own database. Read with the
/// system's `sqlite3`, read-only, so a running OpenCode is never disturbed.
pub fn opencode_today() -> Option<AgentDay> {
    let db = opencode_db()?;
    let query = format!(
        "select json_extract(data,'$.modelID'), count(*), \
         coalesce(sum(json_extract(data,'$.tokens.total')),0), \
         coalesce(sum(json_extract(data,'$.cost')),0) \
         from message where time_created >= {} and json_extract(data,'$.role')='assistant' \
         group by 1 order by 3 desc",
        local_midnight() * 1000
    );
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-separator", "\t"])
        .arg(&db)
        .arg(query)
        .output()
        .ok()?;
    out.status.success().then(|| opencode_rows(&String::from_utf8_lossy(&out.stdout)))
}

/// `model \t replies \t tokens \t cost` rows, the biggest first.
fn opencode_rows(text: &str) -> AgentDay {
    let mut day = AgentDay { agent: "opencode", ..Default::default() };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [model, replies, tokens, cost] = f[..] else { continue };
        day.replies += replies.parse::<u64>().unwrap_or(0);
        day.tokens += tokens.parse::<f64>().unwrap_or(0.) as u64;
        day.cost_usd += cost.parse::<f64>().unwrap_or(0.);
        if day.model.is_none() && !model.is_empty() {
            day.model = Some(model.to_string());
        }
    }
    day
}

/// Gemini CLI's replies today, from the chats it records under
/// `~/.gemini/tmp/<project>/chats`: each reply carries its `tokens` and a
/// UTC `timestamp`. Only files written today are opened.
pub fn gemini_today() -> Option<AgentDay> {
    let midnight = local_midnight();
    let since = UNIX_EPOCH + Duration::from_secs(midnight.max(0) as u64);
    let root = home()?.join(".gemini/tmp");
    let mut day = AgentDay { agent: "gemini", ..Default::default() };
    let mut models: HashMap<String, u64> = HashMap::new();
    for project in std::fs::read_dir(root).ok()?.flatten() {
        let Ok(chats) = std::fs::read_dir(project.path().join("chats")) else { continue };
        for chat in chats.flatten() {
            let fresh = chat.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since);
            if !fresh {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(chat.path()) else { continue };
            gemini_replies(&text, midnight, &mut day, &mut models);
        }
    }
    day.model = models.into_iter().max_by_key(|m| m.1).map(|m| m.0);
    (day.replies > 0).then_some(day)
}

/// Count the replies in one chat stamped at or after `midnight`.
fn gemini_replies(text: &str, midnight: i64, day: &mut AgentDay, models: &mut HashMap<String, u64>) {
    for r in gemini_messages(text).iter().filter_map(gemini_reply) {
        if r.secs < midnight {
            continue;
        }
        day.replies += 1;
        day.tokens += r.total;
        if let Some(model) = &r.model {
            *models.entry(model.clone()).or_default() += r.total;
        }
    }
}

/// The messages of one chat: one JSON document with a `messages` array, or
/// JSON lines where each message is a line and a `{"$set": {"messages":
/// [...]}}` line replaces them all. A message written again under the same
/// `id` counts once, as last written.
fn gemini_messages(text: &str) -> Vec<Value> {
    if let Ok(doc) = serde_json::from_str::<Value>(text) {
        if let Some(all) = doc.get("messages").and_then(Value::as_array) {
            return all.clone();
        }
    }
    let mut out: Vec<Value> = Vec::new();
    let put = |out: &mut Vec<Value>, m: Value| {
        let id = m.get("id").and_then(Value::as_str).map(str::to_string);
        match id.and_then(|id| out.iter().position(|o| o.get("id").and_then(Value::as_str) == Some(&id))) {
            Some(i) => out[i] = m,
            None => out.push(m),
        }
    };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(set) = v.get("$set") {
            if let Some(all) = set.get("messages").and_then(Value::as_array) {
                out.clear();
                for m in all {
                    put(&mut out, m.clone());
                }
            }
        } else if v.get("type").is_some() {
            put(&mut out, v);
        }
    }
    out
}

/// One Gemini CLI reply's tokens.
#[derive(Clone, Debug, PartialEq)]
struct GeminiReply {
    secs: i64,
    model: Option<String>,
    /// Prompt tokens not read from the cache, tool prompts included.
    input: u64,
    /// Output and thinking.
    output: u64,
    cache_read: u64,
    total: u64,
}

/// `{"timestamp", "model", "tokens": {"input", "output", "cached",
/// "thoughts", "tool", "total"}}`; `input` counts the cached tokens too.
fn gemini_reply(m: &Value) -> Option<GeminiReply> {
    let t = m.get("tokens")?;
    let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    let secs = m.get("timestamp").and_then(Value::as_str).and_then(iso_secs)?;
    let cache_read = n("cached");
    let mut input = n("input").saturating_sub(cache_read) + n("tool");
    let output = n("output") + n("thoughts");
    let total = t.get("total").and_then(Value::as_u64).unwrap_or(input + output + cache_read);
    if input + output + cache_read == 0 {
        input = total;
    }
    if total == 0 {
        return None;
    }
    Some(GeminiReply {
        secs,
        model: m.get("model").and_then(Value::as_str).map(str::to_string),
        input,
        output,
        cache_read,
        total,
    })
}

#[cfg(test)]
mod other_agents {
    use super::*;

    #[test]
    fn iso_times_match_the_epoch() {
        assert_eq!(iso_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_secs("2026-10-16T05:31:52.747Z"), Some(1_792_128_712));
        assert_eq!(iso_secs("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(iso_secs("nope"), None);
    }

    #[test]
    fn opencode_rows_sum_and_name_the_biggest_model() {
        let d = opencode_rows("deepseek-v4\t3\t15140\t0\nclaude-sonnet\t1\t900\t0.012\n");
        assert_eq!((d.replies, d.tokens), (4, 16_040));
        assert!((d.cost_usd - 0.012).abs() < 1e-9);
        assert_eq!(d.model.as_deref(), Some("deepseek-v4"));
    }

    #[test]
    fn gemini_counts_todays_replies_only() {
        let text = concat!(
            r#"{"type":"user","timestamp":"2026-10-16T05:31:52Z"}"#, "\n",
            r#"{"type":"gemini","timestamp":"2026-10-16T05:32:00Z","model":"gemini-3-pro","tokens":{"input":10,"output":5,"total":15}}"#, "\n",
            r#"{"type":"gemini","timestamp":"2026-10-15T05:32:00Z","tokens":{"total":99}}"#, "\n",
        );
        let (mut day, mut models) = (AgentDay::default(), HashMap::new());
        gemini_replies(text, iso_secs("2026-10-16T00:00:00Z").unwrap(), &mut day, &mut models);
        assert_eq!((day.replies, day.tokens), (1, 15));
        assert_eq!(models.get("gemini-3-pro"), Some(&15));
    }
}

// ---------------------------------------------------------------------------
// The usage page: days and models over a range

/// One model's use over the range.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUse {
    pub agent: &'static str,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// At API prices, or what the agent itself recorded; 0 for a model
    /// with neither.
    pub cost: f64,
    /// What reading from the cache saved against paying full input price.
    pub saved: f64,
}

impl ModelUse {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// One local day: its cost and tokens by agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DayTotal {
    /// Days from today: 0 today, -1 yesterday.
    pub day: i64,
    pub by_agent: Vec<(&'static str, f64, u64)>,
}

/// One project's use over the range, every agent together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectUse {
    /// The folder's own name, with its parent's when two share it.
    pub name: String,
    /// The folder the agent ran in; empty when it wasn't recorded.
    pub path: String,
    pub cost: f64,
    pub tokens: u64,
    /// Conversations with a reply in the range.
    pub sessions: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// How many days back, today included.
    pub days: u32,
    pub daily: Vec<DayTotal>,
    pub models: Vec<ModelUse>,
    /// Conversations with a reply in the range, by agent.
    pub sessions: Vec<(&'static str, u32)>,
    /// The most costly first.
    pub projects: Vec<ProjectUse>,
    /// Each day's use by agent and model, oldest day first: the CSV's rows.
    pub rows: Vec<(i64, ModelUse)>,
}

impl Report {
    pub fn cost(&self) -> f64 {
        self.models.iter().map(|m| m.cost).sum()
    }
    pub fn tokens(&self) -> u64 {
        self.models.iter().map(ModelUse::tokens).sum()
    }
}

/// One stretch of replies to book: an agent's replies on one model in one
/// conversation, at one time.
struct Entry<'a> {
    agent: &'static str,
    session: &'a str,
    /// The folder it ran in; may be empty.
    project: &'a str,
    secs: i64,
    model: &'a str,
    input: u64,
    output: u64,
    read: u64,
    write: u64,
    cost: f64,
}

/// Accumulates replies into days, models and projects.
#[derive(Default)]
struct Book {
    midnight: i64,
    days: BTreeMap<(i64, &'static str), (f64, u64)>,
    models: HashMap<(&'static str, String), ModelUse>,
    sessions: HashMap<&'static str, HashSet<String>>,
    projects: HashMap<String, (f64, u64, HashSet<(&'static str, String)>)>,
    rows: BTreeMap<(i64, &'static str, String), ModelUse>,
}

impl Book {
    fn day_of(&self, secs: i64) -> i64 {
        (secs - self.midnight).div_euclid(86_400)
    }

    fn add(&mut self, e: Entry) {
        let day = self.day_of(e.secs);
        let tokens = e.input + e.output + e.read + e.write;
        let d = self.days.entry((day, e.agent)).or_default();
        d.0 += e.cost;
        d.1 += tokens;
        let saved = PRICES
            .iter()
            .chain(OPENAI_PRICES)
            .find(|(k, _)| e.model.contains(k))
            .map_or(0., |(_, [inp, _, rd])| e.read as f64 * (inp - rd) / 1e6);
        let fresh = || ModelUse { agent: e.agent, model: e.model.to_string(), ..Default::default() };
        for m in [
            self.models.entry((e.agent, e.model.to_string())).or_insert_with(fresh),
            self.rows.entry((day, e.agent, e.model.to_string())).or_insert_with(fresh),
        ] {
            m.input += e.input;
            m.output += e.output;
            m.cache_read += e.read;
            m.cache_write += e.write;
            m.cost += e.cost;
            m.saved += saved;
        }
        self.sessions.entry(e.agent).or_default().insert(e.session.to_string());
        let p = self.projects.entry(e.project.trim_end_matches('/').to_string()).or_default();
        p.0 += e.cost;
        p.1 += tokens;
        p.2.insert((e.agent, e.session.to_string()));
    }

    fn finish(self, days: u32) -> Report {
        let first = -(days as i64 - 1);
        let mut daily: Vec<DayTotal> = (first..=0).map(|day| DayTotal { day, by_agent: Vec::new() }).collect();
        for ((day, agent), (cost, tokens)) in self.days {
            if let Some(d) = daily.iter_mut().find(|d| d.day == day) {
                d.by_agent.push((agent, cost, tokens));
            }
        }
        let mut models: Vec<ModelUse> = self.models.into_values().filter(|m| m.tokens() > 0).collect();
        models.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.tokens().cmp(&a.tokens())));
        let mut sessions: Vec<(&'static str, u32)> = self.sessions.into_iter().map(|(a, s)| (a, s.len() as u32)).collect();
        sessions.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let mut projects: Vec<ProjectUse> = self
            .projects
            .into_iter()
            .filter(|(_, (_, tokens, _))| *tokens > 0)
            .map(|(path, (cost, tokens, s))| ProjectUse {
                name: String::new(),
                path,
                cost,
                tokens,
                sessions: s.len() as u32,
            })
            .collect();
        name_projects(&mut projects);
        projects.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.tokens.cmp(&a.tokens)).then(a.name.cmp(&b.name)));
        let rows = self
            .rows
            .into_iter()
            .filter(|((day, _, _), m)| *day >= first && *day <= 0 && m.tokens() > 0)
            .map(|((day, _, _), m)| (day, m))
            .collect();
        Report { days, daily, models, sessions, projects, rows }
    }
}

/// Each project by its folder's name, "parent/name" where two share it, and
/// "Other" for replies whose folder wasn't recorded.
fn name_projects(projects: &mut [ProjectUse]) {
    let last = |p: &str, n: usize| -> String {
        let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return if p.is_empty() { "Other".into() } else { "/".into() };
        }
        parts[parts.len().saturating_sub(n)..].join("/")
    };
    let names: Vec<String> = projects.iter().map(|p| last(&p.path, 1)).collect();
    for (i, p) in projects.iter_mut().enumerate() {
        let shared = names.iter().filter(|n| **n == names[i]).count() > 1;
        p.name = if shared && !p.path.is_empty() { last(&p.path, 2) } else { names[i].clone() };
    }
}

/// Claude Code, Codex, OpenCode and Gemini CLI over the last `days` local
/// days, today included. Reads every record written in the range; Claude's
/// scans and Gemini's chats are kept, so asking again reads only what is
/// new. Slow the first time: call it off the main thread.
pub fn report(days: u32) -> Report {
    let days = days.max(1);
    let midnight = local_midnight();
    let from = midnight - (days as i64 - 1) * 86_400;
    let mut book = Book { midnight, ..Default::default() };
    claude_range(&claude_home().join("projects"), from, &mut book);
    if let Some(root) = codex_home().map(|h| h.join("sessions")) {
        codex_range(&root, from, &mut book);
    }
    opencode_range(from, &mut book);
    if let Some(root) = home().map(|h| h.join(".gemini")) {
        gemini_range(&root, from, &mut book);
    }
    book.finish(days)
}

/// A project folder's name under `~/.claude/projects` back to a path:
/// Claude Code writes each `/` (and `.`) as `-`, so this is a guess, used
/// only when a transcript names no `cwd`.
fn decode_project_dir(name: &str) -> String {
    if name.starts_with('-') {
        name.replace('-', "/")
    } else {
        name.to_string()
    }
}

fn claude_range(projects: &Path, from: i64, book: &mut Book) {
    let since = UNIX_EPOCH + Duration::from_secs(from.max(0) as u64);
    let from_bucket = from.div_euclid(BUCKET);
    let count = |path: &Path, key: &str, folder: &str, book: &mut Book| {
        if let Some((rows, cwd)) = with_scan(path, |s| (s.by_model(from_bucket), s.cwd.clone())) {
            let project = cwd.unwrap_or_else(|| decode_project_dir(folder));
            for (bucket, model, t) in rows {
                if model.is_empty() || t.messages == 0 {
                    continue;
                }
                book.add(Entry {
                    agent: "claude",
                    session: key,
                    project: &project,
                    secs: bucket * BUCKET,
                    model: &model,
                    input: t.input,
                    output: t.output,
                    read: t.cache_read,
                    write: t.cache_write,
                    cost: t.cost,
                });
            }
        }
    };
    for project in std::fs::read_dir(projects).into_iter().flatten().flatten() {
        let folder = project.file_name().to_string_lossy().into_owned();
        for entry in std::fs::read_dir(project.path()).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_file() {
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    if meta.modified().is_ok_and(|m| m >= since) {
                        count(&path, stem, &folder, book);
                    }
                }
            } else if meta.is_dir() {
                for (sub, modified) in jsonl_in(&path.join("subagents")) {
                    if modified >= since {
                        count(&sub, &name, &folder, book);
                    }
                }
            }
        }
    }
}

/// Codex logs under `sessions/YYYY/MM/DD/`.
fn codex_range(root: &Path, from: i64, book: &mut Book) {
    let since = UNIX_EPOCH + Duration::from_secs(from.max(0) as u64);
    let mut files = Vec::new();
    for year in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for month in std::fs::read_dir(year.path()).into_iter().flatten().flatten() {
            for day in std::fs::read_dir(month.path()).into_iter().flatten().flatten() {
                files.extend(jsonl_in(&day.path()).into_iter().filter(|(_, m)| *m >= since).map(|(p, _)| p));
            }
        }
    }
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let key = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        codex_log(&text, &key, from, book);
    }
}

/// One Codex log: each `token_count` event's `last_token_usage`, on the
/// model the turn ran, in the folder the session started in
/// (`session_meta`, else the first `turn_context`). Its `input_tokens`
/// include the cached ones.
fn codex_log(text: &str, key: &str, from: i64, book: &mut Book) {
    let mut model = String::from("codex");
    let mut cwd = String::new();
    for line in text.lines() {
        let meta = line.contains("\"session_meta\"");
        if meta || line.contains("\"turn_context\"") {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let p = &v["payload"];
            if let Some(c) = p["cwd"].as_str().filter(|c| !c.is_empty()) {
                if meta || cwd.is_empty() {
                    cwd = c.to_string();
                }
            }
            if let Some(m) = p["model"].as_str().filter(|_| !meta) {
                model = m.to_string();
            }
            continue;
        }
        if !line.contains("\"token_count\"") || !line.contains("\"last_token_usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(secs) = v["timestamp"].as_str().and_then(iso_secs) else { continue };
        if secs < from {
            continue;
        }
        let u = &v["payload"]["info"]["last_token_usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        let read = n("cached_input_tokens");
        let input = n("input_tokens").saturating_sub(read);
        let output = n("output_tokens");
        let write = n("cache_write_input_tokens");
        let cost = OPENAI_PRICES
            .iter()
            .find(|(k, _)| model.contains(k))
            .map_or(0., |(_, [i, o, r])| (input as f64 * i + output as f64 * o + read as f64 * r + write as f64 * i) / 1e6);
        book.add(Entry {
            agent: "codex",
            session: key,
            project: &cwd,
            secs,
            model: &model,
            input,
            output,
            read,
            write,
            cost,
        });
    }
}

/// OpenCode's database: `$XDG_DATA_HOME/opencode/opencode.db`.
fn opencode_db() -> Option<PathBuf> {
    let db = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".local/share")))?
        .join("opencode/opencode.db");
    db.exists().then_some(db)
}

/// OpenCode's replies since `from`, summed by quarter hour, conversation,
/// model and folder in one read-only query. Costs are OpenCode's own.
fn opencode_range(from: i64, book: &mut Book) {
    let Some(db) = opencode_db() else { return };
    let query = format!(
        "select min(m.time_created)/1000, m.session_id, \
         coalesce(json_extract(m.data,'$.modelID'),''), \
         sum(coalesce(json_extract(m.data,'$.tokens.input'),0)), \
         sum(coalesce(json_extract(m.data,'$.tokens.output'),0)+coalesce(json_extract(m.data,'$.tokens.reasoning'),0)), \
         sum(coalesce(json_extract(m.data,'$.tokens.cache.read'),0)), \
         sum(coalesce(json_extract(m.data,'$.tokens.cache.write'),0)), \
         sum(coalesce(json_extract(m.data,'$.cost'),0)), \
         coalesce(json_extract(m.data,'$.path.cwd'),s.directory,'') \
         from message m left join session s on s.id = m.session_id \
         where m.time_created >= {} and json_extract(m.data,'$.role')='assistant' \
         group by m.time_created/{}, 2, 3, 9",
        from * 1000,
        BUCKET * 1000
    );
    let Ok(out) = std::process::Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-separator", "\t"])
        .arg(&db)
        .arg(query)
        .output()
    else {
        return;
    };
    if out.status.success() {
        opencode_book(&String::from_utf8_lossy(&out.stdout), book);
    }
}

/// `secs \t session \t model \t input \t output \t cache read \t cache
/// write \t cost \t folder` rows into the book.
fn opencode_book(text: &str, book: &mut Book) {
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [secs, session, model, input, output, read, write, cost, cwd] = f[..] else { continue };
        let n = |s: &str| s.parse::<f64>().unwrap_or(0.).max(0.) as u64;
        let Ok(secs) = secs.parse::<i64>() else { continue };
        book.add(Entry {
            agent: "opencode",
            session,
            project: cwd,
            secs,
            model: if model.is_empty() { "opencode" } else { model },
            input: n(input),
            output: n(output),
            read: n(read),
            write: n(write),
            cost: cost.parse::<f64>().unwrap_or(0.),
        });
    }
}

/// A chat file's replies, kept while its size and time stay the same.
type GeminiChat = (u64, Option<SystemTime>, Vec<GeminiReply>);
static GEMINI_CHATS: LazyLock<Mutex<HashMap<PathBuf, GeminiChat>>> = LazyLock::new(Default::default);

/// Gemini CLI's chats under `tmp/<project>/chats` written since `from`.
/// Each project folder names the folder it stands for in `.project_root`,
/// or `projects.json` maps folders to it. Gemini CLI records no cost.
fn gemini_range(root: &Path, from: i64, book: &mut Book) {
    let since = UNIX_EPOCH + Duration::from_secs(from.max(0) as u64);
    let known: HashMap<String, String> = std::fs::read(root.join("projects.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("projects").and_then(Value::as_object).cloned())
        .map(|o| o.into_iter().filter_map(|(path, name)| Some((name.as_str()?.to_string(), path))).collect())
        .unwrap_or_default();
    for project in std::fs::read_dir(root.join("tmp")).into_iter().flatten().flatten() {
        let Ok(chats) = std::fs::read_dir(project.path().join("chats")) else { continue };
        let folder = project.file_name().to_string_lossy().into_owned();
        let cwd = std::fs::read_to_string(project.path().join(".project_root"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| known.get(&folder).cloned())
            .unwrap_or_default();
        for chat in chats.flatten() {
            let path = chat.path();
            let Ok(meta) = chat.metadata() else { continue };
            let modified = meta.modified().ok();
            if !meta.is_file() || modified.is_none_or(|m| m < since) {
                continue;
            }
            let key = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let lock = || GEMINI_CHATS.lock().unwrap_or_else(|e| e.into_inner());
            let cached = lock().get(&path).filter(|c| c.0 == meta.len() && c.1 == modified).map(|c| c.2.clone());
            let replies = match cached {
                Some(r) => r,
                None => {
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    let r: Vec<GeminiReply> = gemini_messages(&text).iter().filter_map(gemini_reply).collect();
                    lock().insert(path.clone(), (meta.len(), modified, r.clone()));
                    r
                }
            };
            gemini_book(&replies, &key, &cwd, from, book);
        }
    }
}

fn gemini_book(replies: &[GeminiReply], session: &str, cwd: &str, from: i64, book: &mut Book) {
    for r in replies.iter().filter(|r| r.secs >= from) {
        book.add(Entry {
            agent: "gemini",
            session,
            project: cwd,
            secs: r.secs,
            model: r.model.as_deref().unwrap_or("gemini"),
            input: r.input,
            output: r.output,
            read: r.cache_read,
            write: 0,
            cost: 0.,
        });
    }
}

// ---------------------------------------------------------------------------
// CSV export

/// Days since 1970-01-01 as (year, month, day).
pub fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// The local date `day` days from today as "YYYY-MM-DD", read at local
/// noon so the zone cannot move it across a date line.
pub fn local_date(day: i64) -> String {
    let (y, m, d) = civil((local_midnight() + day * 86_400 + 43_200).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// A field quoted when it holds a comma, quote or line break.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// One row per day, agent and model, oldest first, `date` naming each day.
pub fn csv(r: &Report, date: impl Fn(i64) -> String) -> String {
    let mut out = String::from("date,agent,model,input,output,cache_read,cache_write,cost_usd\n");
    for (day, m) in &r.rows {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{:.6}\n",
            date(*day),
            m.agent,
            csv_field(&m.model),
            m.input,
            m.output,
            m.cache_read,
            m.cache_write,
            m.cost
        ));
    }
    out
}

/// Writes the report to `~/Downloads/deck-usage-<days>d-<today>.csv`.
pub fn export_csv(r: &Report) -> std::io::Result<PathBuf> {
    let dir = home().ok_or(std::io::ErrorKind::NotFound)?.join("Downloads");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("deck-usage-{}d-{}.csv", r.days, local_date(0)));
    std::fs::write(&path, csv(r, local_date))?;
    Ok(path)
}

#[cfg(test)]
mod range_tests {
    use super::*;

    fn book() -> Book {
        Book { midnight: 1_000 * 86_400, ..Default::default() }
    }

    #[test]
    fn json_str_reads_one_field() {
        assert_eq!(json_str(r#"{"a":1,"cwd":"/Users/me/a \"b\"","x":"y"}"#, "cwd").as_deref(), Some("/Users/me/a \"b\""));
        assert_eq!(json_str(r#"{"cwd":"C:\\dev"}"#, "cwd").as_deref(), Some("C:\\dev"));
        assert_eq!(json_str(r#"{"a":"b"}"#, "cwd"), None);
    }

    #[test]
    fn claude_scans_keep_the_first_cwd() {
        let mut s = Scan::default();
        s.line(r#"{"type":"user","cwd":"/Users/me/deck","message":{"role":"user","content":"hi"}}"#);
        s.line(r#"{"type":"user","cwd":"/Users/me/deck/crates"}"#);
        assert_eq!(s.cwd.as_deref(), Some("/Users/me/deck"));
        assert_eq!(decode_project_dir("-Users-me-deck"), "/Users/me/deck");
    }

    #[test]
    fn book_splits_days_models_and_projects() {
        let mut b = book();
        let e = |session, project, secs, model, cost| Entry {
            agent: "claude",
            session,
            project,
            secs,
            model,
            input: 10,
            output: 5,
            read: 0,
            write: 0,
            cost,
        };
        let today = 1_000 * 86_400 + 3_600;
        b.add(e("s1", "/a/deck", today, "claude-opus-5-5", 1.0));
        b.add(e("s1", "/a/deck", today - 86_400, "claude-opus-5-5", 2.0));
        b.add(e("s2", "/b/deck", today, "claude-sonnet-5", 0.5));
        b.add(e("s3", "", today, "claude-sonnet-5", 0.25));
        let r = b.finish(2);
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0].0, -1);
        assert_eq!(r.rows[0].1.cost, 2.0);
        let names: Vec<(&str, u32)> = r.projects.iter().map(|p| (p.name.as_str(), p.sessions)).collect();
        assert_eq!(names, [("a/deck", 1), ("b/deck", 1), ("Other", 1)]);
        assert_eq!(r.projects[0].cost, 3.0);
        assert_eq!(r.projects[0].tokens, 30);
    }

    #[test]
    fn codex_logs_book_their_cwd() {
        let log = concat!(
            r#"{"timestamp":"2026-11-02T10:00:00Z","type":"session_meta","payload":{"cwd":"/Users/me/deck"}}"#, "\n",
            r#"{"timestamp":"2026-11-02T10:00:01Z","type":"turn_context","payload":{"cwd":"/Users/me/other","model":"gpt-5.5"}}"#, "\n",
            r#"{"timestamp":"2026-11-02T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7}}}}"#, "\n",
        );
        let mut b = Book { midnight: iso_secs("2026-11-02T00:00:00Z").unwrap(), ..Default::default() };
        codex_log(log, "k", 0, &mut b);
        let r = b.finish(1);
        assert_eq!(r.projects.len(), 1);
        assert_eq!(r.projects[0].path, "/Users/me/deck");
        let m = &r.models[0];
        assert_eq!((m.model.as_str(), m.input, m.cache_read, m.output), ("gpt-5.5", 60, 40, 7));
    }

    #[test]
    fn opencode_rows_book_with_their_own_cost() {
        let mut b = Book { midnight: 1_793_600_000, ..Default::default() };
        opencode_book(
            "1793634876\tses_1\tbig-pickle\t9930\t229\t16128\t0\t0\t/Users/me\n\
             1793634900\tses_2\tclaude-sonnet\t100\t50\t0\t10\t0.012\t\n\
             bad line\n",
            &mut b,
        );
        let r = b.finish(1);
        assert_eq!(r.sessions, [("opencode", 2)]);
        let pickle = r.models.iter().find(|m| m.model == "big-pickle").unwrap();
        assert_eq!((pickle.input, pickle.output, pickle.cache_read), (9930, 229, 16128));
        assert!((r.cost() - 0.012).abs() < 1e-9);
        assert!(r.projects.iter().any(|p| p.path == "/Users/me" && p.name == "me"));
    }

    #[test]
    fn gemini_messages_follow_set_and_ids() {
        let text = concat!(
            r#"{"sessionId":"x","projectHash":"h"}"#, "\n",
            r#"{"$set":{"messages":[{"id":"a","type":"gemini","timestamp":"2026-11-02T10:00:00Z","model":"gemini-3-pro","tokens":{"input":100,"output":5,"cached":60,"thoughts":3,"tool":2,"total":110}}]}}"#, "\n",
            r#"{"id":"a","type":"gemini","timestamp":"2026-11-02T10:00:00Z","model":"gemini-3-pro","tokens":{"input":100,"output":5,"cached":60,"thoughts":3,"tool":2,"total":110}}"#, "\n",
            r#"{"id":"b","type":"user","timestamp":"2026-11-02T10:01:00Z"}"#, "\n",
        );
        let msgs = gemini_messages(text);
        assert_eq!(msgs.len(), 2);
        let replies: Vec<GeminiReply> = msgs.iter().filter_map(gemini_reply).collect();
        assert_eq!(replies.len(), 1);
        let r = &replies[0];
        assert_eq!((r.input, r.output, r.cache_read, r.total), (42, 8, 60, 110));
        let mut b = Book { midnight: iso_secs("2026-11-02T00:00:00Z").unwrap(), ..Default::default() };
        gemini_book(&replies, "chat", "/Users/me/deck", 0, &mut b);
        let rep = b.finish(1);
        assert_eq!(rep.tokens(), 110);
        assert_eq!(rep.projects[0].name, "deck");
    }

    #[test]
    fn csv_has_a_row_per_day_agent_model() {
        let mut b = book();
        let e = |agent, model, secs| Entry {
            agent,
            session: "s",
            project: "",
            secs,
            model,
            input: 1,
            output: 2,
            read: 3,
            write: 4,
            cost: 0.5,
        };
        let today = 1_000 * 86_400 + 60;
        b.add(e("claude", "claude-opus-5-5", today));
        b.add(e("claude", "claude-opus-5-5", today));
        b.add(e("codex", "gpt,5", today - 86_400));
        let text = csv(&b.finish(7), |d| format!("D{d}"));
        assert_eq!(
            text,
            "date,agent,model,input,output,cache_read,cache_write,cost_usd\n\
             D-1,codex,\"gpt,5\",1,2,3,4,0.500000\n\
             D0,claude,claude-opus-5-5,2,4,6,8,1.000000\n"
        );
        assert_eq!(civil(20_727), (2026, 10, 1));
    }
}

#[cfg(test)]
mod report_tests {
    /// Against this Mac's own transcripts: `cargo test -- --ignored real_report --nocapture`.
    #[test]
    #[ignore]
    fn real_report() {
        let t = std::time::Instant::now();
        let r = super::report(30);
        println!("{:?} cost {:.2} tokens {} sessions {:?}", t.elapsed(), r.cost(), r.tokens(), r.sessions);
        for m in r.models.iter().take(8) {
            println!("{} {} ${:.2} {} saved ${:.2}", m.agent, m.model, m.cost, m.tokens(), m.saved);
        }
        for p in r.projects.iter().take(8) {
            println!("{} ({}) ${:.2} {} {} sessions", p.name, p.path, p.cost, p.tokens, p.sessions);
        }
        println!("{} csv rows", r.rows.len());
        let t = std::time::Instant::now();
        let _ = super::report(30);
        println!("again {:?}", t.elapsed());
    }
}

// ---------------------------------------------------------------------------
// Plan limits over time

/// The highest each plan window reached on a local day: (day as days since
/// the epoch, agent, window label, highest percent).
#[derive(Clone, Debug, PartialEq, serde::Serialize, Deserialize)]
pub struct LimitDay {
    pub day: i64,
    pub agent: String,
    pub label: String,
    pub peak: f32,
}

fn limits_file() -> PathBuf {
    crate::control::socket_dir().join("limits-history.json")
}

pub fn limit_history() -> Vec<LimitDay> {
    std::fs::read(limits_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Fold the readings just taken into today's peaks. Keeps 120 days.
pub fn record_limits(claude: Option<&Limits>, codex: Option<&Limits>) {
    let today = (local_midnight() + 43_200).div_euclid(86_400);
    let mut all = limit_history();
    let before = all.clone();
    for l in claude.into_iter().chain(codex) {
        for lim in &l.limits {
            match all.iter_mut().find(|d| d.day == today && d.agent == l.agent && d.label == lim.label) {
                Some(d) => d.peak = d.peak.max(lim.used_pct),
                None => all.push(LimitDay { day: today, agent: l.agent.to_string(), label: lim.label.clone(), peak: lim.used_pct }),
            }
        }
    }
    all.retain(|d| today - d.day < 120);
    if all != before {
        if let Ok(b) = serde_json::to_vec(&all) {
            let tmp = limits_file().with_extension("json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(tmp, limits_file());
            }
        }
    }
}
