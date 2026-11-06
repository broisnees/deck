//! System-wide shortcuts off macOS. Windows registers them with
//! RegisterHotKey on a thread of their own, whose message loop gets the
//! WM_HOTKEY. Linux does not register them yet: that needs X11 grabs or
//! the GlobalShortcuts portal on Wayland. Deck's in-window shortcuts are
//! unaffected.

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use std::sync::Mutex;

static TX: Mutex<Option<UnboundedSender<&'static str>>> = Mutex::new(None);

/// Where pressed shortcuts arrive, by shortcut id. Once, at launch.
pub fn start() -> UnboundedReceiver<&'static str> {
    let (tx, rx) = unbounded();
    if let Ok(mut t) = TX.lock() {
        *t = Some(tx);
    }
    rx
}

/// Register `keys` (shortcut id, gpui key like "ctrl-alt-space"), replacing
/// what was registered before.
pub fn register(keys: &[(&'static str, String)]) {
    #[cfg(windows)]
    win::register(keys);
    #[cfg(not(windows))]
    if !keys.is_empty() {
        tracing::info!("system-wide shortcuts are not supported on this platform yet");
    }
}

#[cfg(windows)]
mod win {
    use super::TX;
    use std::sync::{Mutex, OnceLock};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
        MOD_WIN,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY,
    };

    /// Asks the hot key thread to register `WANTED` again.
    const WM_RELOAD: u32 = WM_APP + 1;

    static WANTED: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());
    static THREAD: OnceLock<u32> = OnceLock::new();

    pub fn register(keys: &[(&'static str, String)]) {
        if let Ok(mut w) = WANTED.lock() {
            *w = keys.to_vec();
        }
        let tid = *THREAD.get_or_init(spawn);
        // SAFETY: a message with no pointers to a thread that has a queue.
        if tid != 0 {
            let _ = unsafe { PostThreadMessageW(tid, WM_RELOAD, WPARAM(0), LPARAM(0)) };
        }
    }

    /// Start the hot key thread; returns its id once its queue exists, so
    /// the first WM_RELOAD is not lost.
    fn spawn() -> u32 {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("hot keys".into()).spawn(move || {
            let mut msg = MSG::default();
            // SAFETY: plain Win32 calls on this thread's own queue.
            unsafe {
                let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                let _ = tx.send(windows::Win32::System::Threading::GetCurrentThreadId());
                let mut ids: Vec<&'static str> = Vec::new();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    match msg.message {
                        WM_HOTKEY => {
                            let name = ids.get(msg.wParam.0).copied();
                            if let (Some(name), Some(tx)) = (name, TX.lock().ok().and_then(|t| t.clone())) {
                                let _ = tx.unbounded_send(name);
                            }
                        }
                        WM_RELOAD => {
                            for n in 0..ids.len() {
                                let _ = UnregisterHotKey(None, n as i32);
                            }
                            ids.clear();
                            let wanted = WANTED.lock().map(|w| w.clone()).unwrap_or_default();
                            for (id, key) in wanted {
                                let n = ids.len() as i32;
                                // Keep the numbering even when one fails.
                                ids.push(id);
                                let Some((vk, mods)) = parse(&key) else {
                                    tracing::warn!("system-wide shortcut {id}: can't register {key:?}");
                                    continue;
                                };
                                if let Err(e) = RegisterHotKey(None, n, mods | MOD_NOREPEAT, vk) {
                                    // Taken by another app: that one keeps it.
                                    tracing::warn!("system-wide shortcut {key} is taken ({e})");
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        if spawned.is_err() {
            return 0;
        }
        rx.recv().unwrap_or(0)
    }

    /// "ctrl-alt-space" to (virtual key, modifiers). Cmd is Ctrl here.
    fn parse(key: &str) -> Option<(u32, HOT_KEY_MODIFIERS)> {
        let (mods, last) = match key.strip_suffix("--") {
            Some(rest) => (rest, "-"),
            None => key.rsplit_once('-').unwrap_or(("", key)),
        };
        let mut m = HOT_KEY_MODIFIERS(0);
        for part in mods.split('-').filter(|p| !p.is_empty()) {
            m |= match part {
                "cmd" | "ctrl" => MOD_CONTROL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                "super" | "win" => MOD_WIN,
                _ => return None,
            };
        }
        // A global key with no modifier would take that key from every app.
        (m.0 != 0).then_some(())?;
        Some((vk(last)?, m))
    }

    /// Windows virtual key codes (US layout positions).
    fn vk(k: &str) -> Option<u32> {
        let mut chars = k.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_alphanumeric() {
                return Some(c.to_ascii_uppercase() as u32);
            }
            return Some(match c {
                '`' => 0xC0,
                '-' => 0xBD,
                '=' => 0xBB,
                '[' => 0xDB,
                ']' => 0xDD,
                ';' => 0xBA,
                '\'' => 0xDE,
                ',' => 0xBC,
                '.' => 0xBE,
                '/' => 0xBF,
                '\\' => 0xDC,
                _ => return None,
            });
        }
        if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
            return (1..=24).contains(&n).then_some(0x6F + n);
        }
        Some(match k {
            "space" => 0x20,
            "enter" => 0x0D,
            "tab" => 0x09,
            "escape" => 0x1B,
            "backspace" => 0x08,
            "delete" => 0x2E,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" => 0x21,
            "pagedown" => 0x22,
            "up" => 0x26,
            "down" => 0x28,
            "left" => 0x25,
            "right" => 0x27,
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn keys_parse() {
            assert_eq!(parse("ctrl-alt-space"), Some((0x20, MOD_CONTROL | MOD_ALT)));
            assert_eq!(parse("cmd-shift-k"), Some((0x4B, MOD_CONTROL | MOD_SHIFT)));
            assert_eq!(parse("ctrl-alt-t"), Some((0x54, MOD_CONTROL | MOD_ALT)));
            assert_eq!(parse("ctrl--"), Some((0xBD, MOD_CONTROL)));
            assert_eq!(parse("t"), None, "a bare key is never global");
        }
    }
}
