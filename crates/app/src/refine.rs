//! Prompt rewriting: a quick Claude run (Haiku by default) turns what you
//! typed or dictated into the clear, complete prompt you would have written
//! with time to think, using the agent's current task and last reply to work
//! out what "it" and "that" mean.
//!
//! It runs `claude -p` with no tools, no settings and no saved session, so
//! none of your hooks fire and it shows up nowhere as an agent.
//!
//! Each of Deck's AI jobs (this one, and writing shell commands) can run on
//! a local model instead, as Settings › Models routes it. A local model that
//! cannot answer hands the job back to Claude, and the reason is kept for
//! Settings to show.

use crate::local::Route;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Enter rewrites and sends the rewrite.
    #[default]
    Auto,
    /// Enter puts the rewrite in the box; Enter again sends it.
    Review,
    /// Nothing rewrites, not Enter and not ⌃O.
    Off,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Auto, Mode::Review, Mode::Off];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "Rewrite and send",
            Mode::Review => "Show first",
            Mode::Off => "Off",
        }
    }
}

static MODE: std::sync::Mutex<(Mode, String)> = std::sync::Mutex::new((Mode::Auto, String::new()));

/// What Settings chose: when to rewrite, and with which model.
pub fn set(mode: Mode, model: &str) {
    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = (mode, model.to_string());
}

/// Off too when the rewrite job's route is `off` (a hand-edited file), so
/// chats send as typed instead of failing every rewrite.
pub fn mode() -> Mode {
    let off = lock(&ROUTES).get(REWRITE).is_some_and(|r| r == "off");
    if off {
        return Mode::Off;
    }
    MODE.lock().unwrap_or_else(|e| e.into_inner()).0
}

pub fn model() -> String {
    let m = MODE.lock().unwrap_or_else(|e| e.into_inner()).1.clone();
    if m.is_empty() {
        "haiku".into()
    } else {
        m
    }
}

/// The models it may use, by Claude Code's own aliases.
pub const MODELS: [(&str, &str); 3] = [("haiku", "Haiku"), ("sonnet", "Sonnet"), ("opus", "Opus")];

// -- routing -------------------------------------------------------------------

pub const REWRITE: &str = "rewrite";
pub const SHELL: &str = "shell";

/// Deck's own AI jobs: the id `Settings::routes` keys them by, the name
/// Settings shows, and what it does.
pub const JOBS: [(&str, &str, &str); 2] = [
    (
        REWRITE,
        "Rewrite prompts",
        "Turns what you type in a chat into a clear prompt before it goes.",
    ),
    (
        SHELL,
        "Write shell commands",
        "Writes the command for a # request at a zsh prompt.",
    ),
];

static ROUTES: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Why each job last fell back to Claude, and when.
static FALLBACKS: Mutex<BTreeMap<String, (String, Instant)>> = Mutex::new(BTreeMap::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The routes Settings chose. Loading or saving settings sets them, so the
/// jobs follow every change without anyone passing them on.
pub fn set_routes(routes: &BTreeMap<String, String>) {
    *lock(&ROUTES) = routes.clone();
}

/// Where `job` runs: its route, else Claude with `model` (the rewrite
/// model), as before there were routes.
fn route(job: &str, model: &str) -> Route {
    lock(&ROUTES)
        .get(job)
        .and_then(|r| Route::parse(r))
        .unwrap_or_else(|| Route::Claude(model.to_string()))
}

/// What does the rewriting now, for the line under the composer.
pub fn rewriter() -> String {
    route(REWRITE, &model()).label()
}

/// Why `job` last fell back to Claude, and how long ago. Cleared once its
/// local model answers again.
pub fn fallback(job: &str) -> Option<(String, Duration)> {
    lock(&FALLBACKS)
        .get(job)
        .map(|(why, at)| (why.clone(), at.elapsed()))
}

/// The route Settings shows for `job`. Rewriting keeps its on/off in
/// `refine` and its Claude model in `refine_model`, as the Agents page sets
/// them; only a local model needs a route.
pub fn chosen(s: &Settings, job: &str) -> Route {
    if job == REWRITE && s.refine == Mode::Off {
        return Route::Off;
    }
    s.routes
        .get(job)
        .and_then(|r| Route::parse(r))
        .unwrap_or_else(|| Route::Claude(s.refine_model.clone()))
}

/// Make `job` run on `route`, the way `chosen` reads it back.
pub fn choose(s: &mut Settings, job: &str, route: Route) {
    if job != REWRITE {
        s.routes.insert(job.to_string(), route.to_string());
        return;
    }
    match route {
        Route::Off => {
            s.refine = Mode::Off;
            s.routes.remove(REWRITE);
            return;
        }
        Route::Claude(m) => {
            s.refine_model = m;
            s.routes.remove(REWRITE);
        }
        local => {
            s.routes.insert(REWRITE.to_string(), local.to_string());
        }
    }
    if s.refine == Mode::Off {
        s.refine = Mode::default();
    }
}

/// One answer for `job`: from its local model when it has one and that
/// answers, else from `claude -p` with `model`.
fn answer(
    job: &str,
    system: &str,
    input: &str,
    model: &str,
    cwd: &Path,
    wait: Duration,
) -> Result<String, String> {
    match route(job, model) {
        Route::Off => Err("it is turned off in Settings › Models".into()),
        Route::Claude(m) => ask(system, input, &m, cwd, wait),
        local => match crate::local::chat(&local, system, input) {
            Ok(text) => {
                lock(&FALLBACKS).remove(job);
                Ok(text)
            }
            Err(why) => {
                tracing::warn!("{job}: {} failed, using Claude: {why}", local.label());
                lock(&FALLBACKS).insert(job.to_string(), (why, Instant::now()));
                ask(system, input, model, cwd, wait)
            }
        },
    }
}

/// A tiny prompt through `route`, with no fallback: the reply's first line
/// and how long it took.
pub fn test(route: &Route) -> Result<(String, Duration), String> {
    const SYSTEM: &str = "Reply with one short word.";
    const INPUT: &str = "Say ok.";
    let started = Instant::now();
    let reply = match route {
        Route::Off => return Err("it is off".into()),
        Route::Claude(m) => {
            let home = agent_core::dirs::home_dir().unwrap_or_default();
            ask(SYSTEM, INPUT, m, &home, Duration::from_secs(60))?
        }
        local => crate::local::chat(local, SYSTEM, INPUT)?,
    };
    let took = started.elapsed();
    let first = clean(&reply).lines().next().unwrap_or("").to_string();
    Ok((clip(&first, 40), took))
}

/// It may only restate what you said or what the context holds: a confident
/// rewrite that invents a cause or a file sends the agent the wrong way,
/// which is worse than a vague message.
const SYSTEM: &str = "I type or dictate quick, rough messages to a coding agent working in deck, my workspace of terminals, a browser and agents. Rewrite my message into the prompt I would have written with time to think: clear, specific and complete, so the agent gets it right the first time.

Rules:
- Keep my intent, scope and every concrete detail (names, paths, numbers, error text, URLs, code). Never add requirements, files, causes, libraries or other technical facts that are not in my message or the context. Where something is unknown, tell the agent to find out (read the code, check, ask me) instead of guessing.
- Fix speech to text mistakes, typos and grammar, using the context to work out what I meant. Write in English.
- Write as me, to the agent: first person, imperative (\"Fix\", \"Check\", \"Don't\"). Plain ASCII punctuation, no em dashes.
- Size it to the task. A reply or follow-up in an ongoing conversation (\"yes do that\", \"try again\", an answer to its question) keeps its length: fix the wording and resolve references, add nothing, since the agent already has the whole conversation. \"yes go ahead and commit it\" becomes \"Yes, go ahead and commit it.\" A real task gets the goal, the context that matters, what to do and what to leave alone, and how to tell it is done (what to run or check, what to report back). Use a numbered list only for real steps.
- Use the agent's current task and last reply to resolve \"it\", \"that\" and \"the thing\". Don't repeat what it already knows beyond what the message needs.
- Keep @file mentions exactly as written, where they fit in the text.
- Don't do the task, answer it, or ask me anything. Output only the rewritten prompt, with no preamble, quotes or code fences around it.";

/// What the agent is in the middle of, to resolve "it" and "that" with.
pub struct Context {
    pub agent: String,
    pub folder: String,
    pub task: String,
    pub last_reply: String,
    /// The conversation so far in brief: each earlier message of yours and
    /// the start of the answer to it, oldest first.
    pub history: String,
}

/// Whether Enter rewrites `text` before it goes, and if so whether the
/// rewrite then sends itself.
pub fn on_enter(mode: Mode, text: &str) -> Option<bool> {
    (mode != Mode::Off && needed(text)).then_some(mode == Mode::Auto)
}

/// Whether a message is worth rewriting: not a slash command or a shell
/// escape, and more than a short reply like "yes do it".
pub fn needed(text: &str) -> bool {
    let (_, body) = split_mentions(text);
    if body.is_empty() || body.starts_with('/') || body.starts_with('!') {
        return false;
    }
    body.contains('\n') || body.split_whitespace().count() > 4
}

/// Leading `@file` mentions ride along untouched.
fn split_mentions(text: &str) -> (String, String) {
    let text = text.trim();
    let mut mentions = String::new();
    let mut rest = text;
    while let Some(m) = rest.strip_prefix('@') {
        let end = m.find(char::is_whitespace).unwrap_or(m.len());
        mentions.push('@');
        mentions.push_str(&m[..end]);
        mentions.push(' ');
        rest = m[end..].trim_start();
    }
    (mentions, rest.trim().to_string())
}

fn input(body: &str, ctx: &Context) -> String {
    let mut b = String::from("<agent>\n");
    b.push_str(&format!("name: {}\nfolder: {}\n", ctx.agent, ctx.folder));
    if !ctx.task.is_empty() {
        b.push_str(&format!("current task: {}\n", clip(&ctx.task, 1200)));
    }
    if !ctx.history.is_empty() {
        b.push_str(&format!("the conversation so far (in brief):\n{}\n", tail(&ctx.history, 1500)));
    }
    if !ctx.last_reply.is_empty() {
        b.push_str(&format!(
            "its last reply (end):\n{}\n",
            tail(&ctx.last_reply, 3000)
        ));
    }
    b.push_str("</agent>\n\n");
    b.push_str(&format!("<message>\n{body}\n</message>"));
    b
}

/// Rewrite `text` on the rewrite job's route; `model` is the Claude model
/// for when that is Claude or the local model can't answer. Blocking; run
/// it off the main thread.
pub fn rewrite(text: &str, ctx: &Context, model: &str, cwd: &Path) -> Result<String, String> {
    let (mentions, body) = split_mentions(text);
    if body.is_empty() {
        return Ok(text.to_string());
    }
    let out = answer(
        REWRITE,
        SYSTEM,
        &input(&body, ctx),
        model,
        cwd,
        Duration::from_secs(90),
    )
    .map_err(|e| format!("rewrite failed: {e}"))?;
    let refined = clean(&out);
    if refined.is_empty() {
        return Err("rewrite came back empty".into());
    }
    Ok(format!("{mentions}{refined}"))
}

#[cfg(target_os = "macos")]
const SHELL_SYSTEM: &str = "You turn a request into one command for zsh on macOS. \
Reply with the command only: no explanation, no backticks, no comments. Use the \
tools a Mac has (BSD versions of find, sed, stat, date) unless the request names \
others. When it takes more than one step, chain them with && or a pipe. Never \
write something destructive (rm -rf, overwriting files, force pushes) unless the \
request asks for exactly that.";

#[cfg(windows)]
const SHELL_SYSTEM: &str = "You turn a request into one command for PowerShell on Windows. \
Reply with the command only: no explanation, no backticks, no comments. Use \
PowerShell cmdlets and their common aliases (Get-ChildItem, Select-String, \
Get-Content) rather than Unix tools, and Windows paths, unless the request names \
others. When it takes more than one step, chain them with ; or a pipe (Windows \
PowerShell 5.1 has no &&). Never write something destructive (Remove-Item -Recurse \
-Force, overwriting files, force pushes) unless the request asks for exactly that.";

#[cfg(not(any(target_os = "macos", windows)))]
const SHELL_SYSTEM: &str = "You turn a request into one command for the user's shell on Linux. \
Reply with the command only: no explanation, no backticks, no comments. Use the \
GNU versions of find, sed, stat and date unless the request names others. When it \
takes more than one step, chain them with && or a pipe. Never write something \
destructive (rm -rf, overwriting files, force pushes) unless the request asks for \
exactly that.";

/// A shell command for a request in plain words ("# …" at a prompt), on
/// the shell job's route (Claude with `model` unless Settings says
/// otherwise). Blocking; run it off the main thread.
pub fn shell_command(request: &str, cwd: &Path, model: &str) -> Result<String, String> {
    let input = format!("Folder: {}\nRequest: {request}", cwd.display());
    let out = answer(
        SHELL,
        SHELL_SYSTEM,
        &input,
        model,
        cwd,
        Duration::from_secs(60),
    )?;
    // One line is one command; a model that explains anyway gets its
    // first line taken.
    let cmd = clean(&out)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    if cmd.is_empty() {
        return Err("no command came back".into());
    }
    Ok(cmd)
}

/// One answer from `claude -p`, with no tools and no session, within `wait`.
fn ask(
    system: &str,
    input: &str,
    model: &str,
    cwd: &Path,
    wait: Duration,
) -> Result<String, String> {
    let (program, first) = agent_core::exec::launcher("claude");
    let mut cmd = Command::new(program);
    crate::os::no_window(&mut cmd);
    cmd.args(first);
    cmd.args([
        "-p",
        "--model",
        model,
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--no-session-persistence",
        "--output-format",
        "stream-json",
        "--verbose",
        "--system-prompt",
        system,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if cwd.is_dir() {
        cmd.current_dir(cwd);
    }
    // Keep it out of any agent session and out of deck's control plane.
    for (k, _) in std::env::vars() {
        if (k.starts_with("CLAUDE") && k != "CLAUDE_CONFIG_DIR") || k.starts_with("DECK_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let stdout = child.stdout.take().ok_or("no answer")?;
    // The result comes about a second before claude exits: read it off the
    // stream and stop the process instead of waiting for its shutdown.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("result") {
                    let _ = tx.send(v);
                    return;
                }
            }
        }
    });
    let found = rx.recv_timeout(wait);
    let _ = child.kill();
    let _ = child.wait();
    let v = found.map_err(|_| "it took too long".to_string())?;
    let result = v
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_string();
    if v.get("is_error").and_then(|e| e.as_bool()) == Some(true) {
        return Err(clip(&result, 120));
    }
    Ok(result)
}

/// Drop a code fence or quotes the model wrapped the prompt in.
fn clean(s: &str) -> String {
    let mut s = s.trim().to_string();
    if s.starts_with("```") && s.ends_with("```") && s.len() > 6 {
        let inner = s[3..s.len() - 3].trim().to_string();
        s = match inner.split_once('\n') {
            // The fence's language tag.
            Some((first, rest)) if !first.contains(' ') => rest.trim().to_string(),
            _ => inner,
        };
    }
    if s.len() > 1 && s.starts_with('"') && s.ends_with('"') && !s[1..s.len() - 1].contains('"') {
        s = s[1..s.len() - 1].to_string();
    }
    s.trim().to_string()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The last `n` bytes of `s`, cut at a line start when one is close.
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut at = s.len() - n;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    let t = &s[at..];
    match t.find('\n') {
        Some(i) if i < 200 => t[i + 1..].to_string(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_replies_and_commands_go_as_typed() {
        assert!(!needed("yes do it"));
        assert!(!needed("/compact"));
        assert!(!needed("! ls -la and more words here"));
        assert!(!needed("@src/a.rs fix it"));
        assert!(needed("can you make the sidebar rows line up better"));
        assert!(needed("fix\nthis"));
    }

    #[test]
    fn routes_read_back_as_they_were_chosen() {
        use crate::local::Kind;
        let mut s = Settings::default();
        assert_eq!(chosen(&s, REWRITE), Route::Claude("haiku".into()));
        assert_eq!(chosen(&s, SHELL), Route::Claude("haiku".into()));
        let local = Route::Local(Kind::Ollama, "llama3.2:3b".into());
        choose(&mut s, REWRITE, local.clone());
        assert_eq!(s.routes["rewrite"], "ollama:llama3.2:3b");
        assert_eq!(chosen(&s, REWRITE), local);
        // Off and Claude are the Agents page's own settings.
        choose(&mut s, REWRITE, Route::Off);
        assert_eq!((s.refine, s.routes.is_empty()), (Mode::Off, true));
        assert_eq!(chosen(&s, REWRITE), Route::Off);
        choose(&mut s, REWRITE, Route::Claude("sonnet".into()));
        assert_eq!((s.refine, s.refine_model.as_str()), (Mode::Auto, "sonnet"));
        assert_eq!(chosen(&s, REWRITE), Route::Claude("sonnet".into()));
        choose(&mut s, SHELL, Route::Off);
        assert_eq!(s.routes["shell"], "off");
        assert_eq!(chosen(&s, SHELL), Route::Off);
        assert_eq!(chosen(&s, REWRITE), Route::Claude("sonnet".into()));

        set_routes(&[("shell".to_string(), "lmstudio:qwen".to_string())].into());
        assert_eq!(
            route(SHELL, "haiku"),
            Route::Local(Kind::LmStudio, "qwen".into())
        );
        assert_eq!(route(REWRITE, "opus"), Route::Claude("opus".into()));
        set_routes(&Default::default());
        assert_eq!(route(SHELL, "haiku"), Route::Claude("haiku".into()));
    }

    #[test]
    fn off_rewrites_nothing_on_enter() {
        let long = "can you make the sidebar rows line up better";
        assert_eq!(on_enter(Mode::Auto, long), Some(true));
        assert_eq!(on_enter(Mode::Review, long), Some(false));
        assert_eq!(on_enter(Mode::Off, long), None);
        assert_eq!(on_enter(Mode::Off, "fix\nthis"), None);
        // Short replies go as typed whatever the mode.
        assert_eq!(on_enter(Mode::Auto, "yes do it"), None);
    }

    #[test]
    fn mentions_ride_along_and_wrapping_comes_off() {
        assert_eq!(
            split_mentions("@a.rs @b.rs make it faster"),
            ("@a.rs @b.rs ".to_string(), "make it faster".to_string())
        );
        assert_eq!(clean("```text\nFix the bug.\n```"), "Fix the bug.");
        assert_eq!(clean("\"Fix the bug.\""), "Fix the bug.");
        assert_eq!(clean("  Fix \"this\" one. "), "Fix \"this\" one.");
        assert_eq!(tail("abc\ndef", 100), "abc\ndef");
    }
}
