//! Finding and stopping the programs Deck starts, the same way on every
//! platform.
//!
//! On Windows a bare name is not enough: std's `Command::new("claude")`
//! only tries `claude.exe`, but npm installs `claude.cmd` (and `.ps1`)
//! shims. [`resolve`] finds the real file through `PATHEXT`, and std runs a
//! `.cmd`/`.bat` path through `cmd.exe` with its arguments escaped.

use std::path::{Path, PathBuf};

/// Process creation flags for a child Deck talks to through pipes: its own
/// process group (so a stop reaches what it started) and no console window.
#[cfg(windows)]
pub const CREATION_FLAGS: u32 = 0x0000_0200 /* CREATE_NEW_PROCESS_GROUP */ | 0x0800_0000 /* CREATE_NO_WINDOW */;

/// No console window for a short helper (git, taskkill) started from the
/// windowed app.
#[cfg(windows)]
pub const NO_WINDOW: u32 = 0x0800_0000;

/// The extensions a bare name may carry, from `PATHEXT`, lowercase, dot
/// first. Windows' default when it is unset.
fn path_exts(pathext: Option<&str>) -> Vec<String> {
    pathext
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.starts_with('.') && e.len() > 1)
        .collect()
}

/// The candidates for `bin` in `dir`: the name as given when it has an
/// extension or this is not Windows, then each `PATHEXT` one.
fn candidates(dir: &Path, bin: &str, windows: bool, exts: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let has_ext = Path::new(bin).extension().is_some();
    if !windows || has_ext {
        out.push(dir.join(bin));
    }
    if windows {
        out.extend(exts.iter().map(|e| dir.join(format!("{bin}{e}"))));
    }
    out
}

/// `bin` on `PATH`: a file named exactly that, or on Windows that name plus
/// one of `PATHEXT`'s extensions (`claude.cmd`). A path is checked as is.
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.is_empty() {
        return None;
    }
    let exts = path_exts(std::env::var("PATHEXT").ok().as_deref());
    let given = Path::new(bin);
    if given.components().count() > 1 || given.is_absolute() {
        let (dir, name) = (given.parent()?, given.file_name()?.to_str()?);
        return candidates(dir, name, cfg!(windows), &exts).into_iter().find(|p| p.is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| candidates(&dir, bin, cfg!(windows), &exts))
        .find(|p| p.is_file())
}

/// What to hand `Command::new` for `bin`: on Windows its full path through
/// `PATHEXT` when it is found (so `.cmd` shims run); elsewhere, and when it
/// is not found, the name unchanged.
pub fn resolve(bin: &str) -> PathBuf {
    if cfg!(windows) {
        if let Some(p) = which(bin) {
            return p;
        }
    }
    PathBuf::from(bin)
}

/// How to start `bin`: the program and the arguments that go before the
/// caller's. Elsewhere that is `bin` itself. On Windows an npm `.cmd` shim
/// is looked through to what it runs (`node ...\cli.js`, or an `.exe`), so
/// arguments reach it whole: through `cmd.exe` a newline in one (a system
/// prompt) cannot be passed at all.
pub fn launcher(bin: &str) -> (PathBuf, Vec<String>) {
    if !cfg!(windows) {
        return (PathBuf::from(bin), Vec::new());
    }
    let path = resolve(bin);
    let is_batch = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if !is_batch {
        return (path, Vec::new());
    }
    let (Some(dir), Ok(text)) = (path.parent(), std::fs::read_to_string(&path)) else {
        return (path, Vec::new());
    };
    match parse_cmd_shim(&text) {
        Some(Shim::Node(script)) => {
            let script = dir.join(script);
            let local = dir.join("node.exe");
            let node = if local.is_file() { local } else { resolve("node") };
            (node, vec![script.to_string_lossy().into_owned()])
        }
        Some(Shim::Exe(exe)) => (dir.join(exe), Vec::new()),
        None => (path, Vec::new()),
    }
}

/// What an npm shim runs, relative to the shim's folder.
#[derive(Debug, PartialEq)]
enum Shim {
    /// A script for node.
    Node(String),
    /// A program of its own.
    Exe(String),
}

/// Reads the last line of npm's `cmd-shim` output:
/// `... "%_prog%"  "%dp0%\node_modules\pkg\cli.js" %*` for a script, or
/// `"%dp0%\node_modules\pkg\bin\tool.exe"   %*` for a program.
fn parse_cmd_shim(text: &str) -> Option<Shim> {
    let line = text.lines().rev().find(|l| l.contains("%*"))?;
    let at = line.rfind("\"%dp0%\\")?;
    let rest = &line[at + "\"%dp0%\\".len()..];
    let target = &rest[..rest.find('"')?];
    if target.is_empty() || target.contains('%') {
        return None;
    }
    let before = &line[..at];
    if before.contains("%_prog%") || before.contains("node") {
        Some(Shim::Node(target.to_string()))
    } else if target.to_ascii_lowercase().ends_with(".exe") {
        Some(Shim::Exe(target.to_string()))
    } else {
        None
    }
}

/// Stop process `pid` and everything it started. Windows has no process
/// groups to signal, so this is `taskkill /T` (the tree, politely), then
/// `/F` for anything still there: at once when the polite one was refused,
/// else three seconds later. Returns at once.
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    std::thread::spawn(move || {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let run = |force: bool| {
            let mut c = Command::new("taskkill");
            c.arg("/T");
            if force {
                c.arg("/F");
            }
            c.args(["/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(NO_WINDOW);
            c.status().map(|s| s.success()).unwrap_or(false)
        };
        // A console program with no window cannot be closed politely, so
        // the first try is often refused; the second does not ask.
        if run(false) {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let _ = run(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathext_reads_like_windows() {
        assert_eq!(path_exts(None), [".com", ".exe", ".bat", ".cmd"]);
        assert_eq!(path_exts(Some(".EXE;.CMD;;bad")), [".exe", ".cmd"]);
    }

    #[test]
    fn a_bare_name_on_windows_tries_each_extension() {
        let exts = path_exts(Some(".EXE;.CMD"));
        let dir = Path::new("bin");
        assert_eq!(
            candidates(dir, "claude", true, &exts),
            [dir.join("claude.exe"), dir.join("claude.cmd")]
        );
        assert_eq!(
            candidates(dir, "claude.cmd", true, &exts),
            [dir.join("claude.cmd"), dir.join("claude.cmd.exe"), dir.join("claude.cmd.cmd")]
        );
        assert_eq!(candidates(dir, "claude", false, &exts), [dir.join("claude")]);
    }

    #[test]
    fn npm_shims_are_looked_through() {
        let node = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*\r\n";
        assert_eq!(parse_cmd_shim(node), Some(Shim::Node(r"node_modules\@anthropic-ai\claude-code\cli.js".into())));
        let exe = "@ECHO off\r\nGOTO start\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.exe\"   %*\r\n";
        assert_eq!(parse_cmd_shim(exe), Some(Shim::Exe(r"node_modules\@openai\codex\bin\codex.exe".into())));
        assert_eq!(parse_cmd_shim("@echo off\r\nmytool.exe %*\r\n"), None);
        assert_eq!(launcher("deck-no-such-tool"), (PathBuf::from("deck-no-such-tool"), Vec::<String>::new()));
    }

    #[test]
    #[cfg(unix)]
    fn finds_a_real_program() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyzzy").is_none());
        assert_eq!(resolve("sh"), PathBuf::from("sh"));
    }
}
