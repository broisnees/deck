//! A terminal pane: a real PTY (see the `term` crate), painted directly.
//!
//! Glyphs are placed by column, never by shaping a whole row. A stretch of
//! plain characters (ASCII, Latin, box drawing, all exactly one Menlo cell)
//! is shaped as one line; anything else (CJK, emoji, symbols from a fallback
//! font) is shaped alone at its own column. A fallback glyph whose advance is
//! not exactly one cell can therefore never push the rest of the row off the
//! grid. Shaped lines are cached by gpui, so an unchanged row is a hash hit.
//!
//! Text arrives through the platform input handler (IME, dead keys, the emoji
//! picker); `key_down` only claims keys that become escape sequences.

use crate::field::{Field, FieldEvent};
use crate::icon::Icon;
use crate::menu::{self, Entry, Item};
use crate::pasteboard::{self, Pasted};
use crate::theme::{self, OneLine};
use crate::workspace::{
    icon_button, AskCommand, ClearTerminal, ClosePane, Copy, Find, FindNext, FindPrevious, Paste, SelectAll,
    SplitDown, SplitRight,
};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    div, fill, outline, point, px, relative, rgb, rgba, size, AnyElement, App, BorderStyle, Bounds,
    ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, ExternalPaths, FocusHandle, FontStyle, FontWeight, GlobalElementId,
    InspectorElementId, KeyDownEvent, Keystroke, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent, ShapedLine,
    SharedString, Style, Subscription, Task, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub enum TerminalEvent {
    Focused,
    /// The title, the bell or the progress changed: things the sidebar shows.
    Retitled,
    /// The program wants you: a notification it posted (OSC 9 / 777), or
    /// `None` for a plain bell.
    Alert(Option<String>),
    Exited,
    /// ⌘-click on a URL in the output.
    OpenUrl(String),
    /// A command at the shell finished: what ran, its exit status, and how
    /// long it took.
    CommandDone {
        command: Option<String>,
        exit: Option<i32>,
        took: std::time::Duration,
    },
    /// ⌃R at the shell's prompt: search the commands run before.
    History,
    /// "# …" at the prompt: write a command for this, in this folder.
    /// Write a command for `request`; `run` presses Enter once it is in.
    Ask { request: String, cwd: Option<PathBuf>, run: bool },
    /// "Fix with agent" on a block: a message for a chat in this folder.
    AskAgent { prompt: String, cwd: Option<PathBuf> },
}

pub struct TerminalPane {
    /// The command box was sent with Ctrl+Enter: the command runs once in.
    ask_runs: bool,
    /// The failed command whose overlay was closed, by block number.
    fail_dismissed: Option<u64>,
    /// The job this pane shows has exited: its output stays, with no cursor
    /// left blinking in it.
    pub ended: bool,
    /// The AI command box (⌘J): what you asked for, and whether the
    /// command is being written.
    ask: Option<(Entity<crate::field::Field>, Subscription)>,
    asking: bool,
    /// What a program in the pane last said with `deck report`: its state
    /// (working, blocked, done, error) and message.
    pub reported: Option<(String, String)>,
    term: term::Terminal,
    /// The shell's own process when it lives in the background service and
    /// this pane only shows it: what process scans should look under.
    pid_override: Option<u32>,
    pub focus: FocusHandle,
    pub title: SharedString,
    /// When output last arrived; agents are "working" while this is recent.
    pub last_output: Instant,
    /// Set by BEL or a notification, cleared when the pane is looked at:
    /// "this one wants you".
    pub bell: bool,
    /// What the program said when it asked for you, if it said anything.
    pub alert: Option<String>,
    /// OSC 9;4 progress, once the program has reported any: `Some` while it
    /// works.
    progress: Option<Option<u8>>,
    /// Its percentage, when it gives one.
    progress_pct: Option<u8>,
    exited: bool,
    look: Look,
    cell_w: f32,
    /// Where the grid was last painted, for turning mouse positions into cells.
    origin: Point<Pixels>,
    /// A press that has not moved yet; a drag turns it into a selection.
    press: Option<(usize, usize, bool)>,
    /// The button held down while the program is receiving mouse reports.
    reporting: Option<term::MouseButton>,
    /// A press held back from Claude Code: sent as a click on release if
    /// the mouse stayed put, or dropped for Deck's own selection if it
    /// moved. (Claude Code turns any drag into a selection that stays
    /// highlighted grey while you scroll, until a key is pressed.)
    held_click: Option<(Point<Pixels>, gpui::Modifiers)>,
    /// Last cell a motion report was sent for, so moves within a cell are free.
    last_motion: Option<(usize, usize)>,
    /// Trackpads send fractions of a line; the remainder carries over.
    scroll_acc: f32,
    /// IME composition in progress, drawn at the cursor until committed.
    marked: Option<String>,
    /// Shaped rows from the last frame; see `RowCache`.
    rows: Vec<RowCache>,
    /// The find bar (⌘F), while open.
    find: Option<FindBar>,
    /// The right-click menu, while open.
    menu: Option<menu::Open>,
    /// The next snapshot must rebuild every row (the palette changed).
    restyle: bool,
    /// Whether a blinking cursor is in its visible half.
    blink_on: bool,
    /// When the user last typed: the cursor stays solid while they do.
    typed: Instant,
    /// The command block under the mouse, for its actions.
    hover_block: Option<u64>,
    /// The line at the prompt as the shell last reported it: cursor (in
    /// characters) and text. `None` while a command runs.
    line: Option<(usize, String)>,
    /// What the word at the cursor could become, for the line it was worked
    /// out for.
    suggest: Option<Suggest>,
    /// Esc closed the menu: the line before the word it was closed on, so it
    /// stays shut while that word is typed.
    hushed: Option<String>,
    _complete: Option<Task<()>>,
    _blink: Option<Task<()>>,
    _wake: Task<()>,
}

/// How terminals look and type, from Settings. Every pane gets the same one.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub family: SharedString,
    /// The font size on screen: the setting times the app zoom.
    pub font_px: f32,
    /// Line height as a multiple of `font_px`.
    pub line: f32,
    pub palette: term::Palette,
    pub cursor_color: u32,
    /// The cursor until a program asks for another (DECSCUSR).
    pub cursor: term::CursorShape,
    pub blink: bool,
    /// Option sends Esc+key (Meta) instead of typing Option-characters.
    pub option_meta: bool,
    /// Programs' coloured backgrounds (diffs, highlights) drawn as a tint.
    pub soften: bool,
    /// The completion menu at a zsh prompt, as you type.
    pub complete: bool,
}

struct Suggest {
    line: String,
    cursor: usize,
    found: crate::complete::Found,
    sel: usize,
}

/// Rows the completion menu shows at once; the rest scroll into view.
const MENU_ROWS: usize = 8;

/// Half a blink cycle, as macOS text fields use.
const BLINK: std::time::Duration = std::time::Duration::from_millis(530);

struct FindBar {
    field: Entity<Field>,
    /// The query last searched for: the field also notifies on caret moves
    /// and focus changes, which must not search again.
    query: String,
    found: bool,
    _subs: [Subscription; 2],
}

impl EventEmitter<TerminalEvent> for TerminalPane {}

pub fn line_height(font_px: f32, factor: f32) -> f32 {
    (font_px * factor).round()
}

impl TerminalPane {
    pub fn spawn(
        cwd: &Path,
        command: Option<&str>,
        env: HashMap<String, String>,
        engine: term::Engine,
    ) -> anyhow::Result<term::Terminal> {
        let mut spawn = match command {
            Some(c) => term::Spawn::command(c, cwd),
            None => term::Spawn::shell(cwd),
        };
        spawn.env.extend(env);
        spawn.engine = engine;
        term::Terminal::spawn(spawn, 80, 24)
    }

    pub fn new(
        term: term::Terminal,
        title: String,
        look: Look,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Woken by the PTY thread, so output reaches the screen on the next
        // frame instead of the next poll tick.
        let mut wakes = term.wakeups();
        let wake = cx.spawn(async move |this, cx| {
            while wakes.next().await.is_some() {
                let alive = this.update(cx, |p, cx| {
                    p.term.take_wake();
                    p.last_output = Instant::now();
                    // A selection the program has since redrawn over (or
                    // stretched by scrolling its own region) is no longer
                    // what the user selected; drop it rather than show it.
                    // Not mid-drag: the hand is still choosing it.
                    if !mouse_held() {
                        p.term.prune_selection();
                    }
                    for ev in p.term.drain_events() {
                        match ev {
                            term::TermEvent::Title(t) if !t.is_empty() && t != p.title.as_ref() => {
                                p.title = t.into();
                                cx.emit(TerminalEvent::Retitled);
                            }
                            term::TermEvent::Bell => {
                                p.bell = true;
                                cx.emit(TerminalEvent::Alert(None));
                                cx.emit(TerminalEvent::Retitled);
                            }
                            term::TermEvent::Notify { title, body } => {
                                let said = match title {
                                    Some(t) => format!("{t}: {body}"),
                                    None => body,
                                };
                                p.bell = true;
                                p.alert = Some(said.clone());
                                cx.emit(TerminalEvent::Alert(Some(said)));
                                cx.emit(TerminalEvent::Retitled);
                            }
                            term::TermEvent::Progress(state, pct) => {
                                p.progress_pct = pct;
                                if p.progress != Some(state) {
                                    p.progress = Some(state);
                                    cx.emit(TerminalEvent::Retitled);
                                }
                            }
                            term::TermEvent::Exited(_) => {
                                p.exited = true;
                                cx.emit(TerminalEvent::Exited);
                            }
                            term::TermEvent::Report(state, msg) => {
                                p.reported = (state != "clear").then(|| (state.clone(), msg.clone()));
                                if state == "blocked" {
                                    let said = if msg.is_empty() { None } else { Some(msg.clone()) };
                                    cx.emit(TerminalEvent::Alert(said));
                                }
                                cx.emit(TerminalEvent::Retitled);
                            }
                            term::TermEvent::Ask(request) => {
                                let cwd = p.term.shell_cwd().map(PathBuf::from);
                                cx.emit(TerminalEvent::Ask { request, cwd, run: false });
                            }
                            term::TermEvent::Line(line) => {
                                if line.is_none() {
                                    p.hushed = None;
                                }
                                p.line = line;
                                p.refresh_completions(cx);
                            }
                            term::TermEvent::CommandDone { command, exit, took, .. } => {
                                cx.emit(TerminalEvent::CommandDone { command, exit, took });
                            }
                            _ => {}
                        }
                    }
                    cx.notify();
                    !p.exited
                });
                if !matches!(alive, Ok(true)) {
                    return;
                }
            }
        });

        term.set_cursor_default(look.cursor, look.blink);
        Self {
            ask_runs: false,
            fail_dismissed: None,
            ended: false,
            ask: None,
            asking: false,
            reported: None,
            pid_override: None,
            term,
            focus: cx.focus_handle(),
            title: title.into(),
            last_output: Instant::now(),
            bell: false,
            alert: None,
            progress: None,
            progress_pct: None,
            exited: false,
            cell_w: cell_width(window, &look.family, look.font_px),
            look,
            origin: Point::default(),
            press: None,
            reporting: None,
            held_click: None,
            last_motion: None,
            scroll_acc: 0.,
            marked: None,
            rows: Vec::new(),
            find: None,
            menu: None,
            restyle: false,
            blink_on: true,
            typed: Instant::now(),
            hover_block: None,
            line: None,
            suggest: None,
            hushed: None,
            _complete: None,
            _blink: None,
            _wake: wake,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid_override.unwrap_or_else(|| self.term.pid())
    }

    pub fn set_pid(&mut self, pid: u32) {
        self.pid_override = Some(pid);
    }

    fn line_px(&self) -> f32 {
        line_height(self.look.font_px, self.look.line)
    }

    /// Settings or the zoom changed: re-measure the cell and redraw every row.
    pub fn set_look(&mut self, look: Look, window: &Window, cx: &mut Context<Self>) {
        if self.look == look {
            return;
        }
        if (self.look.cursor, self.look.blink) != (look.cursor, look.blink) {
            self.term.set_cursor_default(look.cursor, look.blink);
        }
        self.cell_w = cell_width(window, &look.family, look.font_px);
        self.restyle |= self.look.palette != look.palette;
        self.look = look;
        self.rows.clear();
        cx.notify();
    }

    /// Run the blink timer only while a focused cursor is meant to blink, so
    /// an idle window costs nothing.
    fn sync_blink(&mut self, blink: bool, cx: &mut Context<Self>) {
        if blink == self._blink.is_some() {
            return;
        }
        self.blink_on = true;
        self._blink = blink.then(|| {
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(BLINK).await;
                let alive = this.update(cx, |p, cx| {
                    p.blink_on = !p.blink_on || p.typed.elapsed() < BLINK;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            })
        });
    }

    /// ⌘K: drop scrollback and have the shell redraw its prompt at the top.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.term.clear_scrollback();
        self.term.write(&b"\x0c"[..]);
        cx.notify();
    }

    /// ⌘F: open the find bar, or select its query if it is already open.
    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = &self.find {
            let field = bar.field.clone();
            field.update(cx, |f, cx| f.select_all(cx));
            window.focus(&field.read(cx).focus);
            return;
        }
        let field = cx.new(|cx| Field::new("Find", cx));
        let subs = [
            // Search as you type.
            cx.observe(&field, |p, field, cx| {
                let query = field.read(cx).text().to_string();
                p.search(query, cx);
            }),
            cx.subscribe_in(
                &field,
                window,
                |p, _, ev: &FieldEvent, window, cx| match ev {
                    FieldEvent::Submit(_) => p.find_step(!window.modifiers().shift, cx),
                    FieldEvent::Cancel => p.close_find(window, cx),
                    _ => {}
                },
            ),
        ];
        window.focus(&field.read(cx).focus);
        self.find = Some(FindBar {
            field,
            query: String::new(),
            found: true,
            _subs: subs,
        });
        cx.notify();
    }

    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if bar.query != query {
            bar.found = self.term.search(&query) || query.is_empty();
            bar.query = query;
            cx.notify();
        }
    }

    /// Enter and ⌘G go up, to older output (the search starts at the
    /// bottom); with Shift, back down.
    fn find_step(&mut self, up: bool, cx: &mut Context<Self>) {
        if let Some(bar) = self.find.as_mut().filter(|b| !b.query.is_empty()) {
            bar.found = self.term.search_step(up);
            cx.notify();
        }
    }

    /// Esc: highlights go, the view stays where the last match left it.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            self.term.search_clear();
            window.focus(&self.focus);
            cx.notify();
        }
    }

    fn render_find(&self, bar: &FindBar, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .top(theme::u(theme::PANE_PAD_Y))
            .right(theme::u(theme::PANE_PAD_X))
            .w(theme::u(320.))
            .max_w(relative(1.))
            .flex()
            .items_center()
            .gap(theme::u(2.))
            .p(theme::u(4.))
            .rounded(theme::u(theme::CARD_RADIUS))
            .bg(rgb(theme::sidebar()))
            .border_1()
            .border_color(rgba(theme::hairline()))
            .cursor_default()
            // Clicks here are not grid clicks: no selection, and the field
            // keeps the focus it just took.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| {
                    cx.emit(TerminalEvent::Focused);
                    cx.stop_propagation();
                }),
            )
            .child(bar.field.clone())
            .when(!bar.found, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(theme::u(6.))
                        .text_size(theme::u(12.))
                        .text_color(rgb(theme::dim()))
                        .child("No matches"),
                )
            })
            .child(
                icon_button("find-up", Icon::ChevronUp)
                    .on_click(cx.listener(|p, _, _, cx| p.find_step(true, cx))),
            )
            .child(
                icon_button("find-down", Icon::ChevronDown)
                    .on_click(cx.listener(|p, _, _, cx| p.find_step(false, cx))),
            )
            .child(
                icon_button("find-close", Icon::Close)
                    .on_click(cx.listener(|p, _, window, cx| p.close_find(window, cx))),
            )
    }

    /// Type `text`, then press enter as a separate write. Agent TUIs treat a
    /// newline arriving inside the same burst as pasted text, not a submit.
    pub fn send(&self, text: &str, enter: bool, cx: &mut Context<Self>) {
        self.term.write(text.as_bytes().to_vec());
        if enter {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(40))
                    .await;
                let _ = this.update(cx, |p, _| p.term.write(&b"\r"[..]));
            })
            .detach();
        }
    }

    /// The visible screen as plain text, trailing blanks trimmed.
    pub fn read_text(&mut self) -> String {
        self.term.snapshot(&self.look.palette, true);
        let mut lines: Vec<String> = self
            .term
            .last()
            .rows
            .iter()
            .map(|row| row_text(row).trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn cell_at(&self, pos: Point<Pixels>) -> (usize, usize, bool) {
        let x = (f32::from(pos.x - self.origin.x) / self.cell_w).max(0.);
        let y = (f32::from(pos.y - self.origin.y) / self.line_px()).max(0.);
        let rows = self.term.rows().max(1);
        let cols = self.term.columns().max(1);
        let col = (x as usize).min(cols - 1);
        ((y as usize).min(rows - 1), col, x.fract() > 0.5)
    }

    /// The web link at a viewport cell: one a program marked (OSC 8), else
    /// a URL in the text, followed across soft wraps.
    fn link_at(&self, row: usize, col: usize) -> Option<String> {
        let web = |u: &String| u.starts_with("http://") || u.starts_with("https://");
        if let Some(u) = self.term.hyperlink_at(row, col).filter(web) {
            return Some(u);
        }
        let (line, at) = self.term.wrapped_line_at(row, col);
        url_at(&line, at)
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Mid-composition the IME owns every key, Enter and Backspace included.
        // Keys the find field passes up are not for the shell either.
        if self.marked.is_some() || !self.focus.is_focused(window) {
            return;
        }
        // ⌘↑ / ⌘↓: from prompt to prompt, as Terminal does with its marks.
        let k = &ev.keystroke;
        // The completion menu, while open, has ↑↓, ⇥ and Esc; ⏎ still runs
        // the line as typed.
        let plain = !k.modifiers.control && !k.modifiers.platform && !k.modifiers.alt && !k.modifiers.shift;
        if let Some(n) = self.shown_suggest(true).map(|s| s.found.items.len()) {
            let handled = match k.key.as_str() {
                "up" | "down" if plain => {
                    if let Some(s) = self.suggest.as_mut() {
                        s.sel = if k.key == "up" { (s.sel + n - 1) % n } else { (s.sel + 1) % n };
                    }
                    true
                }
                "tab" if plain => {
                    let sel = self.suggest.as_ref().map_or(0, |s| s.sel);
                    self.accept_completion(sel);
                    true
                }
                "escape" if plain => {
                    self.hushed = self.suggest.as_ref().map(word_lead);
                    true
                }
                _ => false,
            };
            if handled {
                self.typed = Instant::now();
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        // ⌃R at a prompt (zsh, nothing running, no full-screen program):
        // Deck's history search. Anywhere else ⌃R is the program's.
        if k.modifiers.control
            && !k.modifiers.platform
            && !k.modifiers.alt
            && !k.modifiers.shift
            && k.key == "r"
            && !self.term.last().alt_screen
            && self.term.last_block().is_some_and(|(_, b)| !b.running())
        {
            cx.emit(TerminalEvent::History);
            cx.stop_propagation();
            return;
        }
        if k.modifiers.platform
            && !k.modifiers.shift
            && !k.modifiers.alt
            && !k.modifiers.control
            && matches!(k.key.as_str(), "up" | "down")
            && self.term.has_blocks()
        {
            if self.term.jump_block(k.key == "up") {
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        self.typed = Instant::now();
        self.blink_on = true;
        let bytes = key_bytes(&ev.keystroke, self.term.app_cursor()).or_else(|| {
            self.look
                .option_meta
                .then(|| meta_bytes(&ev.keystroke))
                .flatten()
        });
        if let Some(bytes) = bytes {
            self.term.select_clear();
            self.term.write(bytes);
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Work out the completions for the line the shell just reported, off
    /// the main thread; a newer line cancels the last one's.
    fn refresh_completions(&mut self, cx: &mut Context<Self>) {
        let Some((cursor, text)) = self.line.clone().filter(|_| self.look.complete) else {
            self.suggest = None;
            self._complete = None;
            return;
        };
        let cwd = self
            .term
            .shell_cwd()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_default();
        self._complete = Some(cx.spawn(async move |this, cx| {
            let (line, at) = (text.clone(), cursor);
            let found = cx
                .background_executor()
                .spawn(async move { crate::complete::complete(&line, at, &cwd) })
                .await;
            let _ = this.update(cx, |p, cx| {
                // Keep the choice when the list starts the same way.
                let sel = match (&p.suggest, &found) {
                    (Some(old), Some(new)) => old
                        .found
                        .items
                        .get(old.sel)
                        .and_then(|it| new.items.iter().position(|n| n.insert == it.insert))
                        .unwrap_or(0),
                    _ => 0,
                };
                p.suggest = found.map(|found| Suggest { line: text, cursor, found, sel });
                cx.notify();
            });
        }));
    }

    /// The menu, when there is one to show: for the line as it is now, at a
    /// prompt, with the cursor at the end of a word.
    fn shown_suggest(&self, focused: bool) -> Option<&Suggest> {
        let s = self.suggest.as_ref()?;
        let (cursor, line) = self.line.as_ref()?;
        let snap = self.term.last();
        let at_prompt = !snap.alt_screen
            && snap.scrolled_back == 0
            && snap.cursor.is_some()
            && self.term.last_block().is_some_and(|(_, b)| !b.running());
        let word_end = line.chars().nth(*cursor).is_none_or(char::is_whitespace);
        // The list computed for the line a keystroke ago stays up while the
        // new one is worked out, when the line only grew at the cursor: it
        // used to vanish on every key and come back, a flicker per letter.
        let current = (s.cursor == *cursor && s.line == *line)
            || (*cursor > s.cursor
                && *cursor - s.cursor <= 2
                && line.starts_with(&s.line)
                && line.chars().count() == *cursor);
        (focused
            && at_prompt
            && word_end
            && current
            && (s.found.eager || !s.found.word.is_empty())
            && self.hushed.as_deref() != Some(word_lead(s).as_str())
            && self.menu.is_none()
            && self.find.is_none())
        .then_some(s)
    }

    /// Type what turns the word into item `ix`: the rest of it when it
    /// carries on from what was typed, else the word rubbed out and the item.
    fn accept_completion(&mut self, ix: usize) {
        let Some(s) = self.suggest.take() else { return };
        let Some(item) = s.found.items.get(ix) else { return };
        let word = s.found.word.as_str();
        let mut keys = match item.insert.strip_prefix(word) {
            Some(rest) => rest.to_string(),
            None => "\x7f".repeat(word.chars().count()) + &item.insert,
        };
        if item.space {
            keys.push(' ');
        }
        if !keys.is_empty() {
            self.term.write(keys.into_bytes());
        }
    }

    fn render_suggest(&self, focused: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::complete::Kind;
        let s = self.shown_suggest(focused)?;
        let (row, col) = self.term.last().cursor?;
        let lh = self.line_px();
        let fs = self.look.font_px;
        let small = (fs * 0.78).max(10.);
        let label_px = (fs * 0.92).round();
        let item_h = (fs * 1.75).round();
        let n = s.found.items.len();
        let shown = n.min(MENU_ROWS);
        let first = s.sel.saturating_sub(shown - 1).min(n - shown);
        let longest = s.found.items.iter().map(|i| i.label.chars().count()).max().unwrap_or(8);
        let has_detail = s.found.items.iter().any(|i| i.detail.as_deref().is_some_and(|d| !d.is_empty()));
        let grid_w = self.term.columns() as f32 * self.cell_w;
        // The selected item's description sits in the footer, not on every
        // row, so the list stays a clean column of names.
        // A description worth a panel: not a one-word tag ("recent",
        // "alias", "make") that says nothing the list does not.
        let detail = s
            .found
            .items
            .get(s.sel)
            .and_then(|i| i.detail.clone())
            .filter(|d| d.chars().count() > 12 && d.contains(' '));
        let _ = has_detail;
        // A badge only tells kinds apart: a list of one kind (all flags,
        // all files) goes without, rather than one icon down every row.
        let mixed = s.found.items.iter().any(|i| i.kind != s.found.items[0].kind);
        let width = (longest as f32 * self.cell_w + if mixed { 64. } else { 36. })
            .clamp(200., 420.)
            .min(grid_w.max(200.));
        let height = shown as f32 * item_h + 2.;
        let word_cols = s.found.word.chars().count();
        let left = ((col.saturating_sub(word_cols)) as f32 * self.cell_w - 10.)
            .min(grid_w - width)
            .max(0.);
        let rows = self.term.rows() as f32;
        let below = (rows - row as f32 - 1.) * lh;
        let top = if below < height + 4. && row as f32 * lh > below {
            row as f32 * lh - height - 4.
        } else {
            (row as f32 + 1.) * lh + 4.
        };
        let pal = &self.look.palette;
        let tint = |k: Kind| match k {
            Kind::Command => pal.ansi[2],
            Kind::Sub | Kind::Dir => pal.ansi[4],
            Kind::Flag | Kind::Script => pal.ansi[3],
            Kind::Branch => pal.ansi[5],
            Kind::File => theme::text_2(),
        };
        let glyph = |k: Kind| match k {
            Kind::Command => Icon::Terminal,
            Kind::Sub => Icon::ChevronRight,
            Kind::Flag => Icon::Settings,
            Kind::Dir => Icon::Folder,
            Kind::File => Icon::File,
            Kind::Branch => Icon::Git,
            Kind::Script => Icon::Bolt,
        };
        let badge = (fs * 1.15).round();
        let mut list = div().flex().flex_col();
        for (i, it) in s.found.items.iter().enumerate().skip(first).take(shown) {
            let on = i == s.sel;
            let lights: Vec<_> = it
                .label
                .char_indices()
                .enumerate()
                .filter(|(ci, _)| it.matched.contains(ci))
                .map(|(_, (b, ch))| {
                    (
                        b..b + ch.len_utf8(),
                        gpui::HighlightStyle {
                            color: Some(rgb(theme::text()).into()),
                            font_weight: Some(gpui::FontWeight::BOLD),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            list = list.child(
                div()
                    .id(("suggest", i))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .h(px(item_h))
                    .px(px(10.))
                    .when(on, |d| d.bg(rgba(theme::alpha(theme::text(), 0x14))))
                    .when(!on, |d| d.hover(|h| h.bg(rgb(theme::row_hover()))))
                    .cursor_pointer()
                    // A badge in the kind's colour when kinds are mixed:
                    // what it is, at a glance, before the name is read.
                    .when(mixed, |d| d.child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(badge))
                            .rounded(px(4.))
                            .when(on, |d| d.bg(rgba(theme::alpha(tint(it.kind), 0x30))))
                            .when(!on, |d| d.bg(rgba(theme::alpha(theme::text(), 0x0a))))
                            .child(
                                gpui::svg()
                                    .path(glyph(it.kind).path())
                                    .size(px((badge * 0.62).round()))
                                    .text_color(rgb(if on { tint(it.kind) } else { theme::dim() })),
                            ),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .one_line()
                            .font_family(self.look.family.clone())
                            .text_size(px(label_px))
                            .text_color(rgb(if on { theme::text() } else { theme::text_2() }))
                            .child(gpui::StyledText::new(it.label.clone()).with_highlights(theme::safe_highlights(&it.label, lights))),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |p, _, window, cx| {
                            cx.stop_propagation();
                            p.accept_completion(i);
                            window.focus(&p.focus);
                            cx.notify();
                        }),
                    ),
            );
        }
        // The selected item's description beside the list, wrapped and in
        // full, where a footer cut it off.
        let sel_label = s.found.items.get(s.sel).map(|i| i.label.clone()).unwrap_or_default();
        let side_w = 300f32.min((grid_w - left - width - 6.).max(0.));
        let beside = side_w >= 180.;
        let detail_box = detail.map(|d| {
            div()
                .id("suggest-detail")
                .absolute()
                .when(beside, |x| x.top(px(top)).left(px(left + width + 6.)).w(px(side_w)))
                .when(!beside, |x| x.top(px(top + height + 6.)).left(px(left)).w(px(width)))
                .flex()
                .flex_col()
                .gap(px(4.))
                .p(px(10.))
                .rounded(px(10.))
                .bg(rgb(theme::menu()))
                .border_1()
                .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                .child(
                    div()
                        .font_family(self.look.family.clone())
                        .text_size(px(label_px))
                        .text_color(rgb(theme::text()))
                        .child(sel_label),
                )
                .child(div().text_size(px(small + 1.)).line_height(px((small + 1.) * 1.45)).text_color(rgb(theme::text_2())).child(d))
        });
        Some(
            div()
                .absolute()
                .top(theme::u(theme::PANE_PAD_Y))
                .left(theme::u(theme::PANE_PAD_X))
                .child(
                    div()
                        .id("suggest-menu")
                        .absolute()
                        .top(px(top))
                        .left(px(left))
                        .w(px(width))
                        .flex()
                        .flex_col()
                        .rounded(px(10.))
                        .bg(rgb(theme::menu()))
                        .border_1()
                        .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                        .overflow_hidden()
                        .child(list),
                )
                .children(detail_box)
                .into_any_element(),
        )
    }

    fn focus_here(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        crate::browser::reclaim_keyboard(window);
        self.seen(cx);
        cx.emit(TerminalEvent::Focused);
    }

    /// The pane has been looked at: whatever it wanted you for is answered
    /// or at least known.
    pub fn seen(&mut self, cx: &mut Context<Self>) {
        if self.bell || self.alert.is_some() {
            self.bell = false;
            self.alert = None;
            cx.emit(TerminalEvent::Retitled);
        }
    }

    /// The folder the shell last said it is in (OSC 7).
    pub fn shell_cwd(&self) -> Option<PathBuf> {
        self.term.shell_cwd().map(PathBuf::from)
    }

    /// The shell's newest command and how it went, for the status bar.
    pub fn job(&self) -> Option<term::Block> {
        self.term.last_command()
    }

    /// The last `lines` lines this pane printed, scrollback included.
    pub fn tail(&self, lines: usize) -> String {
        self.term.tail_text(lines)
    }

    /// The command lines run here, newest first.
    pub fn session_commands(&self) -> Vec<String> {
        self.term.commands()
    }

    /// What this pane showed, with its colours, to show again after a
    /// relaunch (see `Workspace::save_scrollback`).
    pub fn history_ansi(&self) -> Option<String> {
        self.term.history_ansi(5000)
    }

    /// Claude Code is running here: its title rests on `✳` and turns `◐`/`◑`
    /// while it works.
    fn is_claude_code(&self) -> bool {
        matches!(self.title.chars().next(), Some('✳' | '◐' | '◑'))
    }

    /// Whether the program says it is working, when it says: OSC 9;4
    /// progress if it reports any, else Claude Code's title, which turns
    /// `◐`/`◑` while it works and rests on `✳`. `None` when it does neither.
    pub fn activity(&self) -> Option<bool> {
        if let Some((state, _)) = &self.reported {
            return Some(state == "working");
        }
        if let Some(state) = self.progress {
            return Some(state.is_some());
        }
        match self.title.chars().next() {
            Some('◐' | '◑') => Some(true),
            Some('✳') => Some(false),
            _ => None,
        }
    }

    /// Send a mouse event to the program if it asked for mouse input. Shift
    /// is the universal escape hatch: with it held, the mouse selects text.
    fn report(
        &mut self,
        button: Option<term::MouseButton>,
        action: term::MouseAction,
        pos: Point<Pixels>,
        m: &gpui::Modifiers,
    ) -> bool {
        // A release always goes out: the press it closes was already sent,
        // and swallowing it leaves the program dragging.
        if (m.shift && action != term::MouseAction::Release) || !self.term.mouse_modes().report {
            return false;
        }
        let (row, col, _) = self.cell_at(pos);
        match self.term.mouse_report(button, action, row, col, mods(m)) {
            Some(bytes) => {
                mouse_log(&format!("{action:?} {button:?} r{row} c{col} held={}", mouse_held()), &bytes);
                self.term.write(bytes);
                true
            }
            None => false,
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_here(window, cx);
        if ev.modifiers.platform {
            let (row, col, _) = self.cell_at(ev.position);
            if let Some(url) = self.link_at(row, col) {
                cx.emit(TerminalEvent::OpenUrl(url));
            }
            return;
        }
        let button = term::MouseButton::Left;
        let (row, col, right) = self.cell_at(ev.position);
        // Claude Code: hold the press until we know it is a click.
        if self.is_claude_code() && !ev.modifiers.shift && self.term.mouse_modes().report {
            self.term.select_clear();
            self.held_click = Some((ev.position, ev.modifiers));
            self.press = Some((row, col, right));
            cx.notify();
            return;
        }
        if self.report(
            Some(button),
            term::MouseAction::Press,
            ev.position,
            &ev.modifiers,
        ) {
            self.reporting = Some(button);
            return;
        }
        match ev.click_count {
            2 => self
                .term
                .select_start(row, col, right, term::SelectMode::Word),
            3 => self
                .term
                .select_start(row, col, right, term::SelectMode::Line),
            _ => {
                self.term.select_clear();
                self.press = Some((row, col, right));
            }
        }
        cx.notify();
    }

    fn on_other_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_here(window, cx);
        // Taken by the find bar's field.
        if window.default_prevented() {
            return;
        }
        let button = match ev.button {
            MouseButton::Right => term::MouseButton::Right,
            MouseButton::Middle => term::MouseButton::Middle,
            _ => return,
        };
        if self.report(
            Some(button),
            term::MouseAction::Press,
            ev.position,
            &ev.modifiers,
        ) {
            self.reporting = Some(button);
        } else if button == term::MouseButton::Right {
            self.open_menu(ev.position, window, cx);
        }
    }

    /// The right-click menu. A program that takes the mouse (vim, htop)
    /// gets the click instead; Shift brings the menu back, as it does
    /// selection.
    fn open_menu(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let (row, col, _) = self.cell_at(at);
        let this = cx.entity().downgrade();
        let f = self.focus.clone();
        let mut entries: Vec<Entry> = Vec::new();
        if let Some(url) = self.link_at(row, col) {
            let open = url.clone();
            let open_in = this.clone();
            entries.push(
                Item::new("Open Link", move |_, cx| {
                    let url = open.clone();
                    let _ = open_in.update(cx, |_, cx| cx.emit(TerminalEvent::OpenUrl(url)));
                })
                .icon(Icon::ArrowUpRight)
                .into(),
            );
            entries.push(
                Item::new("Copy Link", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                })
                .icon(Icon::Link)
                .into(),
            );
            entries.push(Entry::Separator);
        }
        let selected = self.term.selection_text().is_some();
        entries.extend([
            Item::new("Copy", menu::send(f.clone(), Copy))
                .icon(Icon::Copy)
                .shortcut("⌘C")
                .disabled(!selected)
                .into(),
            Item::new("Paste", menu::send(f.clone(), Paste))
                .icon(Icon::Clipboard)
                .shortcut("⌘V")
                .into(),
            Item::new("Select All", menu::send(f.clone(), SelectAll))
                .icon(Icon::SelectAll)
                .shortcut("⌘A")
                .into(),
            Entry::Separator,
            Item::new("Find…", menu::send(f.clone(), Find))
                .icon(Icon::Search)
                .shortcut("⌘F")
                .into(),
            Item::new("Clear", menu::send(f.clone(), ClearTerminal))
                .icon(Icon::Eraser)
                .shortcut("⌘K")
                .into(),
            Entry::Separator,
            Item::new("Split Right", menu::send(f.clone(), SplitRight))
                .icon(Icon::Split)
                .shortcut("⌘D")
                .into(),
            Item::new("Split Down", menu::send(f.clone(), SplitDown))
                .icon(Icon::SplitDown)
                .shortcut("⇧⌘D")
                .into(),
            Entry::Separator,
            Item::new("Close", menu::send(f.clone(), ClosePane))
                .icon(Icon::Close)
                .shortcut("⌘W")
                .into(),
        ]);
        self.menu = Some(menu::open(
            entries,
            at,
            Some(f),
            window,
            cx,
            |p: &mut Self, _, _| p.menu = None,
        ));
        cx.notify();
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.press = None;
        // A held press that never moved: it was a click, so Claude Code
        // gets it whole (expand a block, follow a link, place the cursor).
        if let Some((at, mods)) = self.held_click.take() {
            let left = Some(term::MouseButton::Left);
            if self.report(left, term::MouseAction::Press, at, &mods) {
                self.report(left, term::MouseAction::Release, at, &mods);
            }
            return;
        }
        if let Some(button) = self.reporting.take() {
            self.report(
                Some(button),
                term::MouseAction::Release,
                ev.position,
                &ev.modifiers,
            );
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (row, col, right) = self.cell_at(ev.position);
        let hover = self.block_at_row(row);
        if hover != self.hover_block {
            self.hover_block = hover;
            cx.notify();
        }
        // A button let go where this pane never heard it (over the sidebar,
        // another pane, the menu bar) left a press open. The program would
        // take every later move for a drag, and Claude Code then selects as
        // you scroll until the whole screen is grey. Close it now.
        if let (Some(button), None) = (self.reporting, ev.pressed_button) {
            self.reporting = None;
            self.report(
                Some(button),
                term::MouseAction::Release,
                ev.position,
                &ev.modifiers,
            );
        }
        if self.reporting.is_some()
            || (ev.pressed_button.is_none() && self.term.mouse_modes().motion)
        {
            if self.last_motion != Some((row, col)) {
                self.last_motion = Some((row, col));
                self.report(
                    self.reporting,
                    term::MouseAction::Motion,
                    ev.position,
                    &ev.modifiers,
                );
            }
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.press = None;
            self.held_click = None;
            return;
        }
        // Moved off the cell it was pressed on: a drag, so Deck selects.
        if let Some((at, _)) = self.held_click {
            let (r0, c0, _) = self.cell_at(at);
            if (r0, c0) == (row, col) {
                return;
            }
            self.held_click = None;
        }
        if let Some((r, c, side)) = self.press.take() {
            self.term.select_start(r, c, side, term::SelectMode::Char);
        }
        self.term.select_to(row, col, right);
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.line_px();
        let delta_px = match ev.delta {
            ScrollDelta::Lines(l) => l.y * lh,
            ScrollDelta::Pixels(d) => f32::from(d.y),
        };
        let lines = whole_lines(&mut self.scroll_acc, delta_px, lh);
        if lines == 0 {
            return;
        }
        // A press whose release we never heard (let go in another window,
        // after ⌘Tab) makes the program take every wheel step for a drag:
        // Claude Code then selects as you scroll until the screen is grey.
        // The wheel carries no button state, so ask the system.
        if let Some(button) = self.reporting {
            if !mouse_held() {
                self.reporting = None;
                self.report(
                    Some(button),
                    term::MouseAction::Release,
                    ev.position,
                    &ev.modifiers,
                );
            }
        }
        let (row, col, _) = self.cell_at(ev.position);
        // Full-screen programs scroll themselves (wheel reports, or arrows in
        // the alternate screen); everything else scrolls our scrollback.
        let report = if ev.modifiers.shift {
            None
        } else {
            self.term
                .scroll_report(lines, row, col, mods(&ev.modifiers))
        };
        match report {
            // The program scrolls its own content now; a highlight left on
            // cells it is about to redraw would only mislead.
            Some(bytes) => {
                mouse_log(&format!("Wheel {lines} reporting={:?} held={}", self.reporting, mouse_held()), &bytes);
                self.term.select_clear();
                self.term.write(bytes);
            }
            None => self.term.scroll(lines),
        }
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.term.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Text pastes as text. Files copied in Finder paste as their paths, and
    /// an image (a screenshot, "Copy Image") is kept as a file and pastes as
    /// its path, which Claude Code and Codex pick up as an attachment.
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        match pasteboard::read_for_terminal() {
            Some(Pasted::Files(paths)) => self.paste_text(&pasteboard::shell_paths(&paths)),
            Some(Pasted::Image(bytes, ext)) => {
                cx.spawn(async move |this, cx| {
                    let saved = cx
                        .background_executor()
                        .spawn(async move { pasteboard::save_image(&bytes, ext) })
                        .await;
                    match saved {
                        Ok(path) => {
                            let _ = this
                                .update(cx, |t, _| t.paste_text(&pasteboard::shell_paths(&[path])));
                        }
                        Err(e) => tracing::warn!("could not keep the pasted image: {e}"),
                    }
                })
                .detach();
            }
            None => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.paste_text(&text);
                }
            }
        }
    }

    /// What is selected, if anything.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_text()
    }

    /// Type `text` in as a paste: a shell or TUI takes it whole, and a
    /// newline in it does not run anything.
    /// Open the AI command box over the prompt.
    fn open_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ask.is_some() {
            return;
        }
        let field = cx.new(|cx| {
            let mut f = crate::field::Field::new("Describe a command, e.g. install codex from npm", cx);
            f.bare = true;
            f
        });
        let sub = cx.subscribe_in(&field, window, |p, _, ev: &crate::field::FieldEvent, window, cx| match ev {
            crate::field::FieldEvent::Submit(text) if !text.trim().is_empty() && !p.asking => {
                p.asking = true;
                // Ctrl+Enter (⌘↩ on macOS) runs it as soon as it is written.
                let m = window.modifiers();
                let run = m.control || m.platform;
                p.ask_runs = run;
                let cwd = p.term.shell_cwd().map(PathBuf::from);
                cx.emit(TerminalEvent::Ask { request: text.trim().to_string(), cwd, run });
                cx.notify();
            }
            crate::field::FieldEvent::Cancel => {
                p.close_ask(window, cx);
            }
            _ => {}
        });
        window.focus(&field.read(cx).focus);
        self.ask = Some((field, sub));
        cx.notify();
    }

    fn close_ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ask = None;
        self.asking = false;
        self.ask_runs = false;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Press Enter at the prompt: run what is typed there.
    pub fn press_enter(&mut self) {
        self.term.write(b"\r".to_vec());
    }

    /// The command came back (or failed): the box goes, the prompt has it.
    pub fn ask_done(&mut self, cx: &mut Context<Self>) {
        if self.ask.take().is_some() {
            self.asking = false;
            self.ask_runs = false;
            cx.notify();
        }
    }

    fn render_ask(&self, _cx: &mut Context<Self>) -> Option<AnyElement> {
        let (field, _) = self.ask.as_ref()?;
        Some(
            div()
                .absolute()
                .top(theme::u(10.))
                .left(theme::u(16.))
                .right(theme::u(16.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("ask-box")
                        .w_full()
                        .max_w(theme::u(560.))
                        .flex()
                        .flex_col()
                        .gap(theme::u(6.))
                        .p(theme::u(8.))
                        .rounded(theme::u(12.))
                        .bg(rgb(theme::menu()))
                        .border_1()
                        .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                        .occlude()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(theme::u(8.))
                                .h(theme::u(32.))
                                .px(theme::u(6.))
                                .child(if self.asking {
                                    crate::icon::spinner("ask-spin", 14., theme::accent())
                                } else {
                                    crate::icon::icon(Icon::Bolt, 14., theme::accent())
                                })
                                .child(div().flex_1().child(field.clone())),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(theme::u(12.))
                                .px(theme::u(6.))
                                .text_size(theme::u(11.))
                                .text_color(rgb(theme::dim()))
                                .child(if self.asking {
                                    div()
                                        .child(if self.ask_runs { "Writing the command, then running it…" } else { "Writing the command…" })
                                        .into_any_element()
                                } else {
                                    crate::kbd::hints("⏎ write it into the prompt · ⌘⏎ write and run it · esc close").into_any_element()
                                }),
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                )
                .into_any_element(),
        )
    }

    pub fn paste_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.term.bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        self.term.write(bytes.into_bytes());
    }
}

/// Add a scroll delta to the carried remainder and take out whole lines.
/// Trackpads deliver a few pixels per event; rounding each one alone (as the
/// first version did) turned every gesture into zero lines.
fn whole_lines(acc: &mut f32, delta_px: f32, line_px: f32) -> i32 {
    *acc += delta_px;
    let lines = (*acc / line_px).trunc();
    *acc -= lines * line_px;
    lines as i32
}

fn mods(m: &gpui::Modifiers) -> term::Mods {
    term::Mods {
        shift: m.shift,
        alt: m.alt,
        ctrl: m.control,
    }
}

impl EntityInputHandler for TerminalPane {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let n = self.marked.as_ref().map_or(0, |m| m.encode_utf16().count());
        Some(UTF16Selection {
            range: n..n,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        if !text.is_empty() {
            self.typed = Instant::now();
            self.blink_on = true;
            self.term.select_clear();
            self.term.write(text.as_bytes().to_vec());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!text.is_empty()).then(|| text.to_string());
        cx.notify();
    }

    /// Where the IME should put its candidate window: at the cursor.
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (y, x) = self.term.last().cursor?;
        let lh = self.line_px();
        Some(Bounds::new(
            point(
                self.origin.x + px(x as f32 * self.cell_w),
                self.origin.y + px(y as f32 * lh),
            ),
            size(px(self.cell_w), px(lh)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// -- command blocks ------------------------------------------------------------------------

/// A block's mark on screen: where its prompt starts (`None` when that is
/// above the view), how far it reaches, and what deck knows about it.
struct Shown<'a> {
    n: u64,
    start: Option<usize>,
    end: usize,
    block: &'a term::Block,
}

impl TerminalPane {
    /// The blocks on screen, top to bottom, from the last snapshot.
    fn shown_blocks(&self) -> Vec<Shown<'_>> {
        let snap = self.term.last();
        if snap.alt_screen || snap.blocks.is_empty() {
            return Vec::new();
        }
        let info = |n: u64| snap.blocks.iter().find(|(m, _)| *m == n).map(|(_, b)| b);
        let mut starts: Vec<(usize, u64)> = Vec::new();
        for (r, row) in snap.rows.iter().enumerate() {
            if let Some(n) = row.block {
                if r == 0 || snap.rows[r - 1].block != Some(n) {
                    starts.push((r, n));
                }
            }
        }
        // The newest block reaches down to the cursor, not the bottom of
        // an empty screen.
        let bottom = match snap.cursor {
            Some((row, _)) if snap.scrolled_back == 0 => row + 1,
            _ => snap.rows.len(),
        };
        let mut out = Vec::new();
        if let Some(n) = snap.above {
            let end = starts.first().map(|s| s.0).unwrap_or(bottom);
            if let Some(block) = info(n) {
                out.push(Shown { n, start: None, end, block });
            }
        }
        for (i, &(r, n)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map(|s| s.0).unwrap_or(bottom.max(r + 1));
            if let Some(block) = info(n) {
                out.push(Shown { n, start: Some(r), end, block });
            }
        }
        out
    }

    /// The block a screen row belongs to.
    fn block_at_row(&self, row: usize) -> Option<u64> {
        self.shown_blocks()
            .into_iter()
            .find(|b| b.start.unwrap_or(0) <= row && row < b.end)
            .map(|b| b.n)
    }

    fn copy_block(&mut self, n: u64, output: bool, cx: &mut Context<Self>) {
        let text = if output {
            self.term.block_output(n)
        } else {
            self.term.block(n).and_then(|b| b.command)
        };
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Run block `n`'s command again, at the prompt.
    fn rerun_block(&mut self, n: u64, cx: &mut Context<Self>) {
        let at_prompt = self.term.last_block().is_some_and(|(_, b)| !b.running());
        if let Some(cmd) = self.term.block(n).and_then(|b| b.command).filter(|_| at_prompt) {
            self.term.select_clear();
            self.term.write(format!("{cmd}\r").into_bytes());
            cx.notify();
        }
    }

    /// Hand a command and what it printed to an agent.
    fn ask_agent(&mut self, n: u64, cx: &mut Context<Self>) {
        let Some(block) = self.term.block(n) else {
            return;
        };
        let command = block.command.clone().unwrap_or_default();
        let output = self.term.block_output(n).unwrap_or_default();
        // The end of a long log is where the error is.
        let lines: Vec<&str> = output.lines().collect();
        let tail = lines[lines.len().saturating_sub(150)..].join("\n");
        let prompt = match block.exit {
            Some(code) if code != 0 => format!(
                "This command failed in my terminal (exit {code}):\n\n```\n$ {command}\n{tail}\n```\n\nFind out why and fix it."
            ),
            _ => format!("Here is a command I ran and its output:\n\n```\n$ {command}\n{tail}\n```\n\nExplain what happened."),
        };
        cx.emit(TerminalEvent::AskAgent {
            prompt,
            cwd: block.cwd.map(PathBuf::from),
        });
    }

    /// Separators between commands, a bar beside a failed (or running)
    /// one, its status on its prompt row, actions under the mouse, and the
    /// command whose output fills the screen pinned at the top.
    fn render_blocks(&self, pad: u32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.shown_blocks();
        if shown.is_empty() {
            return None;
        }
        let lh = self.line_px();
        let fs = (self.look.font_px * 0.82).max(10.);
        let red = self.look.palette.ansi[1];
        let green = self.look.palette.ansi[2];
        let rows = self.term.last().rows.len();
        let (pad_x, pad_y) = (theme::u(theme::PANE_PAD_X), theme::u(theme::PANE_PAD_Y));
        let mut layer = div()
            .absolute()
            .top(pad_y)
            .left(pad_x)
            .right(pad_x)
            .h(px(rows as f32 * lh));

        let chip = |b: &term::Block| -> Option<AnyElement> {
            let took = b.took().map(human);
            // Running: a still dot. A spinner would redraw the window for as
            // long as a dev server runs.
            let (mark, color, text) = if b.running() {
                ("●", theme::accent(), None)
            } else {
                match b.exit {
                    // A quick success needs no mark; a slow one says how long.
                    Some(0) if b.took().is_none_or(|t| t.as_secs_f32() < 2.) => return None,
                    Some(0) => ("✓", green, took),
                    Some(code) => ("✗", red, Some(format!("{code}{}", took.map(|t| format!(" · {t}")).unwrap_or_default()))),
                    None => return None,
                }
            };
            Some(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .child(div().text_color(rgb(color)).child(mark))
                    .children(text)
                    .into_any_element(),
            )
        };

        for b in &shown {
            let top = b.start.unwrap_or(0) as f32 * lh;
            let height = (b.end.saturating_sub(b.start.unwrap_or(0))) as f32 * lh;
            // The edge: red for a failure, the accent while it runs.
            let edge = if b.block.running() {
                Some(theme::accent())
            } else {
                b.block.exit.filter(|c| *c != 0).map(|_| red)
            };
            if let Some(color) = edge {
                layer = layer.child(
                    div()
                        .absolute()
                        .left(px(-7.))
                        .top(px(top))
                        .w(px(2.))
                        .h(px(height))
                        .rounded(px(1.))
                        .bg(rgb(color)),
                );
            }
            let Some(start) = b.start else { continue };
            if start > 0 {
                // Between commands: a quiet rule on the text's own edges,
                // so blocks part without boxing the screen in lines.
                let rule = theme::hairline();
                layer = layer.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(start as f32 * lh - 1.))
                        .h(px(1.))
                        .bg(rgba((rule & !0xff) | ((rule & 0xff) / 2))),
                );
            }
            let hovered = self.hover_block == Some(b.n);
            let status = if hovered {
                Some(self.block_actions(b.n, b.block, fs, cx))
            } else {
                chip(b.block)
            };
            if let Some(status) = status {
                layer = layer.child(
                    div()
                        .absolute()
                        .right(px(0.))
                        .top(px(start as f32 * lh))
                        .h(px(lh))
                        .flex()
                        .items_center()
                        .px(px(6.))
                        .rounded(px(5.))
                        .bg(rgb(pad))
                        .text_size(px(fs))
                        .text_color(rgb(theme::dim()))
                        .child(status),
                );
            }
        }

        // Reading a command's output with its prompt scrolled away: pin it.
        if let Some(b) = shown.first().filter(|b| b.start.is_none() && b.block.command.is_some()) {
            let n = b.n;
            let hovered = self.hover_block == Some(n);
            let command = b.block.command.clone().unwrap_or_default();
            let right = if hovered {
                Some(self.block_actions(n, b.block, fs, cx))
            } else {
                chip(b.block)
            };
            layer = layer.child(
                div()
                    .id(ElementId::Name(format!("sticky-{n}").into()))
                    .absolute()
                    .left(px(-8.))
                    .right(px(-8.))
                    .top(px(-4.))
                    .h(px(lh + 8.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .bg(rgb(pad))
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .cursor_pointer()
                    .font_family(self.look.family.clone())
                    .text_size(px(self.look.font_px))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |p, _, _, cx| {
                        if p.term.scroll_to_block(n) {
                            cx.notify();
                        }
                    }))
                    .child(div().flex_none().text_color(rgb(theme::accent())).child("❯"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .one_line()
                            .text_color(rgb(theme::text()))
                            .child(crate::chat::one_line_pub(&command)),
                    )
                    .children(right.map(|r| {
                        div()
                            .flex_none()
                            .text_size(px(fs))
                            .text_color(rgb(theme::dim()))
                            .child(r)
                    })),
            );
        }
        Some(layer.into_any_element())
    }

    /// Copy, Output, Rerun and Fix with agent, for the block under the mouse.
    fn block_actions(&self, n: u64, block: &term::Block, fs: f32, cx: &mut Context<Self>) -> AnyElement {
        let failed = block.exit.is_some_and(|c| c != 0);
        let latest = self.term.last_block().map(|(m, b)| (m, b.running()));
        let can_rerun = block.command.is_some() && latest.is_some_and(|(_, running)| !running);
        let button = |id: &str, label: &'static str, strong: bool| {
            div()
                .id(ElementId::Name(format!("blk-{id}-{n}").into()))
                .px(px(7.))
                .py(px(1.))
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(fs))
                .text_color(rgb(if strong { theme::text() } else { theme::text_2() }))
                .bg(rgb(theme::surface()))
                .hover(|s| s.bg(rgb(theme::surface_hover())))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .when(block.command.is_some(), |d| {
                d.child(button("cmd", "Copy", false).on_click(cx.listener(move |p, _, _, cx| p.copy_block(n, false, cx))))
            })
            .child(button("out", "Output", false).on_click(cx.listener(move |p, _, _, cx| p.copy_block(n, true, cx))))
            .when(can_rerun, |d| {
                d.child(button("run", "Rerun", false).on_click(cx.listener(move |p, _, _, cx| p.rerun_block(n, cx))))
            })
            .when(block.finished.is_some(), |d| {
                d.child(
                    button("ask", if failed { "Fix with agent" } else { "Ask agent" }, failed)
                        .on_click(cx.listener(move |p, _, _, cx| p.ask_agent(n, cx))),
                )
            })
            .into_any_element()
    }
}

impl TerminalPane {
    /// The last command failed and nothing has run since: what failed, the
    /// command it was likely meant to be when Deck can tell (`cls` for
    /// `clear`), and the agent to find out why. Gone with the next command.
    fn render_failed(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (n, block) = self.term.last_block()?;
        let code = block.exit.filter(|c| *c != 0)?;
        // Ctrl+C and the like end a command on purpose.
        if block.running() || self.fail_dismissed == Some(n) || matches!(code, 130 | 143) || self.ended {
            return None;
        }
        let command = block.command.clone()?;
        let first = command.split_whitespace().next().unwrap_or("").to_string();
        // A program that isn't there: offer the one it was likely meant to be.
        let fix = (code == 127)
            .then(|| crate::complete::meant(&first))
            .flatten()
            .map(|to| format!("{to}{}", &command[first.len()..]));
        let what = match code {
            127 => format!("{first}: command not found"),
            126 => format!("{first}: can't be run"),
            c => format!("failed with exit code {c}"),
        };
        let button = |id: &'static str, label: String, strong: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(px(24.))
                .px(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(strong, |d| d.bg(rgb(theme::text())).text_color(rgb(theme::pane())).hover(|s| s.opacity(0.9)))
                .when(!strong, |d| {
                    d.bg(rgb(theme::surface()))
                        .text_color(rgb(theme::text_2()))
                        .hover(|s| s.bg(rgb(theme::surface_hover())).text_color(rgb(theme::text())))
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(label)
        };
        let has_fix = fix.is_some();
        let card = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .max_w(px(560.))
            .pl(px(12.))
            .pr(px(6.))
            .py(px(6.))
            .rounded(px(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::DANGER, 0x55)))
            .shadow_lg()
            .font_family(theme::ui())
            .text_size(px(12.5))
            .child(div().flex_none().size(px(7.)).rounded_full().bg(rgb(theme::DANGER)))
            .child(div().flex_1().min_w_0().one_line().text_color(rgb(theme::text())).child(what))
            .children(fix.map(|fix| {
                let label = format!("Run {}", fix.chars().take(32).collect::<String>());
                button("fail-fix", label, true).on_click(cx.listener(move |p, _, _, cx| {
                    p.term.write(format!("{fix}\r").into_bytes());
                    cx.notify();
                }))
            }))
            .child(button("fail-agent", "Fix with agent".into(), !has_fix).on_click(cx.listener(move |p, _, _, cx| {
                p.fail_dismissed = Some(n);
                p.ask_agent(n, cx);
            })))
            .child(
                div()
                    .id("fail-close")
                    .flex_none()
                    .p(px(4.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(crate::icon::icon(crate::icon::Icon::Close, 11., theme::dim()))
                    .on_click(cx.listener(move |p, _, _, cx| {
                        p.fail_dismissed = Some(n);
                        cx.notify();
                    })),
            );
        Some(
            div()
                .absolute()
                .bottom(px(12.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .px(px(16.))
                .child(card)
                .into_any_element(),
        )
    }

    /// OSC 9;4 as a slim bar along the top: filled to the percentage, a
    /// sliding band when the program can't say how far, red for an error,
    /// amber when paused.
    fn render_progress(&self) -> Option<AnyElement> {
        let state = self.progress.flatten()?;
        let (color, fill) = match state {
            2 => (self.look.palette.ansi[1], Some(self.progress_pct.unwrap_or(100))),
            4 => (self.look.palette.ansi[3], Some(self.progress_pct.unwrap_or(100))),
            1 => (theme::accent(), self.progress_pct),
            _ => (theme::accent(), None),
        };
        let track = div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .right(px(0.))
            .h(px(2.))
            .overflow_hidden()
            .bg(rgba(theme::alpha(color, 0x33)));
        let bar = match fill {
            Some(pct) => div()
                .h_full()
                .w(relative(pct as f32 / 100.))
                .bg(rgb(color)),
            None => {
                // On the shared animation clock, not every display frame.
                let t = crate::icon::phase(1400);
                div()
                    .absolute()
                    .top(px(0.))
                    .h_full()
                    .w(relative(0.3))
                    .left(relative(t * 1.3 - 0.3))
                    .bg(rgb(color))
            }
        };
        Some(track.child(bar).into_any_element())
    }
}

/// The line before the word a menu was made for.
fn word_lead(s: &Suggest) -> String {
    let n = s.cursor.saturating_sub(s.found.word.chars().count());
    s.line.chars().take(n).collect()
}

/// "0.4s", "12s", "3m 5s".
fn human(d: std::time::Duration) -> String {
    let s = d.as_secs_f32();
    if s < 10. {
        format!("{s:.1}s")
    } else if s < 60. {
        format!("{}s", s as u64)
    } else {
        format!("{}m {}s", s as u64 / 60, s as u64 % 60)
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let restyle = std::mem::take(&mut self.restyle);
        self.term.snapshot(&self.look.palette, restyle);
        // An open menu holds the keyboard for this pane; the cursor should
        // not go hollow under it.
        let focused = self.focus.is_focused(window) || self.menu.is_some();
        let blink = focused && self.look.blink && self.term.last().cursor_blink;
        self.sync_blink(blink, cx);
        let pad = pane_background(self.term.last(), &self.look);
        let find = self.find.as_ref().map(|bar| self.render_find(bar, cx));
        div()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .relative()
            .size_full()
            .px(theme::u(theme::PANE_PAD_X))
            .py(theme::u(theme::PANE_PAD_Y))
            .bg(rgb(pad))
            .cursor_text()
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(|p, _: &AskCommand, window, cx| p.open_ask(window, cx)))
            .on_action(cx.listener(|p, _: &SelectAll, _, cx| {
                p.term.select_all();
                cx.notify();
            }))
            .on_action(cx.listener(|p, _: &FindNext, _, cx| p.find_step(true, cx)))
            .on_action(cx.listener(|p, _: &FindPrevious, _, cx| p.find_step(false, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_other_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_other_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            // Released outside the pane: still a release, for the program
            // and for a selection being dragged.
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            // Files dropped from Finder type their paths, as in Terminal; an
            // agent CLI turns an image path into an attachment.
            .on_drop(cx.listener(|p, paths: &ExternalPaths, window, cx| {
                p.paste_text(&pasteboard::shell_paths(paths.paths()));
                window.focus(&p.focus);
                cx.notify();
            }))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme::raised())))
            .child(Grid {
                pane: cx.entity(),
                focus: self.focus.clone(),
                focused,
            })
            .children(self.render_progress())
            .children(self.render_blocks(pad, cx))
            .children(self.render_failed(cx))
            .children(self.render_suggest(focused, cx))
            .children(self.render_ask(cx))
            .children(find)
            .children(self.menu.as_ref().map(menu::Open::render))
    }
}

/// The painted grid. Reads the pane's latest snapshot; owns nothing.
struct Grid {
    pane: Entity<TerminalPane>,
    focus: FocusHandle,
    focused: bool,
}

struct Prepared {
    glyphs: Vec<(Point<Pixels>, ShapedLine)>,
    fills: Vec<(Bounds<Pixels>, u32)>,
    cursor: Option<(Bounds<Pixels>, term::CursorShape)>,
    /// The character under a block cursor, redrawn in the background colour.
    under_cursor: Option<(Point<Pixels>, ShapedLine)>,
    /// IME composition, drawn over the cursor cell.
    marked: Option<(Point<Pixels>, ShapedLine)>,
    line_px: f32,
    cursor_color: u32,
    bg: u32,
}

impl IntoElement for Grid {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

/// Characters guaranteed to be exactly one Menlo cell wide.
fn on_grid(ch: char) -> bool {
    matches!(ch as u32, 0x20..=0x7e | 0xa0..=0x17f | 0x2500..=0x259f)
}

/// The colour behind the grid and in the padding around it.
///
/// A full-screen TUI (OpenCode, a theme-painting editor) paints every cell
/// with its own background; extending that colour into the padding, as
/// Ghostty does, keeps it from showing as a frame around the app. A program
/// that paints only some rows (Claude Code's band on each prompt) gets the
/// pane's colour: taking the top-left cell alone turned the whole pane that
/// band's colour whenever scrolling put a prompt on the top row. Painted
/// means most rows start with the top-left cell's colour, and the colour is
/// softened as the cells' fills are, so they match.
fn pane_background(snap: &term::Snapshot, look: &Look) -> u32 {
    let first = |r: &term::Row| r.runs.first().and_then(|run| run.style.bg);
    let Some(top) = snap.rows.first().and_then(first) else {
        return look.palette.bg;
    };
    let same = snap.rows.iter().filter(|r| first(r) == Some(top)).count();
    if same * 2 <= snap.rows.len() {
        return look.palette.bg;
    }
    let p = &look.palette;
    if look.soften && ![p.selection, p.search, p.search_focus].contains(&top) {
        soften(top, p.bg)
    } else {
        top
    }
}

/// A program's red or green background as a tint of the pane rather than
/// a solid block: a third of its saturation taken out, then 40% of the way
/// to the pane's colour. Claude Code's diffs otherwise paint whole lines in
/// deep green and red. Other hues are left alone: a blue is a selection
/// (Claude Code's own, when you drag in it) or a highlight, and softening it
/// made selected text look merely faded. Greys (bars, input boxes) carry the
/// layout and stay too.
fn soften(color: u32, pane: u32) -> u32 {
    let ch = |c: u32, shift: u32| ((c >> shift) & 0xff) as f32;
    let (r, g, b) = (ch(color, 16), ch(color, 8), ch(color, 0));
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    // A grey (Claude Code's command box, a TUI's panel): most of the way to
    // the pane, a quiet tint that still marks the block, not a slab.
    if max - min < 24. {
        let toward = |c: f32, p: f32| (c * 0.4 + p * 0.6).round().clamp(0., 255.) as u32;
        return (toward(r, ch(pane, 16)) << 16) | (toward(g, ch(pane, 8)) << 8) | toward(b, ch(pane, 0));
    }
    let hue = {
        let d = max - min;
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.)
        } else if max == g {
            (b - r) / d + 2.
        } else {
            (r - g) / d + 4.
        };
        h * 60.
    };
    let diff_like = !(25. ..320.).contains(&hue) || (75. ..=165.).contains(&hue);
    if !diff_like {
        return color;
    }
    let luma = 0.299 * r + 0.587 * g + 0.114 * b;
    let mix = |c: f32, p: f32| {
        let grey = c + (luma - c) * 0.35;
        (grey * 0.6 + p * 0.4).round().clamp(0., 255.) as u32
    };
    (mix(r, ch(pane, 16)) << 16) | (mix(g, ch(pane, 8)) << 8) | mix(b, ch(pane, 0))
}

/// One screen row, shaped: glyph runs at their column and background fills
/// as (column, width in columns, colour). Positions are in columns so the
/// row can be reused wherever the grid is painted.
#[derive(Default)]
struct RowCache {
    key: u64,
    valid: bool,
    glyphs: Vec<(usize, ShapedLine)>,
    fills: Vec<(usize, usize, u32)>,
}

/// What a row looks like, as a hash: text, styles and the font metrics.
fn row_key(row: &term::Row, font_px: f32, cell_w: f32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    font_px.to_bits().hash(&mut h);
    cell_w.to_bits().hash(&mut h);
    for run in &row.runs {
        run.text.hash(&mut h);
        let s = &run.style;
        (s.fg, s.bg, s.bold, s.italic, s.underline).hash(&mut h);
    }
    h.finish()
}

fn text_run(font: gpui::Font, color: u32, underline: bool, len: usize) -> TextRun {
    TextRun {
        len,
        font,
        color: rgb(color).into(),
        background_color: None,
        underline: underline.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(rgb(color).into()),
            wavy: false,
        }),
        strikethrough: None,
    }
}

/// Shape one row. A stretch of plain characters is one shaped line; any
/// other glyph is shaped alone at its own column (see the module docs).
fn shape_row(
    row: &term::Row,
    regular: &gpui::Font,
    size_px: Pixels,
    ts: &std::sync::Arc<gpui::WindowTextSystem>,
) -> RowCache {
    let mut out = RowCache {
        valid: true,
        ..Default::default()
    };
    let mut col = 0usize;
    for run in &row.runs {
        let s = &run.style;
        let start = col;
        let mut f = regular.clone();
        if s.bold {
            f.weight = FontWeight::BOLD;
        }
        if s.italic {
            f.style = FontStyle::Italic;
        }
        let mut push = |text: &mut String, at: usize| {
            if !text.is_empty() && (s.underline || !text.trim().is_empty()) {
                let len = text.len();
                let line = ts.shape_line(
                    SharedString::from(std::mem::take(text)),
                    size_px,
                    &[text_run(f.clone(), s.fg, s.underline, len)],
                    None,
                );
                out.glyphs.push((at, line));
            }
            text.clear();
        };
        let mut seg = String::new();
        let mut seg_col = col;
        for ch in run.text.chars() {
            if ch == '\0' {
                // Right half of a wide character: a column, no glyph.
                push(&mut seg, seg_col);
            } else if on_grid(ch) {
                if seg.is_empty() {
                    seg_col = col;
                }
                seg.push(ch);
            } else {
                push(&mut seg, seg_col);
                let mut solo = ch.to_string();
                push(&mut solo, col);
            }
            col += 1;
        }
        push(&mut seg, seg_col);
        if let Some(bg) = s.bg {
            out.fills.push((start, col - start, bg));
        }
    }
    out
}

impl Element for Grid {
    type RequestLayoutState = ();
    type PrepaintState = Prepared;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepared {
        let regular = theme::term_font(self.pane.read(cx).look.family.clone());
        let ts = window.text_system().clone();
        // Rows are shaped once and reused while their content is unchanged:
        // typing or a spinner touches one or two rows a frame, not all fifty.
        let (cell_w, lh, font_px, glyphs, fills) = self.pane.update(cx, |p, cx| {
            p.origin = bounds.origin;
            let lh = p.line_px();
            let cols = (f32::from(bounds.size.width) / p.cell_w).floor().max(2.) as u16;
            let rows = (f32::from(bounds.size.height) / lh).floor().max(2.) as u16;
            if p.term.resize(cols, rows, p.cell_w as u16, lh as u16) {
                cx.notify();
            }
            let (cell_w, font_px) = (p.cell_w, p.look.font_px);
            let size_px = px(font_px);
            let snap = p.term.last();
            let cache = &mut p.rows;
            cache.resize_with(snap.rows.len(), RowCache::default);
            let mut glyphs = Vec::new();
            let mut fills = Vec::new();
            for (y, row) in snap.rows.iter().enumerate() {
                let key = row_key(row, font_px, cell_w);
                let entry = &mut cache[y];
                if entry.key != key || !entry.valid {
                    *entry = shape_row(row, &regular, size_px, &ts);
                    entry.key = key;
                }
                let top = bounds.origin.y + px(y as f32 * lh);
                let x_at = |col: usize| bounds.origin.x + px(col as f32 * cell_w);
                for (col, line) in &entry.glyphs {
                    glyphs.push((point(x_at(*col), top), line.clone()));
                }
                for (col, n, color) in &entry.fills {
                    fills.push((
                        Bounds::new(point(x_at(*col), top), size(px(*n as f32 * cell_w), px(lh))),
                        *color,
                    ));
                }
            }
            (cell_w, lh, font_px, glyphs, fills)
        });

        let size_px = px(font_px);
        let pane = self.pane.read(cx);
        let snap = pane.term.last();
        let shape = |text: String, f: gpui::Font, color: u32, underline: bool| {
            let len = text.len();
            ts.shape_line(
                SharedString::from(text),
                size_px,
                &[text_run(f, color, underline, len)],
                None,
            )
        };

        let cursor_at = snap.cursor.map(|(y, x)| {
            Bounds::new(
                point(
                    bounds.origin.x + px(x as f32 * cell_w),
                    bounds.origin.y + px(y as f32 * lh),
                ),
                size(px(cell_w), px(lh)),
            )
        });
        let (bg, fg, cursor_color) = (
            pane.look.palette.bg,
            pane.look.palette.fg,
            pane.look.cursor_color,
        );
        let mut fills = fills;
        if pane.look.soften {
            let p = &pane.look.palette;
            // Deck's own fills (selection, find) keep the colour chosen for them.
            let ours = [p.selection, p.search, p.search_focus];
            for (_, c) in fills.iter_mut() {
                if !ours.contains(c) {
                    *c = soften(*c, bg);
                }
            }
        }
        let cursor = cursor_at
            .filter(|_| pane.blink_on && !pane.ended)
            .map(|b| (b, snap.cursor_shape));
        let under_cursor = match (snap.cursor, snap.cursor_shape, self.focused) {
            (Some((y, x)), term::CursorShape::Block, true) if pane.blink_on && !pane.ended => snap
                .rows
                .get(y)
                .and_then(|r| row_cols(r).chars().nth(x))
                .filter(|c| *c != '\0' && !c.is_whitespace())
                .zip(cursor_at)
                .map(|(c, b)| (b.origin, shape(c.to_string(), regular.clone(), bg, false))),
            _ => None,
        };
        let marked = pane
            .marked
            .clone()
            .zip(cursor_at)
            .map(|(m, b)| (b.origin, shape(m, regular.clone(), fg, true)));
        Prepared {
            glyphs,
            fills,
            cursor,
            under_cursor,
            marked,
            line_px: lh,
            cursor_color,
            bg,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prep: &mut Prepared,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.pane.clone()),
            cx,
        );
        let lh = px(prep.line_px);
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for (b, color) in &prep.fills {
                window.paint_quad(fill(*b, rgb(*color)));
            }
            for (origin, line) in &prep.glyphs {
                let _ = line.paint(*origin, lh, window, cx);
            }
            if let Some((c, shape)) = prep.cursor {
                let bar = |w: f32, h: f32, bottom: bool| {
                    let y = if bottom {
                        c.origin.y + c.size.height - px(h)
                    } else {
                        c.origin.y
                    };
                    Bounds::new(
                        point(c.origin.x, y),
                        size(
                            if w > 0. { px(w) } else { c.size.width },
                            if h > 0. { px(h) } else { c.size.height },
                        ),
                    )
                };
                match (shape, self.focused) {
                    (term::CursorShape::Block, true) => {
                        window.paint_quad(fill(c, rgb(prep.cursor_color)));
                        if let Some((origin, glyph)) = &prep.under_cursor {
                            let _ = glyph.paint(*origin, lh, window, cx);
                        }
                    }
                    (term::CursorShape::Beam, _) => {
                        window.paint_quad(fill(bar(2., 0., false), rgb(prep.cursor_color)))
                    }
                    (term::CursorShape::Underline, _) => {
                        window.paint_quad(fill(bar(0., 2., true), rgb(prep.cursor_color)))
                    }
                    (term::CursorShape::Block, false) => window.paint_quad(outline(
                        c,
                        rgba(theme::alpha(theme::text(), 0x80)),
                        BorderStyle::Solid,
                    )),
                }
            }
            if let Some((origin, glyph)) = &prep.marked {
                let w = glyph.width;
                window.paint_quad(fill(Bounds::new(*origin, size(w, lh)), rgb(prep.bg)));
                let _ = glyph.paint(*origin, lh, window, cx);
            }
        });
    }
}

/// A row with one char per column ('\0' for the right half of a wide char),
/// for column arithmetic.
fn row_cols(row: &term::Row) -> String {
    row.runs.iter().map(|r| r.text.as_str()).collect()
}

/// A row as text, for copying and reading.
fn row_text(row: &term::Row) -> String {
    row.runs
        .iter()
        .flat_map(|r| r.text.chars())
        .filter(|&c| c != '\0')
        .collect()
}

fn cell_width(window: &Window, family: &SharedString, font_px: f32) -> f32 {
    let ts = window.text_system();
    let id = ts.resolve_font(&theme::term_font(family.clone()));
    ts.advance(id, px(font_px), 'm')
        .map(|s| f32::from(s.width))
        .unwrap_or(font_px * 0.6)
}

/// The http(s) URL covering character column `col`, if any. `line` has one
/// char per column.
fn url_at(line: &str, col: usize) -> Option<String> {
    let mut from = 0;
    while let Some(i) = line[from..].find("http").map(|i| i + from) {
        let rest = &line[i..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            from = i + 4;
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '\0'))
            .map_or(line.len(), |e| i + e);
        let url = line[i..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
        let start_col = line[..i].chars().count();
        let end_col = start_col + url.chars().count();
        if (start_col..end_col).contains(&col) {
            return Some(url.to_string());
        }
        from = end.max(i + 1);
    }
    None
}

/// Translate a keystroke into an escape sequence. `None` means "not a
/// terminal key": ⌘ shortcuts go to the app, and printable text goes to the
/// input handler (so the IME, dead keys and ⌥-characters work).
fn key_bytes(ks: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    // ⌘ with an editing key does what it does in every Mac text field, as
    // Terminal and iTerm send it: ⌘⌫ deletes to the line's start (⌃U),
    // ⌘← / ⌘→ go to its start and end (⌃A / ⌃E), ⌘⌦ deletes to its end
    // (⌃K). Any other ⌘ key is the app's.
    if m.platform {
        if m.alt || m.control {
            return None;
        }
        return match ks.key.as_str() {
            "backspace" => Some(vec![0x15]),
            "left" if !m.shift => Some(vec![0x01]),
            "right" if !m.shift => Some(vec![0x05]),
            "delete" => Some(vec![0x0b]),
            _ => None,
        };
    }
    // Off macOS, Ctrl+⌫ deletes the word before the cursor and Ctrl+Alt+⌫
    // the line before it, and Ctrl+← / Ctrl+→ move by word, as GTK and Qt
    // terminals do; the bytes are the ones ⌥ and ⌘ send on macOS.
    let linux = !cfg!(target_os = "macos");
    if linux && m.control && !m.shift {
        match ks.key.as_str() {
            "backspace" if m.alt => return Some(vec![0x15]),
            "backspace" => return Some(vec![0x17]),
            "left" if !m.alt => return Some(b"\x1bb".to_vec()),
            "right" if !m.alt => return Some(b"\x1bf".to_vec()),
            "delete" if !m.alt => return Some(b"\x1bd".to_vec()),
            _ => {}
        }
    }
    let arrow = |c: u8| -> Vec<u8> {
        match (m.alt, c) {
            // ⌥←/⌥→ move by word, as Terminal.app and iTerm do.
            (true, b'D') => b"\x1bb".to_vec(),
            (true, b'C') => b"\x1bf".to_vec(),
            _ if app_cursor => vec![0x1b, b'O', c],
            _ => vec![0x1b, b'[', c],
        }
    };
    let fixed: Option<&[u8]> = match ks.key.as_str() {
        "enter" => Some(b"\r"),
        // ⌥⌫ deletes the word before the cursor: ⌃W, which zsh, bash,
        // readline and agent TUIs (Claude Code, Codex) all take as that,
        // where Esc-Delete reads as Esc then ⌫ to some of them.
        "backspace" if m.alt => Some(b"\x17"),
        "backspace" => Some(b"\x7f"),
        "tab" if m.shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(b) = fixed {
        return Some(b.to_vec());
    }
    match ks.key.as_str() {
        "up" => return Some(arrow(b'A')),
        "down" => return Some(arrow(b'B')),
        "right" => return Some(arrow(b'C')),
        "left" => return Some(arrow(b'D')),
        _ => {}
    }
    if m.control {
        let b = match ks.key.as_bytes() {
            [c @ b'a'..=b'z'] => c - b'a' + 1,
            b"space" | b"@" | b"2" => 0,
            b"[" | b"3" => 0x1b,
            b"\\" | b"4" => 0x1c,
            b"]" | b"5" => 0x1d,
            b"^" | b"6" => 0x1e,
            b"_" | b"-" | b"7" => 0x1f,
            _ => return None,
        };
        return Some(if m.alt { vec![0x1b, b] } else { vec![b] });
    }
    None
}

/// Option-key as Meta: Esc then the key itself, so Option-b / -f / -d reach
/// readline, Emacs and TUIs as word motions instead of typing ∫ ƒ ∂.
fn meta_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    if !m.alt || m.control || m.platform {
        return None;
    }
    let mut chars = ks.key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if !c.is_ascii_graphic() {
        return None;
    }
    let c = if m.shift { c.to_ascii_uppercase() } else { c };
    Some(vec![0x1b, c as u8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn look(soften: bool) -> Look {
        let mut look = crate::settings::Settings::default().term_look();
        look.soften = soften;
        look.palette.bg = 0x10141c;
        look
    }

    fn snap(bgs: &[Option<u32>]) -> term::Snapshot {
        term::Snapshot {
            rows: bgs
                .iter()
                .map(|bg| term::Row {
                    runs: vec![term::Run { text: "x".into(), style: term::Style { bg: *bg, ..Default::default() } }],
                    block: None,
                })
                .collect(),
            columns: 1,
            ..Default::default()
        }
    }

    #[test]
    fn one_painted_row_on_top_does_not_recolour_the_pane() {
        // Claude Code's prompt band scrolled to the top row.
        let s = snap(&[Some(0x565660), None, None, None, None]);
        assert_eq!(pane_background(&s, &look(true)), 0x10141c);
        assert_eq!(pane_background(&snap(&[None, None]), &look(true)), 0x10141c);
    }

    #[test]
    fn a_program_that_paints_the_screen_colours_the_pane_too() {
        let s = snap(&[Some(0x282c34), Some(0x282c34), Some(0x282c34), Some(0x1e2127)]);
        assert_eq!(pane_background(&s, &look(false)), 0x282c34);
        // Softened like the cells' fills, so no frame shows between them.
        assert_eq!(pane_background(&s, &look(true)), soften(0x282c34, 0x10141c));
    }

    #[test]
    fn coloured_backgrounds_become_tints_and_greys_stay() {
        let pane = 0x232323;
        // Claude Code's diff lines, measured off a screenshot.
        let green = soften(0x0e2705, pane);
        let red = soften(0x370603, pane);
        let sat = |c: u32| {
            let (r, g, b) = ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff);
            r.max(g).max(b) - r.min(g).min(b)
        };
        assert!(sat(green) < sat(0x0e2705) / 2, "{green:06x}");
        assert!(sat(red) < sat(0x370603) / 2, "{red:06x}");
        // Still green and still red.
        assert!((green >> 8) & 0xff > green >> 16 && (green >> 8) & 0xff > green & 0xff);
        assert!(red >> 16 > (red >> 8) & 0xff);
        // A grey bar becomes a quiet tint: nearer the pane, still apart from it.
        let grey = soften(0x373737, pane);
        assert!(grey < 0x373737 && grey != pane, "{grey:06x}");
        // So is a selection: Claude Code's dark and light theme blues.
        assert_eq!(soften(0x264f78, pane), 0x264f78);
        assert_eq!(soften(0xb4d5ff, pane), 0xb4d5ff);
        // Word-level diff highlights are still red and green, so they tint.
        assert_ne!(soften(0x38a660, pane), 0x38a660);
        assert_ne!(soften(0xb3596b, pane), 0xb3596b);
    }

    fn ks(key: &str, ch: Option<&str>, m: Modifiers) -> Keystroke {
        Keystroke {
            modifiers: m,
            key: key.into(),
            key_char: ch.map(Into::into),
        }
    }

    #[test]
    fn keys_map_to_terminal_bytes() {
        let none = Modifiers::default();
        let ctrl = Modifiers {
            control: true,
            ..none
        };
        let alt = Modifiers { alt: true, ..none };
        let cmd = Modifiers {
            platform: true,
            ..none
        };

        assert_eq!(key_bytes(&ks("c", None, ctrl), false), Some(vec![3]));
        assert_eq!(key_bytes(&ks("space", None, ctrl), false), Some(vec![0]));
        assert_eq!(
            key_bytes(&ks("up", None, none), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&ks("up", None, none), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(&ks("left", None, alt), false),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            key_bytes(&ks("enter", None, none), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(key_bytes(&ks("v", None, cmd), false), None);
        assert_eq!(key_bytes(&ks("backspace", None, cmd), false), Some(vec![0x15]));
        assert_eq!(key_bytes(&ks("left", None, cmd), false), Some(vec![0x01]));
        assert_eq!(key_bytes(&ks("right", None, cmd), false), Some(vec![0x05]));
        assert_eq!(key_bytes(&ks("backspace", None, alt), false), Some(vec![0x17]));
        // Text belongs to the input handler, so the IME and ⌥-characters work.
        assert_eq!(key_bytes(&ks("a", Some("a"), none), false), None);
        assert_eq!(key_bytes(&ks("s", Some("ß"), alt), false), None);
    }

    #[test]
    fn urls_are_found_under_the_cursor_column() {
        let line = "  ➜  Local:   http://localhost:5173/ (ready)";
        let at = line.chars().position(|c| c == 'h').unwrap();
        assert_eq!(
            url_at(line, at + 3).as_deref(),
            Some("http://localhost:5173/")
        );
        assert_eq!(url_at(line, 1), None);
        assert_eq!(
            url_at("see https://a.dev/x, then", 6).as_deref(),
            Some("https://a.dev/x")
        );
        assert_eq!(url_at("httpx nothing", 2), None);
        assert_eq!(
            url_at("(https://b.io/y)", 3).as_deref(),
            Some("https://b.io/y")
        );
        // A wide char before the URL occupies two columns ('\0' is its tail).
        assert_eq!(
            url_at("中\0 https://c.io", 5).as_deref(),
            Some("https://c.io")
        );
    }

    #[test]
    fn plain_text_stays_on_one_shaped_run_and_symbols_do_not() {
        assert!("ls -la ~/deck ── │ é".chars().all(on_grid));
        assert!(!on_grid('⏺') && !on_grid('中') && !on_grid('😀'));
    }

    #[test]
    fn trackpad_pixels_add_up_to_lines_in_both_directions() {
        let mut acc = 0.;
        let lines: i32 = (0..30).map(|_| whole_lines(&mut acc, 4., 18.)).sum();
        assert_eq!(lines, 6); // 120px over 18px lines
        assert!((acc - 12.).abs() < 1e-3);
        let back: i32 = (0..30).map(|_| whole_lines(&mut acc, -4., 18.)).sum();
        assert_eq!(back, -6);
        assert!(acc.abs() < 1e-3);
        // A mouse wheel notch arrives as whole lines.
        assert_eq!(whole_lines(&mut acc, 3. * 18., 18.), 3);
    }

    #[test]
    fn option_as_meta_sends_escape_and_the_key() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            meta_bytes(&ks("b", Some("∫"), alt)),
            Some(b"\x1bb".to_vec())
        );
        let alt_shift = Modifiers { shift: true, ..alt };
        assert_eq!(
            meta_bytes(&ks("f", Some("Ï"), alt_shift)),
            Some(b"\x1bF".to_vec())
        );
        assert_eq!(meta_bytes(&ks("b", Some("b"), Modifiers::default())), None);
        let cmd_alt = Modifiers {
            platform: true,
            ..alt
        };
        assert_eq!(meta_bytes(&ks("b", None, cmd_alt)), None);
        assert_eq!(meta_bytes(&ks("space", None, alt)), None);
    }

    #[test]
    fn line_height_matches_the_default_metrics() {
        // 13px Menlo on an 18px line is the classic Terminal.app metric.
        assert_eq!(line_height(13., crate::settings::DEFAULT_LINE_HEIGHT), 18.);
        assert_eq!(
            line_height(theme::FONT_PX, crate::settings::DEFAULT_LINE_HEIGHT),
            18.
        );
        assert_eq!(line_height(13., 1.6), 21.);
    }
}

/// Whether any mouse button is down right now, from the system rather than
/// from events this pane happened to receive.
/// Every mouse report sent to a program, with a timestamp, when
/// `mouse-debug.log` exists in Deck's data folder (create it to turn this
/// on, delete it to stop); nothing otherwise. For finding which event makes
/// a full-screen program think a button is held.
fn mouse_log(what: &str, bytes: &[u8]) {
    use std::io::Write;
    static FILE: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = crate::control::socket_dir().join("mouse-debug.log");
        path.exists()
            .then(|| std::fs::OpenOptions::new().append(true).open(path).ok().map(std::sync::Mutex::new))
            .flatten()
    });
    if let Some(f) = file {
        if let Ok(mut f) = f.lock() {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let _ = writeln!(f, "{t} {what} {:?}", String::from_utf8_lossy(bytes));
        }
    }
}

#[cfg(target_os = "macos")]
fn mouse_held() -> bool {
    objc2_app_kit::NSEvent::pressedMouseButtons() != 0
}

/// gpui has no global button state off macOS; drags end on mouse up.
#[cfg(not(target_os = "macos"))]
fn mouse_held() -> bool {
    false
}
