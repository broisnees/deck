//! When an agent's turn dies on something a retry can fix (an API or
//! network error, an overloaded server), deck says "continue" after a wait
//! that doubles each time: 5 s, 10 s, 20 s… up to five minutes, eight tries.
//! A usage limit waits until it resets. A login or billing problem is shown
//! to you instead, since no retry can fix it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Tries in a row before deck gives up and leaves it to you.
pub const MAX_TRIES: u32 = 8;

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The wait before try `n` (from 1): 5 s, doubling, at most five minutes.
pub fn backoff(n: u32) -> Duration {
    let mut d = Duration::from_secs(5);
    for _ in 1..n {
        if d >= Duration::from_secs(300) {
            break;
        }
        d *= 2;
    }
    d.min(Duration::from_secs(300))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Worth another go: after `wait` if given (a usage limit's reset),
    /// else after the backoff.
    Retry {
        wait: Option<Duration>,
        reason: String,
    },
    /// Only you can fix this.
    Blocked(String),
}

/// Fixes that are yours to make, by what the error says.
const BLOCKED: &[(&str, &str)] = &[
    ("authentication", "login required: run /login"),
    ("invalid api key", "the API key is not valid"),
    ("oauth", "login required: run /login"),
    ("/login", "login required: run /login"),
    ("not logged in", "login required: run /login"),
    (
        "credit balance",
        "billing problem: the credit balance is too low",
    ),
    ("billing", "billing problem"),
    ("account on hold", "account on hold"),
    ("verification required", "account verification required"),
    ("model not found", "model not found"),
    ("not_found_error", "model not found"),
    ("invalid_request", "the API rejected the request"),
    ("permission_error", "the API refused: permission"),
    // An agent missing on a remote host: a retry finds it missing again.
    (
        "is not installed on",
        "the agent is not installed on that host; Claude, Codex and OpenCode work there without installing anything",
    ),
    ("no such file or directory", "a program it needs is missing"),
    ("command not found", "a program it needs is missing"),
];

/// Errors that pass: overloads, server errors, dropped connections.
const TRANSIENT: &[&str] = &[
    "overloaded",
    "429",
    "500",
    "502",
    "503",
    "504",
    "529",
    "connection",
    "timed out",
    "timeout",
    "network",
    "econnreset",
    "econnrefused",
    "fetch failed",
    "socket",
    "stream error",
    "stream disconnected",
    "internal server error",
    "temporarily unavailable",
    "server_error",
    "api_error",
    "try again",
    "no response",
    "rate limit",
    "rate_limit",
];

/// Decide about a failed turn from the API's HTTP status when the agent
/// reported one, else from what it said. `clock` is the local time now as
/// (hour, minute, second), for a limit that resets at "3pm".
pub fn classify(
    status: Option<u16>,
    text: &str,
    now_epoch: u64,
    clock: (u32, u32, u32),
) -> Verdict {
    let lower = text.to_lowercase();
    if lower.contains("limit")
        && (lower.contains("usage") || lower.contains("reached") || lower.contains("resets"))
    {
        // Claude Code says "usage limit reached|<epoch>" or "… resets 3pm".
        if let Some(epoch) = text
            .rsplit('|')
            .next()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .filter(|e| *e > now_epoch)
        {
            return Verdict::Retry {
                wait: Some(Duration::from_secs(epoch - now_epoch + 30)),
                reason: "usage limit".into(),
            };
        }
        if let Some(wait) = until_clock(&lower, clock) {
            return Verdict::Retry {
                wait: Some(wait + Duration::from_secs(30)),
                reason: "usage limit".into(),
            };
        }
    }
    // A usage limit also answers 429, so it is read from the text above;
    // past that the status says it exactly.
    if let Some(v) = status.and_then(by_status) {
        return v;
    }
    for (needle, why) in BLOCKED {
        if lower.contains(needle) {
            return Verdict::Blocked((*why).into());
        }
    }
    if TRANSIENT.iter().any(|t| lower.contains(t)) || lower.contains("api error") {
        let reason = if lower.contains("overloaded") || lower.contains("529") {
            "API overloaded"
        } else if lower.contains("rate") || lower.contains("429") {
            "rate limited"
        } else {
            "API or network error"
        };
        return Verdict::Retry {
            wait: None,
            reason: reason.into(),
        };
    }
    Verdict::Blocked(clip(text.trim(), 80))
}

/// What an HTTP status means for a retry, for the ones that settle it.
fn by_status(status: u16) -> Option<Verdict> {
    let retry = |reason: &str| Verdict::Retry {
        wait: None,
        reason: reason.into(),
    };
    Some(match status {
        429 => retry("rate limited"),
        529 => retry("API overloaded"),
        408 | 500 | 502 | 503 | 504 => retry("API or network error"),
        401 => Verdict::Blocked("login required: run /login".into()),
        402 => Verdict::Blocked("billing problem".into()),
        403 => Verdict::Blocked("the API refused: permission".into()),
        404 => Verdict::Blocked("model not found".into()),
        413 => Verdict::Blocked("the conversation is too long for the model: run /compact".into()),
        400 => Verdict::Blocked("the API rejected the request".into()),
        _ => return None,
    })
}

/// "resets 3pm", "resets 11:30 am": the wait until that time next comes.
fn until_clock(lower: &str, (h, m, s): (u32, u32, u32)) -> Option<Duration> {
    let at = lower.find("resets")?;
    let rest = lower[at + "resets".len()..].trim_start();
    let rest = rest.strip_prefix("at ").unwrap_or(rest);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let hour: u32 = digits.parse().ok()?;
    let mut after = &rest[digits.len()..];
    let mut minute = 0;
    if let Some(r) = after.strip_prefix(':') {
        let mm: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
        minute = mm.parse().ok()?;
        after = &r[mm.len()..];
    }
    let after = after.trim_start();
    let hour = match () {
        _ if after.starts_with("pm") => hour % 12 + 12,
        _ if after.starts_with("am") => hour % 12,
        _ => return None,
    };
    let now = h * 3600 + m * 60 + s;
    let then = hour * 3600 + minute * 60;
    let wait = if then > now {
        then - now
    } else {
        then + 86_400 - now
    };
    Some(Duration::from_secs(wait as u64))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

/// The local wall clock: seconds since the epoch, and (hour, minute, second).
pub fn now() -> (u64, (u32, u32, u32)) {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        _mday: i32,
        _mon: i32,
        _year: i32,
        _wday: i32,
        _yday: i32,
        _isdst: i32,
        // Windows' C runtime ends the struct here.
        #[cfg(unix)]
        _gmtoff: std::ffi::c_long,
        #[cfg(unix)]
        _zone: *const std::ffi::c_char,
    }
    #[cfg(unix)]
    extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    // The same, as Windows' C runtime spells it: 0 on success.
    #[cfg(windows)]
    extern "C" {
        fn _localtime64_s(out: *mut Tm, t: *const i64) -> i32;
    }
    #[cfg(windows)]
    unsafe fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm {
        // SAFETY: as localtime_r; both pointers are the caller's.
        if unsafe { _localtime64_s(out, t) } == 0 {
            out
        } else {
            std::ptr::null_mut()
        }
    }
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = epoch as i64;
    // SAFETY: localtime_r fills the struct it is given and keeps nothing.
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !localtime_r(&t, &mut tm).is_null() };
    let clock = if ok {
        (tm.hour as u32, tm.min as u32, tm.sec as u32)
    } else {
        (
            (epoch / 3600 % 24) as u32,
            (epoch / 60 % 60) as u32,
            (epoch % 60) as u32,
        )
    };
    (epoch, clock)
}

/// Claude Code waiting out its own API retries ("Retrying in 18s · attempt
/// 4/10"), or a request that got no answer, on the status lines just above
/// its input box. Only those lines count, so the same words in a log or a
/// file on screen do not.
pub fn stall_line(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    // The input box is drawn between two long horizontal rules.
    let (top, _) = input_box(&lines)?;
    lines[..top]
        .iter()
        .rev()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(3)
        .find(|l| {
            (l.contains("Retrying in") && l.contains("attempt"))
                || l.contains("No response from the API after")
        })
        .map(str::to_string)
}

/// The two long rules Claude Code draws its input box between, as line
/// indices (top, bottom), from the bottom of the screen.
fn input_box(lines: &[&str]) -> Option<(usize, usize)> {
    let mut found = Vec::new();
    for (i, l) in lines.iter().enumerate().rev() {
        let t = l.trim();
        if t.chars().count() >= 20 && t.chars().all(|c| c == '─') {
            found.push(i);
            if found.len() == 2 {
                return Some((found[1], found[0]));
            }
        }
    }
    None
}

/// Whether Claude Code's input box holds text (after Esc it puts back the
/// message it was working on): then Enter resends it, rather than typing
/// "continue" onto the end of it.
pub fn input_has_text(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let Some((top, bottom)) = input_box(&lines) else {
        return false;
    };
    lines[top + 1..bottom].iter().any(|l| {
        let t = l.trim().trim_start_matches(['│', '>', '❯']).trim();
        !t.is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_to_five_minutes() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(4), Duration::from_secs(40));
        assert_eq!(backoff(8), Duration::from_secs(300));
        assert_eq!(backoff(30), Duration::from_secs(300));
    }

    #[test]
    fn errors_sort_into_retry_and_yours() {
        let at = (14, 0, 0);
        let r = |t| classify(None, t, 1_000, at);
        assert!(matches!(
            r("API Error: 529 {\"type\":\"overloaded_error\"}"),
            Verdict::Retry { wait: None, .. }
        ));
        assert!(matches!(r("Request timed out"), Verdict::Retry { .. }));
        assert!(matches!(r("Connection error."), Verdict::Retry { .. }));
        assert!(matches!(
            r("Invalid API key · Please run /login"),
            Verdict::Blocked(_)
        ));
        assert!(matches!(
            r("Credit balance is too low"),
            Verdict::Blocked(_)
        ));
        assert_eq!(
            r("Claude AI usage limit reached|1600"),
            Verdict::Retry {
                wait: Some(Duration::from_secs(630)),
                reason: "usage limit".into()
            }
        );
        // 2pm now, resets 3pm: an hour (and half a minute to be sure).
        assert_eq!(
            r("5-hour limit reached ∙ resets 3pm"),
            Verdict::Retry {
                wait: Some(Duration::from_secs(3630)),
                reason: "usage limit".into()
            }
        );
        // 11:30am already passed: tomorrow's.
        assert!(matches!(
            r("usage limit reached, resets 11:30 am"),
            Verdict::Retry { wait: Some(w), .. } if w > Duration::from_secs(20 * 3600)
        ));
    }

    #[test]
    fn the_status_decides_when_there_is_one() {
        let at = (14, 0, 0);
        // Words that would read as transient, but the status says login.
        assert!(matches!(
            classify(Some(401), "connection reset", 1_000, at),
            Verdict::Blocked(_)
        ));
        assert!(matches!(
            classify(Some(529), "something odd", 1_000, at),
            Verdict::Retry { wait: None, .. }
        ));
        // A usage limit comes as 429 too, and still waits for its reset.
        assert!(matches!(
            classify(Some(429), "Claude AI usage limit reached|1600", 1_000, at),
            Verdict::Retry { wait: Some(_), .. }
        ));
        // A status it has no rule for falls back to the words.
        assert!(matches!(
            classify(Some(418), "Request timed out", 1_000, at),
            Verdict::Retry { .. }
        ));
    }

    #[test]
    fn a_stall_is_read_only_above_the_input_box() {
        let rule = "─".repeat(40);
        let screen = format!(
            "⏺ working on it\n  ⎿ API Error (overloaded) · Retrying in 18s · attempt 4/10\n\n{rule}\n> \n{rule}\n  ? for shortcuts"
        );
        assert!(stall_line(&screen).unwrap().contains("attempt 4/10"));
        let quoted = format!(
            "Retrying in 18s · attempt 4/10 (in a log)\nmore\nmore\nmore\n{rule}\n> \n{rule}"
        );
        assert_eq!(stall_line(&quoted), None);
        assert_eq!(stall_line("no box at all"), None);
        assert!(!input_has_text(&format!("x\n{rule}\n> \n{rule}")));
        assert!(input_has_text(&format!("x\n{rule}\n> fix the bug\n{rule}")));
    }
}
