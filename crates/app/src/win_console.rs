//! deck.exe is both the app and the `deck` CLI, so it stays a console
//! program. As a GUI-subsystem program a shell would not wait for
//! `deck pane list` to finish, its output would land after the next prompt,
//! and `deck ssh HOST` (an interactive session in that terminal) could not
//! work at all.
//!
//! The cost of a console program is a console window when it starts from
//! Explorer or the Start menu. Two things keep that away:
//!
//! - The manifest (packaging/windows/deck.manifest.xml) asks for
//!   `consoleAllocationPolicy: detached`, so on Windows 11 24H2 and later no
//!   console is made unless deck.exe inherits one from a terminal.
//! - On older Windows one is made. If deck.exe is the only process on it
//!   (nobody launched it from a shell) and it is about to be the window or
//!   the background service, it hides that console and lets it go. It shows
//!   for a frame or so.

use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList, GetConsoleWindow};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

/// Lets go of a console made for this process alone, when `args` start the
/// window (none) or the background service (`daemon`). CLI calls keep it.
pub fn detach_if_own_console(args: &[String]) {
    if !(args.is_empty() || args[0] == "daemon") {
        return;
    }
    let mut ids = [0u32; 2];
    // SAFETY: plain Win32 calls on this process's own console; 0 (no
    // console) or 2+ (a shell shares it) leave it alone.
    unsafe {
        if GetConsoleProcessList(&mut ids) != 1 {
            return;
        }
        let window = GetConsoleWindow();
        if !window.is_invalid() {
            let _ = ShowWindow(window, SW_HIDE);
        }
        let _ = FreeConsole();
    }
}
