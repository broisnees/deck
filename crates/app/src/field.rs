//! Text fields: the address bar (one line, scrolls sideways) and the chat
//! composer (wraps, grows). Typing goes through the platform input handler,
//! so the IME, dead keys, accents and the emoji picker all work; `key_down`
//! only handles editing commands.

use crate::icon::Icon;
use crate::menu::{self, Entry, Item};
use crate::pasteboard::{self, Pasted};
use crate::textbuf::{Move, TextInput};
use crate::theme::{self, u, OneLine};
use crate::workspace::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui::prelude::*;
use gpui::{
    canvas, div, fill, point, px, rgb, rgba, size, Bounds, ClipboardItem, Context,
    ElementInputHandler, EntityInputHandler, EventEmitter, FocusHandle, HighlightStyle,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollHandle,
    SharedString, StyledText, TextLayout, UTF16Selection, UnderlineStyle, Window,
};
use std::ops::Range;

pub enum FieldEvent {
    Submit(String),
    /// ⌘⏎ / ⌃⏎ in a multi-line field: send, and have it read right away.
    SubmitNow,
    Cancel,
    /// ⌘V brought files or an image, in a field that takes them.
    Attach(Pasted),
    /// The text changed, by typing, pasting, undo or history.
    Changed,
    /// A key the owner claimed while its popup is open: `up`, `down`,
    /// `enter`, `tab` or `escape`.
    PopupKey(&'static str),
}

/// What kind of edit ran, so a run of typing or of deleting undoes as one
/// step rather than one per key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    Type,
    Delete,
    Other,
}

/// A state undo can return to.
struct Snap {
    text: String,
    cursor: usize,
    anchor: usize,
}

const UNDO_DEPTH: usize = 200;
const HISTORY_DEPTH: usize = 100;

pub struct Field {
    buf: TextInput,
    pub focus: FocusHandle,
    placeholder: SharedString,
    /// Shown instead of the text while the field is not being edited (the
    /// address bar shows `example.com`, edits `https://example.com/`).
    pub display: Option<SharedString>,
    multiline: bool,
    /// Byte range of an IME composition inside the text.
    marked: Option<Range<usize>>,
    /// The last painted text layout. Only set once measured, because asking
    /// an unmeasured layout for positions panics.
    layout: Option<TextLayout>,
    /// Single-line fields scroll sideways to keep the caret in view.
    scroll_x: Pixels,
    selecting: bool,
    /// No frame of its own: the composer draws the card around it.
    pub bare: bool,
    /// A multi-line field where ⏎ types a newline, for writing text to keep
    /// rather than a message to send.
    pub editor: bool,
    /// ⌘V takes files and images (as `FieldEvent::Attach`), not just text.
    pub attachments: bool,
    /// The owner shows a popup (completions) that takes the arrow keys,
    /// Enter, Tab and Escape.
    pub popup: bool,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    /// The kind of the last edit; the next one of the same kind joins its
    /// undo step.
    last_run: Run,
    /// Messages sent before, oldest first; ↑ in an empty field walks back.
    history: Vec<String>,
    hist_ix: Option<usize>,
    /// What was typed before ↑ started walking the history.
    hist_draft: String,
    /// x the caret aims for while moving up and down through wrapped lines.
    goal_x: Option<Pixels>,
    /// Multi-line fields scroll; the caret is kept in view when it moves.
    scroll: ScrollHandle,
    shown_caret: Option<usize>,
    /// The right-click menu, while open.
    menu: Option<menu::Open>,
}

impl EventEmitter<FieldEvent> for Field {}

impl Field {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            buf: TextInput::new(),
            focus: cx.focus_handle(),
            placeholder: placeholder.into(),
            display: None,
            multiline: false,
            marked: None,
            layout: None,
            scroll_x: px(0.),
            selecting: false,
            bare: false,
            editor: false,
            attachments: false,
            popup: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_run: Run::Other,
            history: Vec::new(),
            hist_ix: None,
            hist_draft: String::new(),
            goal_x: None,
            scroll: ScrollHandle::new(),
            shown_caret: None,
            menu: None,
        }
    }

    pub fn multiline(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            multiline: true,
            ..Self::new(placeholder, cx)
        }
    }

    pub fn text(&self) -> &str {
        self.buf.text()
    }

    /// Replace the text. Undoable, so ⌘Z after sending brings a message back.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.buf.text() != text {
            let text = text.to_string();
            self.edit(Run::Other, cx, |b| b.set_text(text));
            self.marked = None;
            self.hist_ix = None;
        }
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.buf.selected_text()
    }

    /// The text from the start up to the caret.
    pub fn before_caret(&self) -> &str {
        &self.buf.text()[..self.buf.cursor()]
    }

    /// Replace the `len` bytes just before the caret with `text`, as one
    /// undo step (a `;keyword ` expanding: ⌘Z brings the keyword back).
    pub fn replace_before_caret(&mut self, len: usize, text: &str, cx: &mut Context<Self>) {
        let end = self.buf.cursor();
        let Some(start) = end.checked_sub(len) else {
            return;
        };
        let text = text.to_string();
        self.edit(Run::Other, cx, |b| {
            b.place(start, false);
            b.place(end, true);
            b.insert(&text);
        });
        self.marked = None;
    }

    /// Put `text` in at the caret, over any selection. One undo step.
    pub fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = if self.multiline {
            text.to_string()
        } else {
            text.replace(['\r', '\n'], " ")
        };
        self.edit(Run::Other, cx, |b| b.insert(&text));
        self.marked = None;
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.buf.select_all();
        cx.notify();
    }

    /// Show `rest` after the text, selected, the way an address bar
    /// completes a host: typing on replaces it and ⌫ drops it. Not an edit,
    /// so no undo step and no `Changed`.
    pub fn complete_with(&mut self, rest: &str, cx: &mut Context<Self>) {
        let typed = self.buf.text().len();
        let text = format!("{}{rest}", self.buf.text());
        let end = text.len();
        self.buf.restore(text, end, typed);
        cx.notify();
    }

    /// The caret sits at the end with nothing selected: typing appends.
    pub fn caret_at_end(&self) -> bool {
        self.buf.selection().is_none() && self.buf.cursor() == self.buf.text().len()
    }

    /// Start the undo history here (a restored draft is not an edit).
    pub fn forget_undo(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_run = Run::Other;
    }

    /// Remember a sent message for ↑.
    pub fn push_history(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            if self.history.len() > HISTORY_DEPTH {
                self.history.remove(0);
            }
        }
        self.hist_ix = None;
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.hist_ix = None;
    }

    /// The `@…` being typed at the caret: its start and what follows the @.
    pub fn mention(&self) -> Option<(usize, String)> {
        self.buf.mention_token().map(|(at, q)| (at, q.to_string()))
    }

    /// `/command` at the start, or `$skill`: (sigil, start, query).
    pub fn command_token(&self) -> Option<(char, usize, String)> {
        self.buf
            .slash_token()
            .map(|(a, q)| ('/', a, q.to_string()))
            .or_else(|| self.buf.dollar_token().map(|(a, q)| ('$', a, q.to_string())))
    }

    pub fn complete_token(&mut self, start: usize, with: &str, cx: &mut Context<Self>) {
        let with = with.to_string();
        self.edit(Run::Other, cx, |b| b.replace_token(start, &with));
    }

    pub fn complete_mention(&mut self, start: usize, path: &str, cx: &mut Context<Self>) {
        let path = path.to_string();
        self.edit(Run::Other, cx, |b| b.replace_mention(start, &path));
    }

    /// Run an edit, recording an undo step unless it continues a run of the
    /// same kind, and tell the owner the text changed.
    fn edit(&mut self, run: Run, cx: &mut Context<Self>, f: impl FnOnce(&mut TextInput)) {
        let before = Snap {
            text: self.buf.text().to_string(),
            cursor: self.buf.cursor(),
            anchor: self.buf.anchor(),
        };
        f(&mut self.buf);
        self.goal_x = None;
        if self.buf.text() == before.text {
            cx.notify();
            return;
        }
        if run == Run::Other || run != self.last_run {
            self.undo.push(before);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_run = run;
        self.hist_ix = None;
        cx.emit(FieldEvent::Changed);
        cx.notify();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(Snap {
                text: self.buf.text().to_string(),
                cursor: self.buf.cursor(),
                anchor: self.buf.anchor(),
            });
            self.buf.restore(s.text, s.cursor, s.anchor);
            self.after_jump(cx);
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(Snap {
                text: self.buf.text().to_string(),
                cursor: self.buf.cursor(),
                anchor: self.buf.anchor(),
            });
            self.buf.restore(s.text, s.cursor, s.anchor);
            self.after_jump(cx);
        }
    }

    fn after_jump(&mut self, cx: &mut Context<Self>) {
        self.last_run = Run::Other;
        self.marked = None;
        self.goal_x = None;
        cx.emit(FieldEvent::Changed);
        cx.notify();
    }

    /// ↑ / ↓ through the sent messages. Returns false when there is nowhere
    /// to go, so the caret moves instead.
    fn walk_history(&mut self, dir: i32, cx: &mut Context<Self>) -> bool {
        let next = match (self.hist_ix, dir) {
            (None, -1) if !self.history.is_empty() && self.buf.is_empty() => {
                Some(self.history.len() - 1)
            }
            (Some(i), -1) if i > 0 => Some(i - 1),
            (Some(_), -1) => return true,
            (Some(i), 1) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), 1) => None,
            _ => return false,
        };
        if self.hist_ix.is_none() {
            self.hist_draft = self.buf.text().to_string();
        }
        let text = match next {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.hist_draft),
        };
        self.buf.set_text(text);
        self.hist_ix = next;
        self.last_run = Run::Other;
        self.goal_x = None;
        cx.emit(FieldEvent::Changed);
        true
    }

    /// Move the caret one visual line up or down, through soft wraps as well
    /// as newlines. False when already on the first or last line.
    fn move_visual(&mut self, dir: f32, mv: Move) -> bool {
        let Some(layout) = self.layout.as_ref() else {
            return if dir < 0. {
                self.buf.move_up(mv)
            } else {
                self.buf.move_down(mv)
            };
        };
        let Some(p) = layout.position_for_index(self.buf.cursor()) else {
            return false;
        };
        let lh = layout.line_height();
        let bounds = layout.bounds();
        let y = p.y + lh * dir + lh / 2.;
        if y < bounds.top() || y > bounds.bottom() {
            return false;
        }
        let x = *self.goal_x.get_or_insert(p.x);
        let ix = match layout.index_for_position(point(x, y)) {
            Ok(i) | Err(i) => i.min(self.buf.text().len()),
        };
        self.buf.place(ix, mv == Move::Extend);
        true
    }

    fn selection_or_cursor(&self) -> Range<usize> {
        let (s, e) = self
            .buf
            .selection()
            .unwrap_or((self.buf.cursor(), self.buf.cursor()));
        s..e
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Mid-composition, every key belongs to the IME.
        if self.marked.is_some() {
            return;
        }
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        if self.popup && !m.platform && !m.alt && !m.control {
            let claimed = match ks.key.as_str() {
                "up" => Some("up"),
                "down" => Some("down"),
                "enter" if !m.shift => Some("enter"),
                "tab" => Some("tab"),
                "escape" => Some("escape"),
                _ => None,
            };
            if let Some(k) = claimed {
                cx.emit(FieldEvent::PopupKey(k));
                cx.stop_propagation();
                return;
            }
        }
        let mv = if m.shift {
            Move::Extend
        } else {
            Move::Collapse
        };
        let vertical = matches!(ks.key.as_str(), "up" | "down");
        // Off macOS, Ctrl does what ⌥ does there (by word) and Ctrl+Alt
        // what ⌘ does (to the line's start or end), as in GTK and Qt.
        let linux = !cfg!(target_os = "macos");
        let by_line = m.platform || (linux && m.control && m.alt);
        let by_word = !by_line && (m.alt || (linux && m.control));
        match ks.key.as_str() {
            "enter" if self.multiline && (self.editor || m.shift || m.alt) => {
                self.edit(Run::Other, cx, |b| b.insert("\n"))
            }
            "enter" if self.multiline && (m.platform || m.control) => {
                cx.emit(FieldEvent::SubmitNow)
            }
            "enter" => cx.emit(FieldEvent::Submit(self.buf.text().to_string())),
            "escape" => cx.emit(FieldEvent::Cancel),
            "backspace" if by_line => self.edit(Run::Other, cx, |b| b.delete_to_line_start()),
            "backspace" if by_word => self.edit(Run::Other, cx, |b| b.delete_word_left()),
            "backspace" => self.edit(Run::Delete, cx, |b| b.backspace()),
            "delete" if by_word => self.edit(Run::Other, cx, |b| b.delete_word_right()),
            "delete" => self.edit(Run::Delete, cx, |b| b.delete_forward()),
            "left" if by_line => self.buf.move_line_start(mv),
            "left" if by_word => self.buf.move_word_left(mv),
            "left" => self.buf.move_left(mv),
            "right" if by_line => self.buf.move_line_end(mv),
            "right" if by_word => self.buf.move_word_right(mv),
            "right" => self.buf.move_right(mv),
            "up" if m.platform => self.buf.move_doc_start(mv),
            "down" if m.platform => self.buf.move_doc_end(mv),
            "up" if self.multiline => {
                if !self.move_visual(-1., mv) && (m.shift || !self.walk_history(-1, cx)) {
                    self.buf.move_doc_start(mv);
                }
            }
            "down" if self.multiline => {
                if !self.move_visual(1., mv) && (m.shift || !self.walk_history(1, cx)) {
                    self.buf.move_doc_end(mv);
                }
            }
            "home" | "up" => self.buf.move_line_start(mv),
            "end" | "down" => self.buf.move_line_end(mv),
            // Text, and ⌘ shortcuts: not ours. Text reaches us through the
            // input handler below; shortcuts go to the app.
            _ => return,
        }
        // A caret move ends a run, so undo stops where the caret jumped.
        self.last_run = Run::Other;
        if !vertical {
            self.goal_x = None;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn index_at(&self, pos: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        Some(match layout.index_for_position(pos) {
            Ok(i) | Err(i) => i.min(self.buf.text().len()),
        })
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let was_focused = self.focus.is_focused(window);
        window.focus(&self.focus);
        crate::browser::reclaim_keyboard(window);
        // The first click into an unfocused address bar selects it all, as in
        // every browser; later clicks place the caret.
        self.last_run = Run::Other;
        self.goal_x = None;
        if !was_focused && !self.multiline {
            self.buf.select_all();
        } else if let Some(ix) = self.index_at(ev.position) {
            match ev.click_count {
                2 => self.buf.select_word_at(ix),
                3.. => self.buf.select_line_at(ix),
                _ => {
                    self.buf.place(ix, ev.modifiers.shift);
                    self.selecting = true;
                }
            }
        }
        cx.notify();
    }

    /// Cut, Copy, Paste, Select All. A right-click outside the selection
    /// moves the caret there first, as in any Mac text view.
    fn on_right_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The pane around the field (a terminal's find bar) has a menu of
        // its own; this click is the field's.
        window.prevent_default();
        let was_focused = self.focus.is_focused(window);
        window.focus(&self.focus);
        crate::browser::reclaim_keyboard(window);
        if !was_focused && !self.multiline {
            self.buf.select_all();
        } else if let Some(ix) = self.index_at(ev.position) {
            let inside = self
                .buf
                .selection()
                .is_some_and(|(s, e)| (s..=e).contains(&ix));
            if !inside {
                self.buf.place(ix, false);
            }
        }
        let selected = self.buf.selection().is_some();
        let f = self.focus.clone();
        let entries: Vec<Entry> = vec![
            Item::new("Cut", menu::send(f.clone(), Cut))
                .icon(Icon::Scissors)
                .shortcut("⌘X")
                .disabled(!selected)
                .into(),
            Item::new("Copy", menu::send(f.clone(), Copy))
                .icon(Icon::Copy)
                .shortcut("⌘C")
                .disabled(!selected)
                .into(),
            Item::new("Paste", menu::send(f.clone(), Paste))
                .icon(Icon::Clipboard)
                .shortcut("⌘V")
                .into(),
            Entry::Separator,
            Item::new("Select All", menu::send(f.clone(), SelectAll))
                .icon(Icon::SelectAll)
                .shortcut("⌘A")
                .disabled(self.buf.is_empty())
                .into(),
        ];
        self.menu = Some(menu::open(
            entries,
            ev.position,
            Some(f),
            window,
            cx,
            |f: &mut Self, _, _| f.menu = None,
        ));
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            return;
        }
        if let Some(ix) = self.index_at(ev.position) {
            self.buf.place(ix, true);
            cx.notify();
        }
    }

    // UTF-16 offsets are what the platform speaks; the buffer is UTF-8.
    fn to_utf16(&self, byte: usize) -> usize {
        self.buf.text()[..byte.min(self.buf.text().len())]
            .encode_utf16()
            .count()
    }

    fn byte_of_utf16(&self, units: usize) -> usize {
        utf16_to_byte(self.buf.text(), units)
    }

    fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.to_utf16(r.start)..self.to_utf16(r.end)
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.byte_of_utf16(r.start)..self.byte_of_utf16(r.end)
    }
}

fn utf16_to_byte(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (i, ch) in text.char_indices() {
        if seen >= units {
            return i;
        }
        seen += ch.len_utf16();
    }
    text.len()
}

impl EntityInputHandler for Field {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let r = self.range_from_utf16(&range);
        *adjusted = Some(self.range_to_utf16(&r));
        Some(self.buf.text()[r].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let r = self.selection_or_cursor();
        Some(UTF16Selection {
            reversed: self.buf.cursor() == r.start && r.start != r.end,
            range: self.range_to_utf16(&r),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A single-line field takes a pasted or dictated newline as a space.
        let text = if self.multiline {
            text.to_string()
        } else {
            text.replace(['\r', '\n'], " ")
        };
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection_or_cursor());
        // One character at a time is typing, and joins the undo step before
        // it; a space or anything longer (dictation, emoji picker) starts one.
        let mut chars = text.chars();
        let typing = matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_whitespace());
        self.edit(if typing { Run::Type } else { Run::Other }, cx, |b| {
            b.place(range.start, false);
            b.place(range.end, true);
            b.insert(&text);
        });
        self.marked = None;
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection_or_cursor());
        // A composition is one undo step, opened by its first keystroke.
        let owned = text.to_string();
        let r = range.clone();
        self.edit(Run::Type, cx, |b| {
            b.place(r.start, false);
            b.place(r.end, true);
            b.insert(&owned);
        });
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        if let Some(sel) = selected {
            self.buf
                .place(range.start + utf16_to_byte(text, sel.start), false);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let start = self.byte_of_utf16(range.start);
        let p = layout.position_for_index(start)?;
        Some(Bounds::new(p, size(px(1.), layout.line_height())))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let ix = self.index_at(point)?;
        Some(self.to_utf16(ix))
    }
}

impl Render for Field {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // While its menu is open the field still reads as focused: the
        // selection the menu acts on stays highlighted.
        let focused = self.focus.is_focused(window) || self.menu.is_some();
        let entity = cx.entity();
        let focus = self.focus.clone();
        let text = self.buf.text().to_string();
        let line_px = u(if self.multiline { 20. } else { 18. });
        self.layout = None;

        let body = if text.is_empty() || (!focused && self.display.is_some()) {
            let (shown, color) = match (&self.display, text.is_empty()) {
                (Some(d), false) => (d.clone(), theme::text_2()),
                _ => (self.placeholder.clone(), theme::dim()),
            };
            div()
                .relative()
                .w_full()
                .child(
                    div()
                        .w_full()
                        .one_line()
                        .text_color(rgb(color))
                        .child(shown),
                )
                .when(focused, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w(u(1.5))
                            .h(line_px)
                            .bg(rgb(theme::accent())),
                    )
                })
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, cx| {
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, entity),
                                cx,
                            )
                        },
                    )
                    .absolute()
                    .size_full(),
                )
                .into_any_element()
        } else {
            let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
            if let (true, Some((s, e))) = (focused, self.buf.selection()) {
                highlights.push((
                    s..e,
                    HighlightStyle {
                        background_color: Some(rgba(theme::alpha(theme::text(), 0x38)).into()),
                        ..Default::default()
                    },
                ));
            }
            if let Some(m) = self.marked.clone() {
                highlights.push((
                    m,
                    HighlightStyle {
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(rgb(theme::text()).into()),
                            wavy: false,
                        }),
                        ..Default::default()
                    },
                ));
            }
            highlights.sort_by_key(|(r, _)| r.start);
            let highlights = theme::safe_highlights(&text, highlights);
            let styled = StyledText::new(text).with_highlights(highlights);
            let layout = styled.layout().clone();
            let cursor = self.buf.cursor();
            let scroll_x = self.scroll_x;
            let multiline = self.multiline;
            div()
                .relative()
                .w_full()
                .overflow_hidden()
                .child(
                    div()
                        .when(!multiline, |d| d.whitespace_nowrap().ml(-scroll_x))
                        .child(styled),
                )
                .child(
                    canvas(
                        move |bounds, _, cx| {
                            let caret = layout.position_for_index(cursor);
                            entity.update(cx, |f, cx| {
                                f.layout = Some(layout.clone());
                                // A multi-line field scrolls to keep the caret
                                // in view, but only when the caret moved, so it
                                // never fights a scroll by hand.
                                if let (true, Some(p)) = (multiline, caret) {
                                    if f.shown_caret != Some(cursor) {
                                        f.shown_caret = Some(cursor);
                                        let view = f.scroll.bounds();
                                        let lh = layout.line_height();
                                        let pad = px(6.);
                                        let mut off = f.scroll.offset();
                                        if p.y < view.top() + pad {
                                            off.y += view.top() + pad - p.y;
                                        } else if p.y + lh > view.bottom() - pad {
                                            off.y -= p.y + lh - (view.bottom() - pad);
                                        }
                                        if off != f.scroll.offset() {
                                            f.scroll.set_offset(off);
                                            cx.notify();
                                        }
                                    }
                                }
                                // Keep the caret inside a single-line field.
                                if let (false, Some(p)) = (multiline, caret) {
                                    let x = p.x - bounds.origin.x;
                                    let room = bounds.size.width - px(2.);
                                    let next = if x > room {
                                        f.scroll_x + (x - room)
                                    } else if x < px(0.) {
                                        (f.scroll_x + x).max(px(0.))
                                    } else {
                                        f.scroll_x
                                    };
                                    if next != f.scroll_x {
                                        f.scroll_x = next;
                                        cx.notify();
                                    }
                                }
                            });
                            (caret, entity.clone())
                        },
                        move |bounds, (caret, entity), window, cx| {
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, entity),
                                cx,
                            );
                            if let (true, Some(p)) = (focused, caret) {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(p.x, p.y),
                                        size(px(1.5), window.line_height()),
                                    ),
                                    rgb(theme::accent()),
                                ));
                            }
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
                .into_any_element()
        };

        div()
            .id("field")
            .track_focus(&self.focus)
            .key_context("Field")
            .flex()
            .flex_1()
            .min_w_0()
            .when(!self.multiline, |d| d.items_center().h(u(28.)).px(u(10.)))
            .when(self.multiline, |d| {
                d.items_start()
                    .py(u(6.))
                    .px(u(6.))
                    .min_h(u(44.))
                    .max_h(u(240.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
            })
            .when(self.editor, |d| d.min_h(u(132.)).px(u(10.)).py(u(8.)))
            .when(!self.bare, |d| {
                d.rounded(u(8.))
                    .bg(rgb(if focused {
                        theme::surface_hover()
                    } else {
                        theme::surface()
                    }))
                    .border_1()
                    .border_color(rgba(if focused {
                        theme::input_edge()
                    } else {
                        0xffffff00
                    }))
            })
            .text_size(u(13.))
            .line_height(line_px)
            .text_color(rgb(theme::text()))
            .cursor_text()
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_action(cx.listener(|f, _: &SelectAll, _, cx| f.select_all(cx)))
            .on_action(cx.listener(|f, _: &Copy, _, cx| {
                if let Some(t) = f.buf.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t.to_string()));
                }
            }))
            .on_action(cx.listener(|f, _: &Cut, _, cx| {
                if let Some(t) = f.buf.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(t.to_string()));
                    f.edit(Run::Other, cx, |b| b.backspace());
                }
            }))
            .on_action(cx.listener(|f, _: &Paste, _, cx| {
                if f.attachments {
                    if let Some(p) = pasteboard::read() {
                        cx.emit(FieldEvent::Attach(p));
                        return;
                    }
                }
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let t = if f.multiline {
                        t.replace("\r\n", "\n")
                    } else {
                        t.replace(['\r', '\n'], " ")
                    };
                    f.edit(Run::Other, cx, |b| b.insert(&t));
                }
            }))
            .on_action(cx.listener(|f, _: &Undo, _, cx| f.undo(cx)))
            .on_action(cx.listener(|f, _: &Redo, _, cx| f.redo(cx)))
            .child(body)
            .children(self.menu.as_ref().map(menu::Open::render))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_map_onto_utf8_bytes() {
        let s = "aé😀b";
        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 1), 1); // after 'a'
        assert_eq!(utf16_to_byte(s, 2), 3); // after 'é' (2 bytes, 1 unit)
        assert_eq!(utf16_to_byte(s, 4), 7); // after '😀' (4 bytes, 2 units)
        assert_eq!(utf16_to_byte(s, 99), s.len());
    }
}
