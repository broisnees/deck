//! Shortcuts as people read them: `cmd-shift-k` is shown as ⇧⌘K, with the
//! modifiers in the order macOS menus use (⌃⌥⇧⌘). Off macOS ⌘ is Ctrl
//! (`native`), and they read as `Ctrl+Shift+K`.

/// `keys` as this platform binds them: as written on macOS. Elsewhere gpui
/// would read `cmd` as the Super key, which the desktop keeps, and plain
/// Ctrl belongs to the shell, tmux and readline, so Deck's ⌘ shortcuts take
/// Ctrl+Shift instead, as Linux terminals do: ⌘T is Ctrl+Shift+T, ⌘⇧T is
/// Ctrl+Alt+T, ⌥⌘T is Ctrl+Alt+Shift+T, and ⌘1 to ⌘9 are Alt+1 to Alt+9.
/// `plain` keeps ⌘ as plain Ctrl, for what is Ctrl everywhere on Linux:
/// editing in a text field and zoom.
pub fn native(keys: &str, plain: bool) -> String {
    if cfg!(target_os = "macos") {
        return keys.to_string();
    }
    keys.split(' ')
        .map(|stroke| {
            let mut parts: Vec<&str> = stroke.split('-').collect();
            // The key is last, and may itself be "-".
            let key = if stroke.ends_with("--") {
                parts.truncate(parts.len().saturating_sub(2));
                "-"
            } else {
                parts.pop().unwrap_or("")
            };
            let has = |m: &str| parts.contains(&m);
            let (mut ctrl, mut alt, mut shift) = (has("ctrl"), has("alt"), has("shift"));
            let others: Vec<&str> = parts
                .iter()
                .copied()
                .filter(|m| !matches!(*m, "cmd" | "ctrl" | "alt" | "shift"))
                .collect();
            if has("cmd") {
                if plain {
                    ctrl = true;
                } else if key.len() == 1 && key.chars().all(|c| c.is_ascii_digit()) && !alt && !shift {
                    alt = true;
                } else if alt {
                    (ctrl, shift) = (true, true);
                } else if shift {
                    (ctrl, alt, shift) = (true, true, false);
                } else {
                    (ctrl, shift) = (true, true);
                }
            }
            let mut out: Vec<&str> = Vec::new();
            for (on, name) in [(ctrl, "ctrl"), (alt, "alt"), (shift, "shift")] {
                if on {
                    out.push(name);
                }
            }
            out.extend(others);
            out.push(key);
            out.join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Zoom's keys (⌘= ⌘+ ⌘- ⌘0), which stay plain Ctrl off macOS.
pub fn is_zoom(keys: &str) -> bool {
    matches!(keys.rsplit('-').next(), Some("=" | "+" | "0" | ""))
}

/// A keystroke in gpui's form, as symbols.
pub fn pretty(keys: &str) -> String {
    let keys = &native(keys, is_zoom(keys));
    let Ok(k) = gpui::Keystroke::parse(keys) else {
        return keys.to_string();
    };
    let m = k.modifiers;
    if !cfg!(target_os = "macos") {
        let mut out: Vec<String> = Vec::new();
        for (on, name) in [(m.control, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift"), (m.platform, "Super")] {
            if on {
                out.push(name.into());
            }
        }
        out.push(match k.key.as_str() {
            "enter" => "Enter".into(),
            "escape" => "Esc".into(),
            "tab" => "Tab".into(),
            "backspace" => "Backspace".into(),
            "delete" => "Delete".into(),
            "pageup" => "PgUp".into(),
            "pagedown" => "PgDn".into(),
            "home" => "Home".into(),
            "end" => "End".into(),
            _ => key_name(&k.key),
        });
        return out.join("+");
    }
    let mut out = String::new();
    if m.control {
        out.push('⌃');
    }
    if m.alt {
        out.push('⌥');
    }
    if m.shift {
        out.push('⇧');
    }
    if m.platform {
        out.push('⌘');
    }
    if m.function {
        out.push_str("fn ");
    }
    out.push_str(&key_name(&k.key));
    out
}

fn key_name(key: &str) -> String {
    match key {
        "enter" => "↩".into(),
        "escape" => "⎋".into(),
        "tab" => "⇥".into(),
        "space" => "Space".into(),
        "backspace" => "⌫".into(),
        "delete" => "⌦".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "home" => "↖".into(),
        "end" => "↘".into(),
        "pageup" => "⇞".into(),
        "pagedown" => "⇟".into(),
        "-" => "−".into(),
        k if k.len() > 1 && k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit()) => {
            k.to_uppercase()
        }
        k => k.to_uppercase(),
    }
}

/// Whether a recorded key may be a shortcut: it needs ⌘, ⌃ or ⌥ (or is an
/// F-key), or it would take a plain key away from typing.
pub fn allowed(k: &gpui::Keystroke) -> bool {
    let m = k.modifiers;
    m.platform
        || m.control
        || m.alt
        || (k.key.len() > 1
            && k.key.starts_with('f')
            && k.key[1..].chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn command_takes_control_shift_and_reads_as_words() {
        assert_eq!(native("cmd-t", false), "ctrl-shift-t");
        assert_eq!(native("cmd-shift-t", false), "ctrl-alt-t");
        assert_eq!(native("alt-cmd-t", false), "ctrl-alt-shift-t");
        assert_eq!(native("cmd-1", false), "alt-1");
        assert_eq!(native("cmd-shift-]", false), "ctrl-alt-]");
        assert_eq!(native("cmd--", true), "ctrl--");
        assert_eq!(native("cmd-c", true), "ctrl-c");
        assert_eq!(native("ctrl-tab", false), "ctrl-tab");
        assert_eq!(pretty("ctrl-shift-k"), "Ctrl+Shift+K");
        assert_eq!(pretty("ctrl-tab"), "Ctrl+Tab");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shortcuts_read_like_menus() {
        assert_eq!(pretty("cmd-shift-k"), "⇧⌘K");
        assert_eq!(pretty("alt-cmd-i"), "⌥⌘I");
        assert_eq!(pretty("ctrl-tab"), "⌃⇥");
        assert_eq!(pretty("cmd--"), "⌘−");
        assert_eq!(pretty("cmd-,"), "⌘,");
        assert_eq!(pretty("f5"), "F5");
        assert!(allowed(&gpui::Keystroke::parse("cmd-j").unwrap()));
        assert!(allowed(&gpui::Keystroke::parse("f6").unwrap()));
        assert!(!allowed(&gpui::Keystroke::parse("shift-j").unwrap()));
        assert!(!allowed(&gpui::Keystroke::parse("j").unwrap()));
    }
}
