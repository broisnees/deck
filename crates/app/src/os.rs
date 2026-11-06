//! The few things Unix and Windows do differently that the rest of Deck
//! needs: the control socket's types (AF_UNIX on both; Windows 10 and later
//! have it through `uds_windows`), owner-only permissions, and starting a
//! program in place of this one or detached from it.

use std::path::Path;
use std::process::Command;

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
pub use uds_windows::{UnixListener, UnixStream};

/// Only the owner may read or enter `path` (0700 for a folder, 0600 for a
/// file). Windows keeps a user's profile private already.
pub fn private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Open `path` to write from the start, readable only by the owner.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// A symbolic link at `link` to `target`. Windows needs a privilege for
/// these, so there it fails and callers do without.
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no symlinks here"))
    }
}

/// Whether `p` is a program that can be run: an executable bit on Unix, a
/// program's extension on Windows.
pub fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        p.is_file() && matches!(ext.as_str(), "exe" | "bat" | "cmd" | "com" | "ps1")
    }
}

/// Run `cmd` in place of this process. Unix replaces it; Windows runs it
/// and exits with its code. Returns only when it could not start.
pub fn exec(cmd: &mut Command) -> std::io::Error {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => e,
    }
}

/// Start `cmd` on its own: a session (Unix) or a detached process
/// (Windows) of its own, so it outlives Deck and no signal to Deck's group
/// reaches it.
pub fn detach(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches nothing else.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// No console window for a helper (git, curl, ssh) started from the
/// windowed app on Windows; unchanged elsewhere.
pub fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(agent_core::exec::NO_WINDOW);
    }
    cmd
}

/// A program that ships with the system, by its full path where Deck
/// relies on that one (`/usr/bin/curl`), and on Windows from System32
/// (`curl.exe` and `tar.exe` come with Windows 10 and later).
pub fn system_tool(unix: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let name = Path::new(unix).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let sys = std::path::PathBuf::from(root).join("System32").join(format!("{name}.exe"));
        if sys.is_file() {
            return sys;
        }
        return agent_core::exec::resolve(&name);
    }
    #[cfg(not(windows))]
    std::path::PathBuf::from(unix)
}
