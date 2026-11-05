//! Deck as the Mac's terminal: the app that opens shell scripts (`.command`,
//! `.sh`, `.tool`) and `ssh:`, `telnet:` and `x-man-page:` links, each in a
//! new terminal tab. macOS asks Launch Services which app handles them;
//! Settings and the first launch offer to make it deck, and to hand them
//! back to Terminal.

#[cfg(target_os = "macos")]
use objc2_core_foundation::{CFRetained, CFString};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::ptr::NonNull;
use std::sync::{LazyLock, Mutex};

pub const BUNDLE_ID: &str = "com.broisnischal.deck";
pub const TERMINAL_APP: &str = "com.apple.Terminal";

/// The file types a terminal opens, by uniform type identifier.
const TYPES: [&str; 3] = [
    "com.apple.terminal.shell-script",
    "public.shell-script",
    "public.unix-executable",
];
const SCHEMES: [&str; 3] = ["ssh", "telnet", "x-man-page"];

/// Every role (viewer, editor, shell).
#[cfg(target_os = "macos")]
const ROLES_ALL: u32 = 0xFFFF_FFFF;

#[cfg(target_os = "macos")]
#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn LSSetDefaultRoleHandlerForContentType(
        content_type: *const CFString,
        role: u32,
        handler: *const CFString,
    ) -> i32;
    fn LSCopyDefaultRoleHandlerForContentType(
        content_type: *const CFString,
        role: u32,
    ) -> *mut CFString;
    fn LSSetDefaultHandlerForURLScheme(scheme: *const CFString, handler: *const CFString) -> i32;
}

/// Whether deck opens shell scripts now.
#[cfg(target_os = "macos")]
pub fn is_default() -> bool {
    let ty = CFString::from_static_str(TYPES[0]);
    // SAFETY: a valid CFString in; the result is a +1 reference or null.
    let raw = unsafe { LSCopyDefaultRoleHandlerForContentType(&*ty, ROLES_ALL) };
    let Some(raw) = NonNull::new(raw) else {
        return false;
    };
    // SAFETY: a Copy function's result is ours to release.
    let handler = unsafe { CFRetained::from_raw(raw) };
    handler.to_string().eq_ignore_ascii_case(BUNDLE_ID)
}

/// Make `bundle_id` (deck, or Terminal to hand it back) the app for shell
/// scripts and terminal links. Returns how many of them failed.
#[cfg(target_os = "macos")]
pub fn set_default(bundle_id: &str) -> usize {
    let handler = CFString::from_str(bundle_id);
    let mut failed = 0;
    for ty in TYPES {
        let ty = CFString::from_static_str(ty);
        // SAFETY: valid CFStrings, read during the call only.
        if unsafe { LSSetDefaultRoleHandlerForContentType(&*ty, ROLES_ALL, &*handler) } != 0 {
            failed += 1;
        }
    }
    for scheme in SCHEMES {
        let scheme = CFString::from_static_str(scheme);
        // SAFETY: as above.
        if unsafe { LSSetDefaultHandlerForURLScheme(&*scheme, &*handler) } != 0 {
            failed += 1;
        }
    }
    failed
}

/// Off macOS, being the default terminal belongs to the desktop file
/// (`deck.desktop` with `x-scheme-handler/ssh` and friends), which the
/// Linux packages install; there is nothing to switch here yet.
#[cfg(not(target_os = "macos"))]
pub fn is_default() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn set_default(_bundle_id: &str) -> usize {
    TYPES.len() + SCHEMES.len()
}

// -- opened files and links ---------------------------------------------------------

type Queue = (
    futures::channel::mpsc::UnboundedSender<String>,
    Mutex<Option<futures::channel::mpsc::UnboundedReceiver<String>>>,
);

/// Opened URLs, from AppKit to the workspace. They can arrive before the
/// window exists (deck launched by opening a script), so they wait here.
static OPENED: LazyLock<Queue> = LazyLock::new(|| {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    (tx, Mutex::new(Some(rx)))
});

/// AppKit's `application:openURLs:`.
pub fn opened(urls: Vec<String>) {
    for u in urls {
        let _ = OPENED.0.unbounded_send(u);
    }
}

/// The opened URLs, for the one window that takes them.
pub fn receiver() -> Option<futures::channel::mpsc::UnboundedReceiver<String>> {
    OPENED.1.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// What to run, and where, for an opened URL.
pub fn command_for(url: &str) -> Option<(String, Option<PathBuf>)> {
    let (scheme, rest) = url.split_once("://")?;
    match scheme {
        "file" => {
            let path = PathBuf::from(percent_decode(rest.trim_start_matches("localhost")));
            let dir = path.parent().map(PathBuf::from);
            let quoted = quote(&path.to_string_lossy());
            let cmd = if is_executable(&path) {
                quoted
            } else {
                format!("sh {quoted}")
            };
            Some((cmd, dir))
        }
        "ssh" => {
            let rest = percent_decode(rest.trim_end_matches('/'));
            let (userhost, port) = match rest.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                    (h.to_string(), Some(p.to_string()))
                }
                _ => (rest.clone(), None),
            };
            let mut cmd = "ssh".to_string();
            if let Some(p) = port {
                cmd.push_str(&format!(" -p {p}"));
            }
            cmd.push(' ');
            cmd.push_str(&quote(&userhost));
            Some((cmd, None))
        }
        "telnet" => {
            let rest = percent_decode(rest.trim_end_matches('/'));
            let (host, port) = rest.rsplit_once(':').unwrap_or((&rest, ""));
            Some((
                format!("telnet {} {}", quote(host), quote(port))
                    .trim()
                    .to_string(),
                None,
            ))
        }
        "x-man-page" => {
            // x-man-page://ls or x-man-page://1/printf
            let rest = percent_decode(rest.trim_end_matches('/'));
            let args: Vec<String> = rest
                .split('/')
                .filter(|s| !s.is_empty())
                .map(quote)
                .collect();
            (!args.is_empty()).then(|| (format!("man {}", args.join(" ")), None))
        }
        _ => None,
    }
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `s` in single quotes for the shell, safe whatever it holds.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./@:+=,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_become_commands() {
        assert_eq!(
            command_for("ssh://me@box.local:2222").unwrap().0,
            "ssh -p 2222 me@box.local"
        );
        assert_eq!(command_for("ssh://host/").unwrap().0, "ssh host");
        assert_eq!(
            command_for("x-man-page://1/printf").unwrap().0,
            "man 1 printf"
        );
        assert_eq!(command_for("telnet://h:23").unwrap().0, "telnet h 23");
        assert_eq!(command_for("deck://auth?x=1"), None);
        let (cmd, dir) = command_for("file:///tmp/my%20dir/run%27s.command").unwrap();
        assert_eq!(cmd, r"sh '/tmp/my dir/run'\''s.command'");
        assert_eq!(dir, Some(PathBuf::from("/tmp/my dir")));
        assert_eq!(percent_decode("a%2"), "a%2");
    }
}
