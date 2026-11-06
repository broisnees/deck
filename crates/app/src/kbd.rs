//! Keys as keycaps: one look for every shortcut deck shows, in the sidebar,
//! menus, the ⌘K palette's footer, the new tab page, and the chat's hint
//! line, approval cards and messages.
//!
//! The look is the ⌘K palette footer's, where keycaps started: a small
//! rounded pill on a faint tint, in the UI face. Elsewhere keys were plain
//! grey text.

use crate::theme::{self, u};
use gpui::prelude::*;
use gpui::{div, rgb, rgba, Div, SharedString};
use std::ops::Range;

/// One keycap: `⌘K`, `⏎`, `esc`. Off macOS the glyphs read as the keys'
/// names (`Ctrl+K`, `Enter`, `Esc`).
pub fn key(keys: impl Into<SharedString>) -> Div {
    let keys: SharedString = keys.into();
    let keys: SharedString = if cfg!(target_os = "macos") { keys } else { spoken(&keys).into() };
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(u(18.))
        .min_w(u(18.))
        .px(u(5.))
        .rounded(u(4.))
        .bg(rgba(theme::tint()))
        .text_size(u(11.))
        .text_color(rgb(theme::text_2()))
        .child(keys)
}

/// `⇧⌘N` as `Shift+Ctrl+N`, `⏎` as `Enter`: a hint's glyphs by name. These
/// keys are a text field's, where ⌘ is plain Ctrl off macOS.
fn spoken(keys: &str) -> String {
    if keys == "esc" {
        return "Esc".into();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut key = String::new();
    for c in keys.chars() {
        let name = match c {
            '⌘' | '⌃' => Some("Ctrl"),
            '⌥' => Some("Alt"),
            '⇧' => Some("Shift"),
            _ => None,
        };
        match name {
            Some(n) if key.is_empty() => {
                if !parts.iter().any(|p| p == n) {
                    parts.push(n.into());
                }
            }
            _ => match c {
                '⏎' | '↵' | '↩' => key.push_str("Enter"),
                '⇥' => key.push_str("Tab"),
                '⌫' => key.push_str("Backspace"),
                '⌦' => key.push_str("Delete"),
                '⎋' => key.push_str("Esc"),
                _ => key.push(c),
            },
        }
    }
    if !key.is_empty() {
        parts.push(key);
    }
    parts.join("+")
}

/// A hint line, `⏎ send · ⇧⏎ new line · esc close`: each key a keycap, the
/// words beside it as they are, and the parts (split at ` · `) spaced
/// apart instead of dotted, as in the palette footer. The caller sets the
/// words' size, colour and alignment.
pub fn hints(line: &str) -> Div {
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(u(14.))
        .gap_y(u(4.));
    for part in line.split(" · ").filter(|p| !p.trim().is_empty()) {
        row = row.child(phrase(part));
    }
    row
}

/// One part of a hint line: keycaps for the keys, runs of words between.
fn phrase(part: &str) -> Div {
    let mut d = div().flex().items_center().gap(u(5.)).min_w_0();
    let mut words: Vec<&str> = Vec::new();
    for tok in part.split_whitespace() {
        if is_hint_key(tok) {
            if !words.is_empty() {
                d = d.child(words.join(" "));
                words.clear();
            }
            d = d.child(key(tok.to_string()));
        } else {
            words.push(tok);
        }
    }
    if !words.is_empty() {
        d = d.child(words.join(" "));
    }
    d
}

const MODIFIERS: [char; 4] = ['⌘', '⌃', '⌥', '⇧'];
/// Keys that stand alone, including `↩` and `⌦` as Settings › Shortcuts
/// writes them. `←` and `→` are left out: prose uses them as arrows
/// ("Tabs → Move Tab Up").
const NAMED: [char; 9] = ['⏎', '↵', '↩', '⇥', '⌫', '⌦', '⎋', '↑', '↓'];

/// A shortcut as Settings › Shortcuts writes one (`⇧⌘N`, `⌘Space`, `F5`),
/// for a hint that may hold a word instead ("chat", "not installed").
pub fn is_chord(s: &str) -> bool {
    let fkey = s.len() > 1
        && s.starts_with('F')
        && s[1..].chars().all(|c| c.is_ascii_digit());
    is_key(s) || s.starts_with(MODIFIERS) || fkey
}

/// A key or chord as written: modifiers and then one key (`⌘K`, `⇧⌘]`,
/// `⌥⌘↑`), a named key alone (`⏎`, `↑↓`), or `esc`.
fn is_key(tok: &str) -> bool {
    if tok == "esc" || tok == "Esc" {
        return true;
    }
    let rest = tok.trim_start_matches(MODIFIERS);
    let modified = rest.len() < tok.len();
    let named = |s: &str| {
        let n = s.chars().count();
        (1..=2).contains(&n) && s.chars().all(|c| NAMED.contains(&c))
    };
    if modified {
        let mut cs = rest.chars();
        named(rest) || matches!((cs.next(), cs.next()), (Some(c), None) if !c.is_whitespace())
    } else {
        named(rest)
    }
}

/// In a hint line, also what is typed as is: `@` for a file, `1–9`.
fn is_hint_key(tok: &str) -> bool {
    if is_key(tok) || tok == "@" {
        return true;
    }
    let b: Vec<char> = tok.chars().collect();
    matches!(b.as_slice(), [a, '–', z] if a.is_ascii_digit() && z.is_ascii_digit())
}

/// Where keys are in prose, for a message to draw them as keys. Strict:
/// only what reads as a key (`⌃O`, `⇧⌘N`, `⏎`, `esc`), with punctuation
/// around it left out (`(⌘K)`, `⌘K,`).
pub fn find(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for tok in text.split_inclusive(char::is_whitespace) {
        let start = at;
        at += tok.len();
        let word = tok.trim_end();
        let lead = word.len() - word.trim_start_matches(['(', '"', '“', '\'']).len();
        let mut end = word.len();
        // The longest reading first: `⌘,` is a key, `⌘K,` is `⌘K` and a comma.
        for _ in 0..3 {
            if end <= lead {
                break;
            }
            if is_key(&word[lead..end]) {
                out.push(start + lead..start + end);
                break;
            }
            match word[lead..end].chars().last() {
                Some(c) if ".,;:!?)\"”'".contains(c) => end -= c.len_utf8(),
                _ => break,
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_chords_read_as_keys() {
        for k in [
            "⌘K",
            "⇧⌘N",
            "⌃O",
            "⌥⌘I",
            "⇧⌘]",
            "⌘,",
            "⌘−",
            "⌘⏎",
            "⇧⏎",
            "⌥⌘↑",
            "⏎",
            "↵",
            "↑↓",
            "↑",
            "esc",
        ] {
            assert!(is_key(k), "{k}");
        }
        for w in ["→", "←", "^O", "K", "⌘", "⌘-click", "escape", "a", "⌘KK"] {
            assert!(!is_key(w), "{w}");
        }
        assert!(is_hint_key("@") && is_hint_key("1–9"));
        assert!(!is_key("@") && !is_key("1–9"));
    }

    #[test]
    fn chords_from_settings_read_as_keys_and_words_do_not() {
        for k in ["⇧⌘N", "⌘,", "⌘Space", "⌃⇥", "F5", "⌘↩", "⏎"] {
            assert!(is_chord(k), "{k}");
        }
        for w in ["chat", "terminal", "not installed", "F", "Fx"] {
            assert!(!is_chord(w), "{w}");
        }
    }

    #[test]
    fn prose_marks_only_the_keys() {
        let text = "Press ⌃O (or ⌘K), then ⏎. Tabs → Move, esc closes; ⌘, opens Settings";
        let keys: Vec<&str> = find(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(keys, ["⌃O", "⌘K", "⏎", "esc", "⌘,"]);
        assert!(find("pages 1–9 and @someone, a → b").is_empty());
    }
}
