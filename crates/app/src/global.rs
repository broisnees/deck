//! System-wide shortcuts: keys that reach Deck from any app (show or hide
//! Deck, a new chat, a new terminal). Carbon's hot keys, which macOS still
//! supports for exactly this and which need no Accessibility permission.
//!
//! They are the "System-wide" group in Settings › Keyboard, rebound like
//! any other shortcut; each change registers them again.

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use std::ffi::c_void;
use std::sync::Mutex;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HotKeyId {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        count: u32,
        list: *const EventTypeSpec,
        user: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(code: u32, modifiers: u32, id: HotKeyId, target: *mut c_void, options: u32, out: *mut *mut c_void) -> i32;
    fn UnregisterEventHotKey(hot_key: *mut c_void) -> i32;
    fn GetEventParameter(
        event: *mut c_void,
        name: u32,
        kind: u32,
        out_kind: *mut u32,
        size: usize,
        out_size: *mut usize,
        data: *mut c_void,
    ) -> i32;
}

const fn four(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}

const SIGNATURE: u32 = four(b"Deck");

/// The shortcut ids registered, by their hot key number, and the hot keys
/// themselves (to unregister on the next change).
struct State {
    ids: Vec<&'static str>,
    keys: Vec<usize>,
    installed: bool,
}

static STATE: Mutex<State> = Mutex::new(State { ids: Vec::new(), keys: Vec::new(), installed: false });
static TX: Mutex<Option<UnboundedSender<&'static str>>> = Mutex::new(None);

extern "C" fn on_hot_key(_next: *mut c_void, event: *mut c_void, _user: *mut c_void) -> i32 {
    let mut id = HotKeyId::default();
    // SAFETY: Carbon hands us a live hot key event; we read its id.
    let ok = unsafe {
        GetEventParameter(
            event,
            four(b"----"),
            four(b"hkid"),
            std::ptr::null_mut(),
            std::mem::size_of::<HotKeyId>(),
            std::ptr::null_mut(),
            (&mut id as *mut HotKeyId).cast(),
        )
    } == 0;
    if ok && id.signature == SIGNATURE {
        let name = STATE.lock().ok().and_then(|s| s.ids.get(id.id as usize).copied());
        if let (Some(name), Some(tx)) = (name, TX.lock().ok().and_then(|t| t.clone())) {
            let _ = tx.unbounded_send(name);
        }
    }
    0
}

/// Where pressed shortcuts arrive, by shortcut id. Once, at launch.
pub fn start() -> UnboundedReceiver<&'static str> {
    let (tx, rx) = unbounded();
    if let Ok(mut t) = TX.lock() {
        *t = Some(tx);
    }
    rx
}

/// Register `keys` (shortcut id, gpui key like "ctrl-alt-space"), replacing
/// what was registered before. Main thread.
pub fn register(keys: &[(&'static str, String)]) {
    let Ok(mut st) = STATE.lock() else { return };
    // SAFETY: Carbon calls on the main thread with pointers it gave us.
    unsafe {
        if !st.installed {
            let spec = EventTypeSpec { class: four(b"keyb"), kind: 5 };
            let mut handler = std::ptr::null_mut();
            InstallEventHandler(GetApplicationEventTarget(), on_hot_key, 1, &spec, std::ptr::null_mut(), &mut handler);
            st.installed = true;
        }
        for k in st.keys.drain(..) {
            UnregisterEventHotKey(k as *mut c_void);
        }
        st.ids.clear();
        for (id, key) in keys {
            let Some((code, mods)) = parse(key) else {
                tracing::warn!("system-wide shortcut {id}: can't register {key:?}");
                continue;
            };
            let n = st.ids.len() as u32;
            let mut out = std::ptr::null_mut();
            let rc = RegisterEventHotKey(code, mods, HotKeyId { signature: SIGNATURE, id: n }, GetApplicationEventTarget(), 0, &mut out);
            if rc == 0 {
                st.ids.push(id);
                st.keys.push(out as usize);
            } else {
                // Taken by another app: that one keeps it.
                tracing::warn!("system-wide shortcut {key} is taken ({rc})");
                st.ids.push(id);
            }
        }
    }
}

/// "ctrl-alt-space" → (virtual key code, Carbon modifiers).
fn parse(key: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut code = None;
    for part in key.split('-') {
        match part {
            "cmd" => mods |= 0x0100,
            "shift" => mods |= 0x0200,
            "alt" => mods |= 0x0800,
            "ctrl" => mods |= 0x1000,
            k => code = keycode(k),
        }
    }
    // A global key with no modifier would take that key from every app.
    (mods != 0).then_some(())?;
    Some((code?, mods))
}

/// macOS virtual key codes (US layout positions).
fn keycode(k: &str) -> Option<u32> {
    const LETTERS: [(char, u32); 26] = [
        ('a', 0), ('s', 1), ('d', 2), ('f', 3), ('h', 4), ('g', 5), ('z', 6), ('x', 7), ('c', 8), ('v', 9), ('b', 11), ('q', 12), ('w', 13),
        ('e', 14), ('r', 15), ('y', 16), ('t', 17), ('o', 31), ('u', 32), ('i', 34), ('p', 35), ('l', 37), ('j', 38), ('k', 40), ('n', 45),
        ('m', 46),
    ];
    const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some((_, code)) = LETTERS.iter().find(|(l, _)| *l == c) {
            return Some(*code);
        }
        if let Some(d) = c.to_digit(10) {
            return Some(DIGITS[d as usize]);
        }
        return match c {
            '`' => Some(50),
            '-' => Some(27),
            '=' => Some(24),
            '[' => Some(33),
            ']' => Some(30),
            ';' => Some(41),
            '\'' => Some(39),
            ',' => Some(43),
            '.' => Some(47),
            '/' => Some(44),
            '\\' => Some(42),
            _ => None,
        };
    }
    Some(match k {
        "space" => 49,
        "enter" => 36,
        "tab" => 48,
        "escape" => 53,
        "up" => 126,
        "down" => 125,
        "left" => 123,
        "right" => 124,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        assert_eq!(parse("ctrl-alt-space"), Some((49, 0x1800)));
        assert_eq!(parse("cmd-shift-k"), Some((40, 0x0300)));
        assert_eq!(parse("ctrl-alt-t"), Some((17, 0x1800)));
        assert_eq!(parse("t"), None, "a bare key is never global");
    }
}
