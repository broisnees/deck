//! Handing things to the desktop: open a URL or file in its default app,
//! show a file in the file manager. `open` on macOS, `xdg-open` and the
//! FileManager1 D-Bus call on Linux, the URL handler and Explorer on Windows.

use std::path::Path;
use std::process::Command;

/// The command that opens its one argument (a URL or a path) in the
/// default app.
pub fn opener() -> Command {
    #[cfg(target_os = "macos")]
    return Command::new("/usr/bin/open");
    #[cfg(target_os = "windows")]
    {
        // Not `cmd /C start`: cmd splits an unquoted URL at its `&`.
        let mut c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        return c;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return Command::new("xdg-open");
}

/// Open `target` (a URL or a path) in its default app, without waiting.
pub fn open(target: &str) {
    let _ = opener().arg(target).spawn();
}

/// Show `path` selected in the file manager. Where that can't select it,
/// its folder opens instead.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    return Command::new("/usr/bin/open").arg("-R").arg(path).spawn().map(drop);
    #[cfg(target_os = "windows")]
    {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        return Command::new("explorer").arg(arg).spawn().map(drop);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let uri = format!("file://{}", path.display());
        let shown = Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .status()
            .is_ok_and(|s| s.success());
        if shown {
            return Ok(());
        }
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        Command::new("xdg-open").arg(dir).spawn().map(drop)
    }
}
