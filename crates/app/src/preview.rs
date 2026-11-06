//! An attachment opened inside deck: an image at full size, a text file
//! with line numbers, a folder's entries. Anything else says what it is and
//! offers to open it in the app macOS picks.
//!
//! Before this a click handed the file to `open`, which left deck, and in
//! the composer a click did nothing at all.

use crate::icon::{icon, Icon};
use crate::theme::{self, u, OneLine};
use gpui::prelude::*;
use gpui::{div, img, rgb, rgba, AnyElement, FontWeight, SharedString};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Bytes of a text file read for its preview.
const TEXT_BYTES: u64 = 256 * 1024;
const TEXT_LINES: usize = 3000;
const FOLDER_ENTRIES: usize = 500;

pub struct Preview {
    pub path: PathBuf,
    pub body: Body,
}

pub enum Body {
    Loading,
    Image {
        size: Option<(u32, u32)>,
        bytes: u64,
    },
    Text {
        numbers: SharedString,
        text: SharedString,
        lines: usize,
        /// Only the start of the file is shown.
        cut: bool,
        bytes: u64,
    },
    Folder {
        entries: Vec<(String, bool)>,
        more: usize,
    },
    /// Not something deck can draw: a PDF, an archive, a binary.
    Other {
        bytes: u64,
    },
    Missing(String),
}

impl Preview {
    pub fn loading(path: PathBuf) -> Self {
        Self {
            path,
            body: Body::Loading,
        }
    }
}

/// Read what the preview of `path` shows. Touches the disk, so run it off
/// the main thread.
pub fn load(path: &Path) -> Body {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return Body::Missing(e.to_string()),
    };
    if meta.is_dir() {
        return folder(path);
    }
    let bytes = meta.len();
    if crate::chat::is_image(path) {
        return Body::Image {
            size: image::image_dimensions(path).ok(),
            bytes,
        };
    }
    let mut head = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| f.take(TEXT_BYTES).read_to_end(&mut head));
    if let Err(e) = read {
        return Body::Missing(e.to_string());
    }
    match text(&head, bytes > TEXT_BYTES) {
        Some((numbers, text, lines, cut)) => Body::Text {
            numbers: numbers.into(),
            text: text.into(),
            lines,
            cut,
            bytes,
        },
        None => Body::Other { bytes },
    }
}

/// `head` as numbered lines, or `None` if it is not text. `truncated` says
/// the file goes on past `head`, so a character cut in half at the end is
/// not a reason to call it binary.
fn text(head: &[u8], truncated: bool) -> Option<(String, String, usize, bool)> {
    if head.contains(&0) {
        return None;
    }
    let s = match std::str::from_utf8(head) {
        Ok(s) => s,
        Err(e) if truncated && head.len() - e.valid_up_to() < 4 => {
            std::str::from_utf8(&head[..e.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    let mut lines: Vec<&str> = s.lines().collect();
    let mut cut = truncated;
    if lines.len() > TEXT_LINES {
        lines.truncate(TEXT_LINES);
        cut = true;
    }
    // Tabs as four spaces: the grid stays even and nothing draws a glyph
    // for the tab itself.
    let text = lines
        .iter()
        .map(|l| l.replace('\t', "    "))
        .collect::<Vec<_>>()
        .join("\n");
    let numbers = (1..=lines.len().max(1))
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    Some((numbers, text, lines.len(), cut))
}

fn folder(path: &Path) -> Body {
    let read = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(e) => return Body::Missing(e.to_string()),
    };
    let mut entries: Vec<(String, bool)> = read
        .flatten()
        .map(|e| {
            let dir = e.file_type().is_ok_and(|t| t.is_dir());
            (e.file_name().to_string_lossy().into_owned(), dir)
        })
        .collect();
    // Folders first, then by name as Finder sorts it.
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    let more = entries.len().saturating_sub(FOLDER_ENTRIES);
    entries.truncate(FOLDER_ENTRIES);
    Body::Folder { entries, more }
}

/// The line under the name: what it is and how big.
pub fn detail(p: &Preview) -> String {
    let kind = p
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let size = crate::inspect::human_bytes;
    let parts: Vec<String> = match &p.body {
        Body::Loading => vec![],
        Body::Image { size: dims, bytes } => {
            let mut v = vec![kind];
            if let Some((w, h)) = dims {
                v.push(format!("{w} × {h}"));
            }
            v.push(size(*bytes));
            v
        }
        Body::Text {
            lines, cut, bytes, ..
        } => vec![
            if *cut {
                format!("first {lines} lines")
            } else if *lines == 1 {
                "1 line".into()
            } else {
                format!("{lines} lines")
            },
            size(*bytes),
        ],
        Body::Folder { entries, more } => {
            let n = entries.len() + more;
            vec![if n == 1 {
                "1 item".into()
            } else {
                format!("{n} items")
            }]
        }
        Body::Other { bytes } => vec![kind, size(*bytes)],
        Body::Missing(_) => vec!["Can't be read".into()],
    };
    parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What the preview shows under its header.
pub fn body(p: &Preview) -> AnyElement {
    let centred = |child: AnyElement| {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(u(10.))
            .p(u(24.))
            .child(child)
    };
    match &p.body {
        Body::Loading => centred(div().into_any_element()).into_any_element(),
        Body::Image { .. } => div()
            .size_full()
            .p(u(16.))
            .child(
                img(p.path.clone())
                    .size_full()
                    .object_fit(gpui::ObjectFit::Contain),
            )
            .into_any_element(),
        Body::Text {
            numbers, text, cut, ..
        } => {
            let mono = |d: gpui::Div| {
                d.font_family(theme::mono())
                    .text_size(u(12.))
                    .line_height(u(18.))
                    .whitespace_nowrap()
            };
            div()
                .id("preview-text")
                .size_full()
                .overflow_scroll()
                .child(
                    div()
                        .flex()
                        .p(u(14.))
                        .child(
                            mono(div())
                                .flex_none()
                                .pr(u(16.))
                                .text_right()
                                .text_color(rgb(theme::dim()))
                                .child(numbers.clone()),
                        )
                        .child(
                            mono(div())
                                .flex_none()
                                .text_color(rgb(theme::text()))
                                .child(text.clone()),
                        ),
                )
                .when(*cut, |d| {
                    d.child(
                        div()
                            .px(u(14.))
                            .pb(u(14.))
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child("The rest of the file is not shown. Open it to see all of it."),
                    )
                })
                .into_any_element()
        }
        Body::Folder { entries, more } => {
            let mut list = div().flex().flex_col().p(u(8.));
            for (name, dir) in entries {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .h(u(26.))
                        .px(u(8.))
                        .text_size(u(12.))
                        .text_color(rgb(if name.starts_with('.') {
                            theme::dim()
                        } else {
                            theme::text()
                        }))
                        .child(icon(
                            if *dir { Icon::Folder } else { Icon::File },
                            13.,
                            theme::dim(),
                        ))
                        .child(div().min_w_0().one_line().child(name.clone())),
                );
            }
            if *more > 0 {
                list = list.child(
                    div()
                        .px(u(8.))
                        .pt(u(6.))
                        .text_size(u(11.))
                        .text_color(rgb(theme::dim()))
                        .child(format!("and {more} more")),
                );
            }
            if entries.is_empty() {
                list = list.child(note("This folder is empty."));
            }
            div()
                .id("preview-folder")
                .size_full()
                .overflow_y_scroll()
                .child(list)
                .into_any_element()
        }
        Body::Other { .. } => centred(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(u(10.))
                .child(icon(Icon::File, 40., theme::dim()))
                .child(note(
                    "Deck can't show this kind of file. Open it to see it.",
                ))
                .into_any_element(),
        )
        .into_any_element(),
        Body::Missing(why) => centred(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(u(10.))
                .child(icon(Icon::File, 40., theme::dim()))
                .child(note(format!("This file can't be read: {why}")))
                .into_any_element(),
        )
        .into_any_element(),
    }
}

fn note(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(u(12.))
        .text_color(rgb(theme::text_2()))
        .child(text.into())
}

/// A small text button for the header.
pub fn action(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(u(26.))
        .px(u(10.))
        .rounded(u(7.))
        .border_1()
        .border_color(rgba(theme::hairline()))
        .text_size(u(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::text_2()))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_numbered_and_tabs_are_spaces() {
        let (numbers, text, lines, cut) = text(b"fn main() {\n\tok();\n}\n", false).unwrap();
        assert_eq!(numbers, "1\n2\n3");
        assert_eq!(text, "fn main() {\n    ok();\n}");
        assert_eq!((lines, cut), (3, false));
    }

    #[test]
    fn binary_is_not_text() {
        assert!(text(b"%PDF-1.7\n\x00\x01\x02", false).is_none());
        assert!(text(&[0xff, 0xfe, 0x41], false).is_none());
    }

    #[test]
    fn a_character_cut_at_the_read_limit_is_still_text() {
        let mut head = "héllo\n".repeat(3).into_bytes();
        head.extend_from_slice(&"é".as_bytes()[..1]);
        let (_, text, lines, cut) = text(&head, true).unwrap();
        assert_eq!((lines, cut), (3, true));
        assert!(text.ends_with("héllo"));
        // The same bytes in a file that ends there are not UTF-8.
        assert!(super::text(&head, false).is_none());
    }

    #[test]
    fn long_files_stop_at_the_line_limit() {
        let many = "x\n".repeat(TEXT_LINES + 10);
        let (_, _, lines, cut) = text(many.as_bytes(), false).unwrap();
        assert_eq!((lines, cut), (TEXT_LINES, true));
    }

    #[test]
    fn loads_what_each_kind_needs() {
        let dir = std::env::temp_dir().join(format!("deck-preview-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("notes.md"), "# hi\n").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        image::RgbImage::new(30, 20)
            .save(dir.join("pic.png"))
            .unwrap();

        assert!(matches!(
            load(&dir.join("notes.md")),
            Body::Text { lines: 1, .. }
        ));
        assert!(matches!(
            load(&dir.join("blob.bin")),
            Body::Other { bytes: 4 }
        ));
        assert!(matches!(
            load(&dir.join("pic.png")),
            Body::Image {
                size: Some((30, 20)),
                ..
            }
        ));
        assert!(matches!(load(&dir.join("gone.txt")), Body::Missing(_)));
        match load(&dir) {
            Body::Folder { entries, more } => {
                assert_eq!(more, 0);
                // Folders first.
                assert_eq!(entries[0], ("sub".to_string(), true));
                assert_eq!(entries.len(), 4);
            }
            _ => panic!("expected a folder"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
