//! A text buffer for single-line fields: cursor, selection, word motions.
//!
//! Copied whole from yourcode with its tests; the multi-line motions are unused
//! by a single-line field but kept so the model stays one tested unit.
//!
//! gpui ships no text input, so this is the editing model the composer drives.
//! It is deliberately UI-free and exhaustively tested — cursor bugs are the kind
//! that are obvious to a user and invisible in a screenshot.
//!
//! Offsets are **byte** offsets into a UTF-8 string, and every motion lands on a
//! char boundary. Column arithmetic is in `char`s, because that is what a reader
//! perceives as "one position".

#![allow(dead_code)]

/// What a motion does to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Collapse the selection to the new position.
    Collapse,
    /// Keep the anchor and extend to the new position.
    Extend,
}

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    text: String,
    /// Insertion point, in bytes.
    cursor: usize,
    /// Other end of the selection. Equal to `cursor` when nothing is selected.
    anchor: usize,
    /// Column (in chars) to aim for during vertical movement, so moving down
    /// through a short line and back does not lose the original column.
    goal_col: Option<usize>,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Selected range as `(start, end)` in bytes, or `None` when collapsed.
    pub fn selection(&self) -> Option<(usize, usize)> {
        (self.cursor != self.anchor)
            .then(|| (self.cursor.min(self.anchor), self.cursor.max(self.anchor)))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(s, e)| &self.text[s..e])
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = 0;
        self.goal_col = None;
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        self.goal_col = None;
    }

    // -- editing -------------------------------------------------------------

    /// Insert text at the cursor, replacing any selection.
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.goal_col = None;
    }

    /// Returns true when something was removed.
    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((s, e)) => {
                self.text.replace_range(s..e, "");
                self.cursor = s;
                self.anchor = s;
                self.goal_col = None;
                true
            }
            None => false,
        }
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
            self.anchor = prev;
        }
        self.goal_col = None;
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(next) = self.next_boundary(self.cursor) {
            self.text.replace_range(self.cursor..next, "");
        }
        self.goal_col = None;
    }

    /// ctrl/alt+backspace — delete the word before the cursor.
    pub fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.word_left(self.cursor);
        self.text.replace_range(to..self.cursor, "");
        self.cursor = to;
        self.anchor = to;
        self.goal_col = None;
    }

    /// alt/ctrl+delete — delete the word after the cursor.
    pub fn delete_word_right(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.word_right(self.cursor);
        self.text.replace_range(self.cursor..to, "");
        self.goal_col = None;
    }

    /// ctrl+u — delete to the start of the current line.
    pub fn delete_to_line_start(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.line_start(self.cursor);
        self.text.replace_range(to..self.cursor, "");
        self.cursor = to;
        self.anchor = to;
        self.goal_col = None;
    }

    /// ctrl+k — delete to the end of the current line.
    pub fn delete_to_line_end(&mut self) {
        if self.delete_selection() {
            return;
        }
        let to = self.line_end(self.cursor);
        // At the end of a line, kill the newline itself so repeated presses
        // join lines rather than doing nothing.
        let to = if to == self.cursor {
            self.next_boundary(self.cursor).unwrap_or(to)
        } else {
            to
        };
        self.text.replace_range(self.cursor..to, "");
        self.goal_col = None;
    }

    // -- motion --------------------------------------------------------------

    pub fn move_left(&mut self, m: Move) {
        // Collapsing with a selection active goes to its near edge, which is
        // what every editor does and what users expect.
        if m == Move::Collapse {
            if let Some((s, _)) = self.selection() {
                self.set_cursor(s, m);
                return;
            }
        }
        let to = self.prev_boundary(self.cursor).unwrap_or(0);
        self.set_cursor(to, m);
    }

    pub fn move_right(&mut self, m: Move) {
        if m == Move::Collapse {
            if let Some((_, e)) = self.selection() {
                self.set_cursor(e, m);
                return;
            }
        }
        let to = self.next_boundary(self.cursor).unwrap_or(self.text.len());
        self.set_cursor(to, m);
    }

    pub fn move_word_left(&mut self, m: Move) {
        let to = self.word_left(self.cursor);
        self.set_cursor(to, m);
    }

    pub fn move_word_right(&mut self, m: Move) {
        let to = self.word_right(self.cursor);
        self.set_cursor(to, m);
    }

    pub fn move_line_start(&mut self, m: Move) {
        let to = self.line_start(self.cursor);
        self.set_cursor(to, m);
    }

    pub fn move_line_end(&mut self, m: Move) {
        let to = self.line_end(self.cursor);
        self.set_cursor(to, m);
    }

    pub fn move_doc_start(&mut self, m: Move) {
        self.set_cursor(0, m);
    }

    pub fn move_doc_end(&mut self, m: Move) {
        self.set_cursor(self.text.len(), m);
    }

    /// Vertical movement over logical lines. Returns false at the boundary, so
    /// the caller can fall back to something else (e.g. history navigation).
    pub fn move_up(&mut self, m: Move) -> bool {
        let (line, col) = self.line_col(self.cursor);
        if line == 0 {
            return false;
        }
        let goal = self.goal_col.unwrap_or(col);
        let to = self.offset_at(line - 1, goal);
        self.set_cursor(to, m);
        self.goal_col = Some(goal);
        true
    }

    pub fn move_down(&mut self, m: Move) -> bool {
        let (line, col) = self.line_col(self.cursor);
        if line + 1 >= self.line_count() {
            return false;
        }
        let goal = self.goal_col.unwrap_or(col);
        let to = self.offset_at(line + 1, goal);
        self.set_cursor(to, m);
        self.goal_col = Some(goal);
        true
    }

    /// Put the caret at a byte offset, as a click does. `extend` keeps the
    /// anchor so a shift-click or a drag grows the selection.
    pub fn place(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.text.len());
        // Clamp onto a char boundary: an offset from hit-testing is derived from
        // shaped glyph positions, which should already align, but a mismatch
        // must not panic on the next slice.
        let mut at = offset;
        while at > 0 && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        self.cursor = at;
        if !extend {
            self.anchor = at;
        }
        self.goal_col = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
        self.goal_col = None;
    }

    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// Put back an earlier state wholesale (undo / redo).
    pub fn restore(&mut self, text: String, cursor: usize, anchor: usize) {
        self.text = text;
        self.cursor = cursor.min(self.text.len());
        self.anchor = anchor.min(self.text.len());
        self.goal_col = None;
    }

    /// Select the word under `offset`, as a double-click does. On whitespace
    /// or punctuation it selects just that character.
    pub fn select_word_at(&mut self, offset: usize) {
        if self.text.is_empty() {
            return;
        }
        // Past the end, the word clicked is the one just before it.
        let offset = if offset >= self.text.len() {
            self.prev_boundary(self.text.len()).unwrap_or(0)
        } else {
            offset
        };
        if is_word_sep(self.char_at(offset)) {
            self.anchor = offset;
            self.cursor = self.next_boundary(offset).unwrap_or(self.text.len());
        } else {
            let mut s = offset;
            while let Some(p) = self.prev_boundary(s) {
                if is_word_sep(self.char_at(p)) {
                    break;
                }
                s = p;
            }
            let mut e = offset;
            while e < self.text.len() && !is_word_sep(self.char_at(e)) {
                e = self.next_boundary(e).unwrap_or(self.text.len());
            }
            self.anchor = s;
            self.cursor = e;
        }
        self.goal_col = None;
    }

    /// Select the whole line under `offset`, as a triple-click does.
    pub fn select_line_at(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        self.anchor = self.line_start(offset);
        self.cursor = self.line_end(offset);
        self.goal_col = None;
    }

    fn set_cursor(&mut self, to: usize, m: Move) {
        self.cursor = to;
        if m == Move::Collapse {
            self.anchor = to;
        }
        self.goal_col = None;
    }

    // -- geometry ------------------------------------------------------------

    /// Zero-based (line, column-in-chars) of a byte offset.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let before = &self.text[..offset];
        let line = before.matches('\n').count();
        let col = before[before.rfind('\n').map(|i| i + 1).unwrap_or(0)..]
            .chars()
            .count();
        (line, col)
    }

    pub fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.split('\n')
    }

    /// Byte offset of (line, column), clamped to the line's length.
    fn offset_at(&self, line: usize, col: usize) -> usize {
        let mut start = 0;
        for (i, l) in self.text.split('\n').enumerate() {
            if i == line {
                let take = l.char_indices().nth(col).map(|(b, _)| b).unwrap_or(l.len());
                return start + take;
            }
            start += l.len() + 1; // + '\n'
        }
        self.text.len()
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.text.len())
    }

    fn prev_boundary(&self, offset: usize) -> Option<usize> {
        (offset > 0).then(|| {
            let mut i = offset - 1;
            while i > 0 && !self.text.is_char_boundary(i) {
                i -= 1;
            }
            i
        })
    }

    fn next_boundary(&self, offset: usize) -> Option<usize> {
        (offset < self.text.len()).then(|| {
            let mut i = offset + 1;
            while i < self.text.len() && !self.text.is_char_boundary(i) {
                i += 1;
            }
            i
        })
    }

    /// Word-left: skip any whitespace immediately behind the cursor, then skip
    /// the word itself. Matches the behaviour of every editor's alt+left.
    fn word_left(&self, from: usize) -> usize {
        let mut i = from;
        while let Some(p) = self.prev_boundary(i) {
            if is_word_sep(self.char_at(p)) {
                i = p;
            } else {
                break;
            }
        }
        while let Some(p) = self.prev_boundary(i) {
            if is_word_sep(self.char_at(p)) {
                break;
            }
            i = p;
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let mut i = from;
        while i < self.text.len() && is_word_sep(self.char_at(i)) {
            i = self.next_boundary(i).unwrap_or(self.text.len());
        }
        while i < self.text.len() && !is_word_sep(self.char_at(i)) {
            i = self.next_boundary(i).unwrap_or(self.text.len());
        }
        i
    }

    fn char_at(&self, offset: usize) -> char {
        self.text[offset..].chars().next().unwrap_or(' ')
    }

    // -- token under cursor (for `@` completion) -----------------------------

    /// The `@…` token the cursor sits in, as `(start_offset, query)`.
    ///
    /// Used to drive file completion: the trigger must be at a word start so an
    /// email address or a `user@host` string does not open the picker.
    pub fn mention_token(&self) -> Option<(usize, &str)> {
        let line_start = self.line_start(self.cursor);
        let before = &self.text[line_start..self.cursor];
        let at = before.rfind('@')? + line_start;
        // Preceded by nothing or whitespace only.
        if let Some(p) = self.prev_boundary(at) {
            if !self.char_at(p).is_whitespace() {
                return None;
            }
        }
        let query = &self.text[at + 1..self.cursor];
        // A space ends the token; the picker closes rather than matching across
        // words.
        if query.contains(char::is_whitespace) {
            return None;
        }
        Some((at, query))
    }

    /// The `/command` being typed: a `/` at the very start of the text, no
    /// space typed after it yet.
    pub fn slash_token(&self) -> Option<(usize, &str)> {
        let before = &self.text[..self.cursor];
        let rest = before.strip_prefix('/')?;
        (!rest.contains(char::is_whitespace)).then_some((0, rest))
    }

    /// The `$skill` being typed: a `$` at a word's start on this line.
    pub fn dollar_token(&self) -> Option<(usize, &str)> {
        let line_start = self.line_start(self.cursor);
        let before = &self.text[line_start..self.cursor];
        let at = before.rfind('$')? + line_start;
        if let Some(p) = self.prev_boundary(at) {
            if !self.char_at(p).is_whitespace() {
                return None;
            }
        }
        let query = &self.text[at + 1..self.cursor];
        (!query.contains(char::is_whitespace)).then_some((at, query))
    }

    /// Replace the token from `start` to the cursor with `with` and a space.
    pub fn replace_token(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.anchor = start;
        self.insert(&format!("{with} "));
    }

    /// Replace an `@…` token with a concrete path plus a trailing space.
    pub fn replace_mention(&mut self, start: usize, path: &str) {
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.anchor = start;
        self.insert(&format!("@{path} "));
    }
}

/// Word separators. Path characters (`/`, `.`, `-`, `_`) count as *word* chars,
/// so alt+backspace over `src/main.rs` removes the whole path rather than
/// stopping at each punctuation mark.
fn is_word_sep(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`' | '='
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, cursor: usize) -> TextInput {
        let mut i = TextInput::new();
        i.set_text(text);
        i.cursor = cursor;
        i.anchor = cursor;
        i
    }

    #[test]
    fn insert_and_backspace_at_the_cursor() {
        let mut i = TextInput::new();
        i.insert("hello");
        assert_eq!(i.text(), "hello");
        i.move_left(Move::Collapse);
        i.insert("X");
        assert_eq!(i.text(), "hellXo");
        i.backspace();
        assert_eq!(i.text(), "hello");
        assert_eq!(i.cursor(), 4);
    }

    #[test]
    fn backspace_at_start_and_delete_at_end_are_no_ops() {
        let mut i = at("ab", 0);
        i.backspace();
        assert_eq!(i.text(), "ab");
        let mut i = at("ab", 2);
        i.delete_forward();
        assert_eq!(i.text(), "ab");
    }

    #[test]
    fn motions_never_split_a_multibyte_char() {
        let mut i = at("héllo wörld", 0);
        for _ in 0..20 {
            i.move_right(Move::Collapse);
            assert!(i.text().is_char_boundary(i.cursor()));
        }
        for _ in 0..20 {
            i.move_left(Move::Collapse);
            assert!(i.text().is_char_boundary(i.cursor()));
        }
    }

    #[test]
    fn backspace_removes_a_whole_multibyte_char() {
        let mut i = at("café", "café".len());
        i.backspace();
        assert_eq!(i.text(), "caf");
    }

    #[test]
    fn emoji_are_deleted_whole_not_byte_by_byte() {
        let mut i = at("hi 🚀", "hi 🚀".len());
        i.backspace();
        assert_eq!(i.text(), "hi ");
    }

    #[test]
    fn word_left_skips_trailing_space_then_the_word() {
        let mut i = at("cargo build ", 12);
        i.move_word_left(Move::Collapse);
        assert_eq!(&i.text()[..i.cursor()], "cargo ");
        i.move_word_left(Move::Collapse);
        assert_eq!(i.cursor(), 0);
    }

    #[test]
    fn word_right_stops_after_each_word() {
        let mut i = at("one two three", 0);
        i.move_word_right(Move::Collapse);
        assert_eq!(&i.text()[..i.cursor()], "one");
        i.move_word_right(Move::Collapse);
        assert_eq!(&i.text()[..i.cursor()], "one two");
    }

    #[test]
    fn delete_word_left_removes_a_whole_path() {
        // Path punctuation is word-internal, so this removes the whole thing.
        let mut i = at("open src/main.rs", 16);
        i.delete_word_left();
        assert_eq!(i.text(), "open ");
    }

    #[test]
    fn delete_word_right_removes_the_next_word_only() {
        let mut i = at("one two", 0);
        i.delete_word_right();
        assert_eq!(i.text(), " two");
    }

    #[test]
    fn line_start_and_end_operate_within_the_current_line() {
        let mut i = at("first\nsecond", 8);
        i.move_line_start(Move::Collapse);
        assert_eq!(i.cursor(), 6);
        i.move_line_end(Move::Collapse);
        assert_eq!(i.cursor(), 12);
    }

    #[test]
    fn delete_to_line_start_and_end() {
        let mut i = at("keep XXX", 8);
        i.delete_to_line_start();
        assert_eq!(i.text(), "");

        let mut i = at("keep XXX", 4);
        i.delete_to_line_end();
        assert_eq!(i.text(), "keep");
    }

    #[test]
    fn delete_to_line_end_at_eol_joins_the_next_line() {
        let mut i = at("a\nb", 1);
        i.delete_to_line_end();
        assert_eq!(i.text(), "ab", "repeated ctrl+k must make progress");
    }

    #[test]
    fn vertical_movement_keeps_the_goal_column() {
        // Down through a short line and back must return to column 5.
        let mut i = at("aaaaaaaa\nbb\ncccccccc", 5);
        assert!(i.move_down(Move::Collapse));
        assert_eq!(i.line_col(i.cursor()), (1, 2), "clamped to the short line");
        assert!(i.move_down(Move::Collapse));
        assert_eq!(
            i.line_col(i.cursor()),
            (2, 5),
            "goal column restored after a short line"
        );
    }

    #[test]
    fn vertical_movement_reports_the_boundary() {
        let mut i = at("only line", 0);
        assert!(!i.move_up(Move::Collapse));
        assert!(!i.move_down(Move::Collapse));
    }

    #[test]
    fn extending_selection_keeps_the_anchor() {
        let mut i = at("hello world", 0);
        i.move_word_right(Move::Extend);
        assert_eq!(i.selected_text(), Some("hello"));
        i.move_word_right(Move::Extend);
        assert_eq!(i.selected_text(), Some("hello world"));
    }

    #[test]
    fn collapsing_left_goes_to_the_near_edge_of_the_selection() {
        let mut i = at("hello", 0);
        i.move_word_right(Move::Extend);
        assert_eq!(i.selection(), Some((0, 5)));
        i.move_left(Move::Collapse);
        assert_eq!(i.cursor(), 0, "left collapses to the start, not start-1");

        let mut i = at("hello", 0);
        i.move_word_right(Move::Extend);
        i.move_right(Move::Collapse);
        assert_eq!(i.cursor(), 5, "right collapses to the end");
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut i = at("hello world", 0);
        i.move_word_right(Move::Extend);
        i.insert("bye");
        assert_eq!(i.text(), "bye world");
        assert!(i.selection().is_none());
    }

    #[test]
    fn backspace_deletes_the_selection_without_eating_a_neighbour() {
        let mut i = at("hello world", 0);
        i.move_word_right(Move::Extend);
        i.backspace();
        assert_eq!(i.text(), " world");
    }

    #[test]
    fn place_moves_the_caret_and_collapses_by_default() {
        let mut i = at("hello world", 0);
        i.place(6, false);
        assert_eq!(i.cursor(), 6);
        assert!(i.selection().is_none());
    }

    #[test]
    fn place_with_extend_selects_from_the_anchor() {
        let mut i = at("hello world", 0);
        i.place(5, true);
        assert_eq!(i.selected_text(), Some("hello"));
    }

    #[test]
    fn place_clamps_past_the_end_and_onto_char_boundaries() {
        let mut i = at("café", 0);
        i.place(9999, false);
        assert_eq!(i.cursor(), i.text().len());
        // Byte 4 is inside the 'é'; placing there must land on a boundary.
        i.place(4, false);
        assert!(i.text().is_char_boundary(i.cursor()));
    }

    #[test]
    fn select_all_then_type_clears_everything() {
        let mut i = at("some long text", 3);
        i.select_all();
        i.insert("x");
        assert_eq!(i.text(), "x");
    }

    #[test]
    fn mention_token_is_found_at_a_word_start() {
        let i = at("look at @src/ma", 15);
        assert_eq!(i.mention_token(), Some((8, "src/ma")));

        let i = at("@", 1);
        assert_eq!(i.mention_token(), Some((0, "")));
    }

    #[test]
    fn an_email_like_string_does_not_trigger_the_picker() {
        let i = at("mail me@example.com", 19);
        assert_eq!(
            i.mention_token(),
            None,
            "`@` must be at a word start to trigger completion"
        );
    }

    #[test]
    fn a_space_ends_the_mention_token() {
        let i = at("@src/main.rs and then", 21);
        assert_eq!(i.mention_token(), None);
    }

    #[test]
    fn replacing_a_mention_inserts_the_path_and_a_space() {
        let mut i = at("look at @src/ma", 15);
        let (start, _) = i.mention_token().unwrap();
        i.replace_mention(start, "src/main.rs");
        assert_eq!(i.text(), "look at @src/main.rs ");
        assert_eq!(i.cursor(), i.text().len());
        assert_eq!(i.mention_token(), None, "picker should close afterwards");
    }

    #[test]
    fn multiline_editing_tracks_lines_correctly() {
        let mut i = TextInput::new();
        i.insert("one");
        i.insert("\n");
        i.insert("two");
        assert_eq!(i.line_count(), 2);
        assert_eq!(i.lines().collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(i.line_col(i.cursor()), (1, 3));
    }

    #[test]
    fn every_offset_in_unicode_text_is_a_valid_line_col_roundtrip() {
        let i = at("héllo\nwörld 🚀\nlast", 0);
        for off in 0..=i.text().len() {
            if !i.text().is_char_boundary(off) {
                continue;
            }
            let (l, col) = i.line_col(off);
            assert_eq!(i.offset_at(l, col), off, "roundtrip failed at {off}");
        }
    }

    #[test]
    fn double_click_selects_the_word_and_triple_click_the_line() {
        let mut i = TextInput::new();
        i.set_text("open src/main.rs now\nnext line");
        i.select_word_at(7);
        assert_eq!(i.selected_text(), Some("src/main.rs"));
        i.select_word_at(4);
        assert_eq!(i.selected_text(), Some(" "));
        i.select_word_at(999);
        assert_eq!(i.selected_text(), Some("line"));
        i.select_line_at(2);
        assert_eq!(i.selected_text(), Some("open src/main.rs now"));
        i.select_line_at(25);
        assert_eq!(i.selected_text(), Some("next line"));
    }
}
