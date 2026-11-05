//! What a command can do, read from its own `--help`: the subcommands and
//! flags of any CLI, not only the few `complete.rs` knows by heart. The
//! help runs once per binary (and per subcommand it lists), off to the side
//! with a short timeout; until it answers the menu offers what it already
//! had, and from then on the answer comes from memory, or from a cache on
//! disk that the next shell and the next launch share.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Help {
    /// `(name, what it does)`.
    pub subs: Vec<(String, String)>,
    pub flags: Vec<(String, String)>,
}

enum Slot {
    Asking,
    Known(Arc<Help>),
}

static KNOWN: Mutex<Option<HashMap<String, Slot>>> = Mutex::new(None);

/// How long a `--help` may take before it is given up on.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Never run with `--help`: they act, prompt or take over the terminal
/// whatever they are given, or have nothing to list.
const SKIP: &[&str] = &[
    "sudo", "su", "doas", "ssh", "scp", "sftp", "mosh", "telnet", "nc", "say", "shutdown", "reboot", "halt",
    "kill", "killall", "pkill", "rm", "rmdir", "dd", "mkfs", "diskutil", "open", "osascript", "caffeinate",
    "screencapture", "afplay", "yes", "sleep", "watch", "script", "login", "exec", "exit", "logout", "cd",
    "sh", "bash", "zsh", "fish", "vi", "vim", "nvim", "nano", "emacs", "less", "more", "man", "top", "htop",
    "cat", "echo", "true", "false", "time", "env", "nohup", "xargs",
];

/// The subcommands and flags of `path` (a command and the subcommands
/// after it), when known. Asking about one not known yet starts reading
/// its help and answers `None` for now.
pub fn help(path: &[&str]) -> Option<Arc<Help>> {
    let (&cmd, _) = path.split_first()?;
    if SKIP.contains(&cmd) || cmd.contains('/') || path.len() > 3 {
        return None;
    }
    let key = path.join(" ");
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    match known.get(&key) {
        Some(Slot::Known(h)) => return Some(h.clone()),
        Some(Slot::Asking) => return None,
        None => {}
    }
    let bin = which(cmd)?;
    let stamp = stamp(&bin);
    if let Some(h) = read_cache(&key, stamp) {
        let h = Arc::new(h);
        known.insert(key, Slot::Known(h.clone()));
        return Some(h);
    }
    known.insert(key.clone(), Slot::Asking);
    let words: Vec<String> = path.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        let text = run_help(&bin, &words[1..]).unwrap_or_default();
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let h = parse(&text, &refs);
        write_cache(&key, stamp, &h);
        if let Some(known) = KNOWN.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            known.insert(key, Slot::Known(Arc::new(h)));
        }
    });
    None
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(cmd))
        .find(|p| p.is_file())
}

/// The binary's change time: a new version of the tool reads its help again.
fn stamp(bin: &Path) -> u64 {
    std::fs::metadata(bin)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn run_help(bin: &Path, subs: &[String]) -> Option<String> {
    use std::process::{Command, Stdio};
    let child = Command::new(bin)
        .args(subs)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("COLUMNS", "200")
        .env("PAGER", "cat")
        .spawn()
        .ok()?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(out)) => {
            // Some print their help on stderr.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            Some(text)
        }
        _ => {
            let _ = Command::new("/bin/kill").args(["-9", &pid.to_string()]).status();
            None
        }
    }
}

fn cache_file(key: &str) -> Option<PathBuf> {
    let name: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/Caches/deck/help")
            .join(format!("{name}.json")),
    )
}

#[derive(Serialize, Deserialize)]
struct Cached {
    stamp: u64,
    help: Help,
}

fn read_cache(key: &str, stamp: u64) -> Option<Help> {
    let c: Cached = serde_json::from_slice(&std::fs::read(cache_file(key)?).ok()?).ok()?;
    (c.stamp == stamp).then_some(c.help)
}

fn write_cache(key: &str, stamp: u64, help: &Help) {
    let Some(file) = cache_file(key) else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&Cached { stamp, help: help.clone() }) {
        let _ = std::fs::write(file, bytes);
    }
}

/// Read the subcommands and flags out of help text in the usual shapes:
/// commander and yargs ("Commands:", each line led by the command's name),
/// cobra ("Available Commands:", gh's "CORE COMMANDS"), clap ("Commands:",
/// "SUBCOMMANDS:"), click. Flags are any indented line led by a dash.
pub fn parse(text: &str, path: &[&str]) -> Help {
    let mut help = Help::default();
    let mut in_cmds = false;
    // The indent of a section's entries: deeper lines continue a description.
    let mut indent: Option<usize> = None;
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        if depth == 0 {
            let header = trimmed.trim_end_matches(':').to_ascii_lowercase();
            let looks = trimmed.ends_with(':') || trimmed == trimmed.to_uppercase();
            in_cmds = looks && header.contains("command") && !header.contains("usage");
            indent = None;
            continue;
        }
        let (lhs, desc) = columns(trimmed);
        let desc = clip(desc);
        if trimmed.starts_with('-') {
            for flag in flag_words(lhs) {
                if !help.flags.iter().any(|f| f.0 == flag) {
                    help.flags.push((flag, desc.clone()));
                }
            }
            continue;
        }
        if !in_cmds || indent.is_some_and(|i| depth > i) {
            continue;
        }
        indent.get_or_insert(depth);
        let mut words = lhs.split_whitespace().peekable();
        for p in path {
            if words.peek() == Some(p) {
                words.next();
            }
        }
        let Some(first) = words.next() else { continue };
        // "auth:", "list, ls", "stash|st".
        let name = first.trim_end_matches([':', ',']).split('|').next().unwrap_or("");
        if valid(name) && !help.subs.iter().any(|s| s.0 == name) {
            help.subs.push((name.to_string(), desc));
        }
    }
    help
}

/// A command's name and its description, split at the first run of two
/// or more spaces (or a tab).
fn columns(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'\t' || (bytes[i] == b' ' && bytes[i + 1] == b' ') {
            return (line[..i].trim(), line[i..].trim());
        }
    }
    (line, "")
}

fn flag_words(lhs: &str) -> Vec<String> {
    lhs.split([',', ' ', '=', '[', '<'])
        .filter_map(|w| {
            let w = w.trim();
            let long = w.strip_prefix("--").is_some_and(|r| r.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()));
            let short = w.len() == 2 && w.starts_with('-') && w.as_bytes()[1].is_ascii_alphanumeric();
            (long || short).then(|| w.to_string())
        })
        .collect()
}

fn valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "-_:.".contains(c))
        && name.len() <= 32
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(59).collect::<String>())
    } else {
        s.to_string()
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yargs_lines_drop_the_command_and_placeholders() {
        let text = "opencode [project]\n\nCommands:\n  opencode completion          generate shell completion script\n  opencode [project]           start opencode tui\n  opencode run [message..]     run opencode with a message\n\nOptions:\n  -h, --help     show help  [boolean]\n  -m, --model    model to use  [string]\n";
        let h = parse(text, &["opencode"]);
        let subs: Vec<&str> = h.subs.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(subs, ["completion", "run"]);
        assert_eq!(h.subs[1].1, "run opencode with a message");
        let flags: Vec<&str> = h.flags.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(flags, ["-h", "--help", "-m", "--model"]);
    }

    #[test]
    fn cobra_and_gh_sections_and_wrapped_descriptions() {
        let text = "Usage:\n  gh <command>\n\nCORE COMMANDS\n  auth:          Authenticate gh and git\n  pr:            Manage pull requests\n                 that wrap\n\nFLAGS\n  --version   Show gh version\n";
        let h = parse(text, &["gh"]);
        let subs: Vec<&str> = h.subs.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(subs, ["auth", "pr"]);
        assert_eq!(h.flags, vec![("--version".to_string(), "Show gh version".to_string())]);
    }

    #[test]
    fn commander_lists_with_options_after_the_name() {
        let text = "Commands:\n  agents [options]          Manage background agents\n  attach <id|name>          Open a background session\n";
        let h = parse(text, &["claude"]);
        let subs: Vec<&str> = h.subs.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(subs, ["agents", "attach"]);
    }
}
