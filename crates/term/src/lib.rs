//! An embedded terminal, backed by `alacritty_terminal`.
//!
//! ## Why this shape
//!
//! `alacritty_terminal` is the terminal *emulator* — the VTE parser, the grid,
//! the scrollback, the mode state — factored out of Alacritty as a library. It
//! owns a PTY on its own thread and applies bytes to a `Term` behind a mutex.
//! That is the whole hard part of a terminal, and it is battle-tested; writing
//! another one would be a year of getting escape sequences subtly wrong.
//!
//! What this crate adds is the seam to a **retained-mode GPU UI**, which is not
//! how a terminal usually renders. Three decisions carry the performance:
//!
//! 1. **Runs, not cells.** A naive port emits one element per cell: an 80×24
//!    grid is 1,920 elements *per frame*, and a full-screen build log is far
//!    worse. [`Terminal::snapshot`] coalesces horizontally adjacent cells with
//!    identical styling into a single run, so an ordinary line of output becomes
//!    one or two text elements instead of eighty.
//! 2. **Dirty-gated.** The emulator thread raises an atomic flag on new content.
//!    The UI checks that flag and rebuilds only then, so an idle terminal costs
//!    one atomic load per frame and allocates nothing.
//! 3. **Short lock.** The `Term` mutex is held only for the length of the grid
//!    walk and never across UI work, so the PTY thread is never blocked by
//!    rendering.
//!
//! The crate is deliberately UI-agnostic: colours come out as `0xRRGGBB` and the
//! caller supplies the palette, which is what lets the terminal follow the app's
//! own theme instead of shipping a second one.

mod tap;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

pub type Rgb = u32;

/// Lines of scrollback. Ten thousand is Alacritty's own default and costs a few
/// megabytes at most; a build log that scrolls off the top of the buffer is the
/// single most annoying thing a terminal can do.
const SCROLLBACK: usize = 10_000;

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

/// The 16 ANSI colours plus the default foreground and background.
///
/// Supplied by the caller so the terminal inherits the app's theme. Without
/// this a terminal widget always looks like it was pasted in from another
/// program, because it was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    /// Fill behind selected cells.
    pub selection: Rgb,
    /// Fill behind find matches, and behind the one the view is on (whose
    /// text is drawn in `bg` so it reads on a light fill).
    pub search: Rgb,
    pub search_focus: Rgb,
    /// Black, red, green, yellow, blue, magenta, cyan, white — normal then
    /// bright.
    pub ansi: [Rgb; 16],
}

impl Palette {
    fn named(&self, n: NamedColor) -> Rgb {
        use NamedColor as N;
        match n {
            N::Black => self.ansi[0],
            N::Red => self.ansi[1],
            N::Green => self.ansi[2],
            N::Yellow => self.ansi[3],
            N::Blue => self.ansi[4],
            N::Magenta => self.ansi[5],
            N::Cyan => self.ansi[6],
            N::White => self.ansi[7],
            N::BrightBlack => self.ansi[8],
            N::BrightRed => self.ansi[9],
            N::BrightGreen => self.ansi[10],
            N::BrightYellow => self.ansi[11],
            N::BrightBlue => self.ansi[12],
            N::BrightMagenta => self.ansi[13],
            N::BrightCyan => self.ansi[14],
            N::BrightWhite => self.ansi[15],
            N::Foreground | N::BrightForeground => self.fg,
            N::Background => self.bg,
            N::Cursor => self.fg,
            // The dim variants are the normal ones darkened; the DIM flag is
            // applied separately, so mapping them to their base colour here
            // avoids dimming twice.
            N::DimBlack => self.ansi[0],
            N::DimRed => self.ansi[1],
            N::DimGreen => self.ansi[2],
            N::DimYellow => self.ansi[3],
            N::DimBlue => self.ansi[4],
            N::DimMagenta => self.ansi[5],
            N::DimCyan => self.ansi[6],
            N::DimWhite => self.ansi[7],
            N::DimForeground => self.fg,
        }
    }

    /// Dark or light, as `CSI ? 997 ; n` reports it: 1 dark, 2 light.
    fn scheme(&self) -> u8 {
        let ch = |shift: u32| ((self.bg >> shift) & 0xff) as f32;
        if 0.299 * ch(16) + 0.587 * ch(8) + 0.114 * ch(0) < 128. {
            1
        } else {
            2
        }
    }

    /// Answer an OSC colour query. Index 256 is the foreground, 257 the
    /// background, 258 the cursor; below that it is the 256-colour table.
    fn answer(&self, index: usize) -> Rgb {
        match index {
            256 => self.fg,
            257 => self.bg,
            258 => self.fg,
            i => indexed(self, (i & 0xff) as u8),
        }
    }

    fn resolve(&self, color: AnsiColor) -> Rgb {
        match color {
            AnsiColor::Named(n) => self.named(n),
            AnsiColor::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
            AnsiColor::Indexed(i) => indexed(self, i),
        }
    }
}

/// The xterm 256-colour cube. 0–15 defer to the theme's ANSI set; 16–231 are a
/// 6×6×6 RGB cube; 232–255 are a 24-step grey ramp.
fn indexed(p: &Palette, i: u8) -> Rgb {
    match i {
        0..=15 => p.ansi[i as usize],
        16..=231 => {
            const STEPS: [u32; 6] = [0, 95, 135, 175, 215, 255];
            let i = (i - 16) as usize;
            let (r, g, b) = (STEPS[i / 36], STEPS[(i / 6) % 6], STEPS[i % 6]);
            (r << 16) | (g << 8) | b
        }
        232..=255 => {
            let v = 8 + (i as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

/// Darken towards black, for the DIM attribute.
fn dim(rgb: Rgb) -> Rgb {
    let f = |shift: u32| (((rgb >> shift) & 0xff) * 2 / 3) & 0xff;
    (f(16) << 16) | (f(8) << 8) | f(0)
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Rgb,
    /// `None` means "the default background", which the renderer can skip
    /// drawing entirely. Most cells on a screen are this.
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A horizontal span of identically styled cells.
///
/// **One `char` per column.** `'\0'` is the right half of a wide char (CJK,
/// emoji): renderers skip drawing it but count it as a column, and text
/// extraction drops it. It carries the wide char's style, so a background fill
/// spans both cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

/// One viewport line. Its runs hold exactly `Snapshot::columns` chars in
/// total, so the n-th char is the n-th column (see [`Run`] for `'\0'`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub runs: Vec<Run>,
    /// The block whose prompt or command line this row shows, when the
    /// shell marks them (see [`Block`]).
    pub block: Option<u64>,
}

/// One command at the shell, from its prompt to the next: what deck knows
/// about it besides its text on screen. The shell's marks (OSC 133, sent by
/// deck's zsh integration) number them; see `tap.rs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    /// The command line, once it runs.
    pub command: Option<String>,
    /// The folder it ran in.
    pub cwd: Option<String>,
    pub started: Option<std::time::Instant>,
    pub finished: Option<std::time::Instant>,
    pub exit: Option<i32>,
}

impl Block {
    pub fn running(&self) -> bool {
        self.started.is_some() && self.finished.is_none()
    }

    /// How long it ran, or has run so far.
    pub fn took(&self) -> Option<std::time::Duration> {
        let start = self.started?;
        Some(self.finished.unwrap_or_else(std::time::Instant::now) - start)
    }
}

/// Blocks kept per terminal; the oldest go first.
const MAX_BLOCKS: usize = 5000;

/// The cursor shape a program asked for with DECSCUSR (`ESC [ n SP q`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

fn cursor_shape_of(shape: alacritty_terminal::vte::ansi::CursorShape) -> CursorShape {
    use alacritty_terminal::vte::ansi::CursorShape as A;
    match shape {
        A::Beam => CursorShape::Beam,
        A::Underline => CursorShape::Underline,
        // Hollow is how alacritty draws an unfocused block; focus is the UI's
        // business. Hidden already yields `cursor: None`.
        A::Block | A::HollowBlock | A::Hidden => CursorShape::Block,
    }
}

/// One frame of the terminal, ready to render.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub columns: usize,
    /// `(row, column)` within the viewport, absent when the cursor is hidden or
    /// scrolled out of view.
    pub cursor: Option<(usize, usize)>,
    pub cursor_shape: CursorShape,
    /// Whether the cursor style in force (the program's, or the default set
    /// with [`Terminal::set_cursor_default`]) asks for blinking.
    pub cursor_blink: bool,
    /// How far the view is scrolled back, in lines. Non-zero means the user is
    /// looking at history rather than the live tail.
    pub scrolled_back: usize,
    /// A full-screen program (an editor, a TUI) has the screen: no blocks.
    pub alt_screen: bool,
    /// The blocks on screen, and the one the top row belongs to when its
    /// prompt has scrolled out above (`above`), by number.
    pub blocks: Vec<(u64, Block)>,
    pub above: Option<u64>,
}

/// Something that happened that the UI has to react to, rather than just
/// redraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    Title(String),
    Bell,
    /// A desktop notification the program asked for (OSC 9 / OSC 777).
    Notify {
        title: Option<String>,
        body: String,
    },
    /// OSC 9;4 progress: `Some(state)` while working, `None` when cleared,
    /// and the percentage when the program gives one.
    Progress(Option<u8>, Option<u8>),
    /// "# …" at the prompt: a command wanted for this.
    Ask(String),
    /// The line being edited at the prompt and the cursor in it (characters),
    /// or `None` when it was entered or abandoned.
    Line(Option<(usize, String)>),
    /// A command at the shell finished.
    CommandDone {
        block: u64,
        command: Option<String>,
        exit: Option<i32>,
        took: std::time::Duration,
    },
    Exited(Option<i32>),
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// Shared state the emulator thread writes and the UI thread reads.
struct Shared {
    dirty: AtomicBool,
    exited: AtomicBool,
    events: Mutex<Vec<TermEvent>>,
    /// Channel back to the PTY, installed once the event loop exists.
    ///
    /// Needed because a terminal is a *dialogue*, not a one-way pipe: the
    /// emulator answers the program's queries, and those answers have to reach
    /// the PTY. See the `PtyWrite` arm below.
    writer: Mutex<Option<alacritty_terminal::event_loop::EventLoopSender>>,
    /// Last known geometry and palette, for the queries that ask about them.
    window: Mutex<WindowSize>,
    palette: Mutex<Option<Palette>>,
    /// Wakes the UI when there is something to look at, instead of it polling.
    /// `wake_pending` coalesces a burst of output into one wakeup per UI turn.
    wake: Mutex<Option<futures::channel::mpsc::UnboundedSender<()>>>,
    wake_pending: AtomicBool,
    /// The shell's commands by block number, and its folder (OSC 7).
    blocks: Mutex<std::collections::BTreeMap<u64, Block>>,
    cwd: Mutex<Option<String>>,
    /// Whether the program asked to hear when the theme turns dark or light
    /// (DEC mode 2031), and which it is: 0 not yet known, else 1 dark, 2 light.
    scheme_updates: AtomicBool,
    scheme: AtomicU8,
}

impl Shared {
    fn new(window: WindowSize) -> Shared {
        Shared {
            dirty: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            writer: Mutex::new(None),
            window: Mutex::new(window),
            palette: Mutex::new(None),
            wake: Mutex::new(None),
            wake_pending: AtomicBool::new(false),
            blocks: Mutex::new(std::collections::BTreeMap::new()),
            cwd: Mutex::new(None),
            scheme_updates: AtomicBool::new(false),
            scheme: AtomicU8::new(0),
        }
    }

    /// Bytes to the program, as if typed: the emulator's answers to its queries.
    fn send(&self, bytes: Vec<u8>) {
        if let Ok(mut w) = self.writer.lock() {
            if let Some(sender) = w.as_mut() {
                let _ = sender.send(Msg::Input(Cow::Owned(bytes)));
            }
        }
    }

    /// `CSI ? 997 ; n`: the theme is dark (1) or light (2). Before the first
    /// frame it isn't known yet; deck's default theme is dark.
    fn send_scheme(&self, scheme: u8) {
        let scheme = if scheme == 2 { 2 } else { 1 };
        self.send(format!("\x1b[?997;{scheme}n").into_bytes());
    }

    /// What the scanner heard on its way to the emulator.
    fn signals(&self, found: impl Iterator<Item = tap::Signal>) {
        use tap::Signal;
        let mut events = Vec::new();
        {
            let mut blocks = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
            for signal in found {
                match signal {
                    Signal::Notify { title, body } => events.push(TermEvent::Notify { title, body }),
                    Signal::Progress(p, pct) => events.push(TermEvent::Progress(p, pct)),
                    Signal::Prompt(n) => {
                        // The shell has the terminal back: whatever asked for
                        // theme updates has gone, and a report now would land
                        // on the command line as typing.
                        self.scheme_updates.store(false, Ordering::Relaxed);
                        let cwd = self.cwd.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        blocks.insert(n, Block { cwd, ..Default::default() });
                        while blocks.len() > MAX_BLOCKS {
                            blocks.pop_first();
                        }
                    }
                    Signal::Command(n, command) => {
                        if let Some(b) = blocks.get_mut(&n) {
                            b.command = Some(command);
                        }
                    }
                    Signal::Started(n) => {
                        if let Some(b) = blocks.get_mut(&n) {
                            b.started = Some(std::time::Instant::now());
                        }
                    }
                    Signal::Finished(n, exit) => {
                        if let Some(b) = blocks.get_mut(&n).filter(|b| b.running()) {
                            let now = std::time::Instant::now();
                            b.finished = Some(now);
                            b.exit = exit;
                            events.push(TermEvent::CommandDone {
                                block: n,
                                command: b.command.clone(),
                                exit,
                                took: b.took().unwrap_or_default(),
                            });
                        }
                    }
                    Signal::Cwd(path) => *self.cwd.lock().unwrap_or_else(|e| e.into_inner()) = Some(path),
                    Signal::Ask(text) => events.push(TermEvent::Ask(text)),
                    Signal::Line(line) => {
                        // Only the newest edit matters; drop older ones still queued.
                        events.retain(|e| !matches!(e, TermEvent::Line(_)));
                        events.push(TermEvent::Line(line));
                    }
                    Signal::SchemeUpdates(on) => self.scheme_updates.store(on, Ordering::Relaxed),
                    Signal::SchemeQuery => self.send_scheme(self.scheme.load(Ordering::Relaxed)),
                }
            }
        }
        if !events.is_empty() {
            if let Ok(mut q) = self.events.lock() {
                q.extend(events);
            }
        }
        self.mark_dirty();
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            if let Ok(w) = self.wake.lock() {
                if let Some(tx) = w.as_ref() {
                    let _ = tx.unbounded_send(());
                }
            }
        }
    }
}

#[derive(Clone)]
struct Proxy(Arc<Shared>);

impl EventListener for Proxy {
    fn send_event(&self, event: AlacEvent) {
        match event {
            // The common case by far: new content. Setting a flag is all this
            // does — anything expensive here would run on the PTY thread, in
            // the path of every byte the shell prints.
            AlacEvent::Wakeup | AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => {}

            // A reply the emulator owes the program. **Dropping these is not a
            // cosmetic omission.** Cursor-position reports, device attributes
            // and the rest are how a shell finds out where it is; a prompt that
            // asks and never hears back carries on with wrong coordinates, and
            // the next thing it prints lands in the wrong column. That was the
            // symptom that led here: zsh rendered `printfX1` where a plain `sh`
            // rendered `X1`, because only zsh asks.
            // alacritty doesn't know mode 2031 and says so when a program asks
            // (DECRQM); deck does, and answers with its state instead.
            AlacEvent::PtyWrite(text) if text == "\x1b[?2031;0$y" => {
                let state = if self.0.scheme_updates.load(Ordering::Relaxed) { 1 } else { 2 };
                self.reply(format!("\x1b[?2031;{state}$y").into_bytes());
            }
            AlacEvent::PtyWrite(text) => self.reply(text.into_bytes()),

            AlacEvent::TextAreaSizeRequest(format) => {
                let size = self.0.window.lock().map(|w| *w).unwrap_or(WindowSize {
                    num_cols: 80,
                    num_lines: 24,
                    cell_width: 8,
                    cell_height: 16,
                });
                self.reply(format(size).into_bytes());
            }

            // Programs ask what colour the terminal is so they can pick a
            // readable palette. Answering with the theme's real colours is what
            // makes a `bat` or a `vim` inside this pane match the app.
            AlacEvent::ColorRequest(index, format) => {
                let rgb = self
                    .0
                    .palette
                    .lock()
                    .ok()
                    .and_then(|p| p.map(|p| p.answer(index)))
                    .unwrap_or(0);
                self.reply(
                    format(alacritty_terminal::vte::ansi::Rgb {
                        r: (rgb >> 16) as u8,
                        g: (rgb >> 8) as u8,
                        b: rgb as u8,
                    })
                    .into_bytes(),
                );
            }

            AlacEvent::Title(t) => self.push(TermEvent::Title(t)),
            AlacEvent::ResetTitle => self.push(TermEvent::Title(String::new())),
            AlacEvent::Bell => self.push(TermEvent::Bell),
            AlacEvent::ChildExit(status) => {
                self.0.exited.store(true, Ordering::Release);
                self.push(TermEvent::Exited(status.code()));
            }
            AlacEvent::Exit => {
                self.0.exited.store(true, Ordering::Release);
                self.push(TermEvent::Exited(None));
            }
            // Clipboard integration is not wired up; ignoring a paste request is
            // an inert no-op, where answering it wrongly would not be.
            _ => {}
        }
        self.0.mark_dirty();
    }
}

impl Proxy {
    fn push(&self, ev: TermEvent) {
        if let Ok(mut q) = self.0.events.lock() {
            q.push(ev);
        }
    }

    fn reply(&self, bytes: Vec<u8>) {
        self.0.send(bytes);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    Char,
    Word,
    Line,
}

/// Which mouse events the program asked for (DECSET 1000/1002/1003/1006 and
/// alternate-screen scrolling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseModes {
    /// Any of 1000, 1002, 1003: clicks are the program's, not ours.
    pub report: bool,
    /// 1002: motion while a button is held.
    pub drag: bool,
    /// 1003: all motion.
    pub motion: bool,
    /// 1006: SGR encoding, no coordinate limit.
    pub sgr: bool,
    /// Alternate screen with alternate scroll: the wheel becomes arrow keys.
    pub alt_scroll: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Wheel deltas past this are clamped: a trackpad fling can report dozens of
/// lines in one event, and replaying all of them floods the program.
const MAX_SCROLL_REPEAT: usize = 10;

fn modes_of(mode: alacritty_terminal::term::TermMode) -> MouseModes {
    use alacritty_terminal::term::TermMode as M;
    MouseModes {
        report: mode.intersects(M::MOUSE_MODE),
        drag: mode.contains(M::MOUSE_DRAG),
        motion: mode.contains(M::MOUSE_MOTION),
        sgr: mode.contains(M::SGR_MOUSE),
        alt_scroll: mode.contains(M::ALT_SCREEN | M::ALTERNATE_SCROLL),
    }
}

fn mods_bits(m: Mods) -> u8 {
    (if m.shift { 4 } else { 0 }) | (if m.alt { 8 } else { 0 }) | (if m.ctrl { 16 } else { 0 })
}

/// Encode one mouse event. `code` already includes the button, the motion bit
/// (32) and the modifier bits; `row`/`col` are 0-based viewport cells.
///
/// SGR (1006): `ESC [ < code ; col+1 ; row+1 M`, or `m` for a release (which
/// keeps its button). Normal: `ESC [ M` and three bytes offset by 32, a release
/// reported as button 3; `None` when a coordinate cannot fit in a byte.
fn encode_mouse(sgr: bool, code: u8, release: bool, row: usize, col: usize) -> Option<Vec<u8>> {
    if sgr {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    let code = if release { (code & !0b11) | 3 } else { code };
    let byte = |v: usize| u8::try_from(32 + v).ok();
    Some(vec![
        0x1b,
        b'[',
        b'M',
        byte(code as usize)?,
        byte(col + 1)?,
        byte(row + 1)?,
    ])
}

fn report_bytes(
    modes: MouseModes,
    button: Option<MouseButton>,
    action: MouseAction,
    row: usize,
    col: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    let base = match button {
        Some(MouseButton::Left) => 0,
        Some(MouseButton::Middle) => 1,
        Some(MouseButton::Right) => 2,
        None => 3,
    };
    let code = match (action, button) {
        (MouseAction::Press | MouseAction::Release, Some(_)) if modes.report => base,
        (MouseAction::Motion, Some(_)) if modes.drag || modes.motion => base + 32,
        (MouseAction::Motion, None) if modes.motion => base + 32,
        _ => return None,
    };
    encode_mouse(
        modes.sgr,
        code | mods_bits(mods),
        action == MouseAction::Release,
        row,
        col,
    )
}

fn scroll_bytes(
    modes: MouseModes,
    app_cursor: bool,
    lines: i32,
    row: usize,
    col: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    if lines == 0 {
        return None;
    }
    let n = (lines.unsigned_abs() as usize).min(MAX_SCROLL_REPEAT);
    let up = lines > 0;
    if modes.report {
        let one = encode_mouse(
            modes.sgr,
            (if up { 64 } else { 65 }) | mods_bits(mods),
            false,
            row,
            col,
        )?;
        return Some(one.repeat(n));
    }
    if modes.alt_scroll {
        let arrow: &[u8] = match (up, app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(n));
    }
    None
}

fn side(right: bool) -> alacritty_terminal::index::Side {
    if right {
        alacritty_terminal::index::Side::Right
    } else {
        alacritty_terminal::index::Side::Left
    }
}

/// A viewport cell as a grid point, accounting for scrollback.
fn viewport_point(term: &Term<Proxy>, row: usize, col: usize) -> Point {
    let offset = term.grid().display_offset() as i32;
    let col = col.min(term.columns().saturating_sub(1));
    Point::new(Line(row as i32 - offset), Column(col))
}

// ---------------------------------------------------------------------------
// Find
// ---------------------------------------------------------------------------

/// Lines scanned past each edge of the viewport, so a match that starts or
/// ends off screen is still highlighted where it shows (Alacritty's margin).
const SEARCH_MARGIN: usize = 100;

/// Find-in-scrollback: the compiled query and the match the view is on.
struct Search {
    regex: RegexSearch,
    /// In grid coordinates, which shift as output pushes lines into history.
    /// `history` is the history size it was found at, so [`Search::rebase`]
    /// can follow it.
    focus: Option<Match>,
    history: usize,
}

impl Search {
    fn new(query: &str, term: &Term<Proxy>) -> Option<Search> {
        Some(Search {
            regex: RegexSearch::new(&literal(query)).ok()?,
            focus: None,
            history: term.history_size(),
        })
    }

    /// Keep the focused match on the same text as output pushes lines into
    /// history; drop it once it falls out of the buffer or ⌘K clears it.
    // phobos: at the scrollback cap history stops growing, so the focus slips
    // a line per new line there; a lifetime line counter would fix that.
    fn rebase(&mut self, term: &Term<Proxy>) {
        let history = term.history_size();
        let grown = history.checked_sub(self.history);
        self.history = history;
        let (Some(m), Some(d)) = (self.focus.take(), grown) else {
            return;
        };
        let (start, end) = (*m.start(), *m.end());
        if start.line - d >= term.topmost_line() {
            self.focus = Some(
                Point::new(start.line - d, start.column)..=Point::new(end.line - d, end.column),
            );
        }
    }

    /// The next match up (older) or down from the focused one, wrapping at
    /// either end; with nothing focused, the newest.
    fn step(&mut self, term: &mut Term<Proxy>, up: bool) -> bool {
        self.rebase(term);
        let origin = match (&self.focus, up) {
            (Some(m), true) => m.start().sub(term, Boundary::None, 1),
            (Some(m), false) => m.end().add(term, Boundary::None, 1),
            (None, _) => bottom_right(term),
        };
        let direction = if up || self.focus.is_none() {
            Direction::Left
        } else {
            Direction::Right
        };
        self.find(term, origin, direction)
    }

    /// Focus the first match starting at or past `origin`, scrolling it into
    /// view.
    fn find(&mut self, term: &mut Term<Proxy>, origin: Point, direction: Direction) -> bool {
        self.focus = term.search_next(&mut self.regex, origin, direction, Side::Left, None);
        if let Some(m) = &self.focus {
            term.scroll_to_point(*m.start());
        }
        self.focus.is_some()
    }
}

fn bottom_right(term: &Term<Proxy>) -> Point {
    Point::new(term.bottommost_line(), term.last_column())
}

/// The query as a pattern that matches it literally: what you type is what is
/// found, `1+1` included. Case follows Alacritty's smart case: insensitive
/// unless the query has a capital.
fn literal(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Matches touching the viewport, in grid order.
fn visible_matches(term: &Term<Proxy>, regex: &mut RegexSearch) -> Vec<Match> {
    let top = Line(-(term.grid().display_offset() as i32));
    let bottom = top + (term.screen_lines() - 1);
    let mut start = term.line_search_left(Point::new(top, Column(0)));
    let mut end = term.line_search_right(Point::new(bottom, Column(0)));
    start.line = start.line.max(top - SEARCH_MARGIN);
    end.line = end.line.min(bottom + SEARCH_MARGIN);
    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(|m| m.end().line < top)
        .take_while(|m| m.start().line <= bottom)
        .collect()
}

/// Grid dimensions, in the shape `alacritty_terminal` asks for.
#[derive(Debug, Clone, Copy)]
struct Size {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// How to start the shell.
#[derive(Debug, Clone, Default)]
pub struct Spawn {
    /// `None` starts the user's login shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
}

/// What we tell programs they are running inside.
///
/// **This is not optional, and it must not be inherited.** `alacritty_terminal`
/// leaves `TERM` alone, so without this the shell inherits whatever launched the
/// app. Launch from inside tmux and the child is told `TERM=screen-256color`,
/// whereupon zsh sets the window title with screen's `ESC k … ESC \` sequence —
/// which this emulator does not implement, so it lands on the grid as literal
/// text. The symptom was output lines rendering as `printfX1` instead of `X1`:
/// the title text, printed.
///
/// `xterm-256color` rather than `alacritty`: it is a subset of what this
/// emulator does, and unlike `alacritty` its terminfo entry exists on every
/// machine. Declaring a terminfo name the system does not have is its own class
/// of breakage.
const TERM_NAME: &str = "xterm-256color";

/// Variables that would make a child believe it is inside something it is not.
/// Cleared rather than passed through, for the same reason as `TERM`. The
/// Claude Code ones leak in when deck itself is launched from a Claude session,
/// and make every `claude` started in a pane think it is a nested child.
const STALE_ENV: &[&str] = &[
    "TMUX",
    "TMUX_PANE",
    "STY",
    "TERM_PROGRAM",
    "VTE_VERSION",
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

/// The user's login shell, without the `Last login: …` banner.
///
/// Left to itself `alacritty_terminal` runs the shell through `/usr/bin/login`
/// so it gets a utmp entry, and `login` prints the last-login line unless
/// `~/.hushlogin` exists. This is the same command with `-q` added, so the
/// banner is gone without anyone having to create that file.
#[cfg(target_os = "macos")]
fn login_shell() -> Option<tty::Shell> {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let name = shell.rsplit('/').next().unwrap_or("zsh");
    // -f skips authentication, -l keeps the cwd, -p keeps the environment.
    // zsh runs the `exec -a` that makes the real shell a login shell.
    let exec = format!("exec -a -{name} {shell}");
    Some(tty::Shell::new(
        "/usr/bin/login".into(),
        vec!["-qflp".into(), user, "/bin/zsh".into(), "-fc".into(), exec],
    ))
}

#[cfg(not(target_os = "macos"))]
fn login_shell() -> Option<tty::Shell> {
    None
}

fn base_env() -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("TERM".into(), TERM_NAME.into());
    // We render 24-bit colour, so say so; otherwise tools quantise to 256.
    env.insert("COLORTERM".into(), "truecolor".into());
    for key in STALE_ENV {
        // The child inherits our environment and the PTY layer offers no
        // removal, so these are blanked. Every consumer of them tests for
        // non-empty.
        env.insert((*key).into(), String::new());
    }
    env
}

impl Spawn {
    /// An interactive shell in `cwd`.
    pub fn shell(cwd: &Path) -> Spawn {
        Spawn {
            program: None,
            args: Vec::new(),
            cwd: Some(cwd.to_path_buf()),
            env: base_env(),
        }
    }

    /// Run one command, then leave the user at a live prompt.
    ///
    /// Not `exit`-ing afterwards is deliberate: a pane that dies the moment the
    /// command finishes takes its output with it, exactly when you want to read
    /// it.
    pub fn command(cmd: &str, cwd: &Path) -> Spawn {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        Spawn {
            program: Some(shell.clone()),
            args: vec!["-c".into(), format!("{cmd}; exec {shell}")],
            cwd: Some(cwd.to_path_buf()),
            env: base_env(),
        }
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Proxy>>>,
    pid: u32,
    notifier: Notifier,
    shared: Arc<Shared>,
    size: Size,
    /// Reused between frames so a steady stream of output does not allocate a
    /// fresh grid sixty times a second.
    scratch: Snapshot,
    search: Option<Search>,
    /// The text of the selection as the user made it; see `prune_selection`.
    selected: Mutex<Option<String>>,
}

/// The block a cell's prompt belongs to: its hyperlink, when the scanner
/// put one there (see `tap.rs`).
fn block_of(cell: &alacritty_terminal::term::cell::Cell) -> Option<u64> {
    let link = cell.hyperlink()?;
    link.uri().strip_prefix(tap::BLOCK_LINK)?.parse().ok()
}

/// A prompt's mark is where the shell started drawing it: the first columns.
const MARK_COLUMNS: usize = 4;

fn line_block(term: &Term<Proxy>, line: Line) -> Option<u64> {
    let row = &term.grid()[line];
    let cols = term.columns().min(MARK_COLUMNS);
    (0..cols).find_map(|c| block_of(&row[Column(c)]))
}

/// The nearest prompt above `from`, looking at most `limit` lines up.
fn block_above(term: &Term<Proxy>, from: Line, limit: usize) -> Option<u64> {
    let top = term.topmost_line().0;
    (top..from.0).rev().take(limit).find_map(|l| line_block(term, Line(l)))
}

/// The SGR parameters (each with its leading `;`) that draw `cell`.
fn sgr_of(cell: &alacritty_terminal::term::cell::Cell) -> String {
    let mut s = String::new();
    let f = cell.flags;
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if f.contains(flag) {
            s.push(';');
            s.push_str(code);
        }
    }
    let color = |c: AnsiColor, fg: bool, s: &mut String| match c {
        AnsiColor::Named(n) => {
            let i = n as usize;
            let base = if fg { 30 } else { 40 };
            if i < 8 {
                s.push_str(&format!(";{}", base + i));
            } else if i < 16 {
                s.push_str(&format!(";{}", base + 60 + i - 8));
            }
        }
        AnsiColor::Indexed(i) => s.push_str(&format!(";{};5;{i}", if fg { 38 } else { 48 })),
        AnsiColor::Spec(rgb) => s.push_str(&format!(";{};2;{};{};{}", if fg { 38 } else { 48 }, rgb.r, rgb.g, rgb.b)),
    };
    color(cell.fg, true, &mut s);
    color(cell.bg, false, &mut s);
    s
}

/// Whether any cell on `line` belongs to block `n`'s prompt or command line.
fn line_has_block(term: &Term<Proxy>, line: Line, n: u64) -> bool {
    let row = &term.grid()[line];
    (0..term.columns()).any(|c| block_of(&row[Column(c)]) == Some(n))
}

/// Every prompt in the scrollback and on screen, top to bottom: the line
/// it starts on and its block.
fn prompt_lines(term: &Term<Proxy>) -> Vec<(i32, u64)> {
    let mut out: Vec<(i32, u64)> = Vec::new();
    for l in term.topmost_line().0..=term.bottommost_line().0 {
        if let Some(n) = line_block(term, Line(l)) {
            if out.last().map(|(_, m)| *m) != Some(n) {
                out.push((l, n));
            }
        }
    }
    out
}

/// Select every line, leaving out the blank rows below the prompt so a copy
/// does not end in a run of empty lines.
fn select_everything(term: &mut Term<Proxy>) {
    use alacritty_terminal::selection::{Selection, SelectionType};
    let top = term.topmost_line();
    let mut bottom = term.bottommost_line();
    while bottom > top && term.grid()[bottom].is_clear() {
        bottom -= 1;
    }
    let mut s = Selection::new(
        SelectionType::Simple,
        Point::new(top, Column(0)),
        Side::Left,
    );
    s.update(Point::new(bottom, term.last_column()), Side::Right);
    term.selection = Some(s);
}

/// The terminal config with `shape` as the default cursor.
fn cursor_config(shape: CursorShape, blinking: bool) -> Config {
    use alacritty_terminal::vte::ansi::{CursorShape as A, CursorStyle};
    let shape = match shape {
        CursorShape::Block => A::Block,
        CursorShape::Beam => A::Beam,
        CursorShape::Underline => A::Underline,
    };
    Config {
        scrolling_history: SCROLLBACK,
        default_cursor_style: CursorStyle { shape, blinking },
        ..Default::default()
    }
}

/// Whether the live selection no longer covers the text that was selected.
fn selection_changed(term: &Term<Proxy>, held: Option<&str>) -> bool {
    term.selection_to_string().as_deref() != held
}

impl Terminal {
    pub fn spawn(spawn: Spawn, columns: u16, rows: u16) -> anyhow::Result<Terminal> {
        let columns = columns.max(2);
        let rows = rows.max(2);
        let size = Size {
            columns: columns as usize,
            screen_lines: rows as usize,
        };
        let window = WindowSize {
            num_cols: columns,
            num_lines: rows,
            // Only used to answer a program's pixel-size query; the cell metrics
            // that matter for layout live in the UI.
            cell_width: 8,
            cell_height: 16,
        };

        // A hand-built `Spawn` still gets the terminal identity; anything the
        // caller set explicitly wins.
        let mut env = base_env();
        env.extend(spawn.env);
        let options = tty::Options {
            shell: match spawn.program {
                Some(program) => Some(tty::Shell::new(program, spawn.args)),
                None => login_shell(),
            },
            working_directory: spawn.cwd,
            drain_on_exit: true,
            env,
        };

        let shared = Arc::new(Shared::new(window));
        let proxy = Proxy(shared.clone());

        let pty = tty::new(&options, window, 0)?;
        let pid = pty.child().id();
        let pty = tap::Tapped::new(pty, shared.clone())?;
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Default::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        let event_loop = EventLoop::new(term.clone(), proxy, pty, options.drain_on_exit, false)?;
        let notifier = Notifier(event_loop.channel());
        // Installed before the loop starts, so the very first query the shell
        // sends already has somewhere to send its answer.
        if let Ok(mut w) = shared.writer.lock() {
            *w = Some(event_loop.channel());
        }
        // The join handle is dropped deliberately: the loop shuts itself down on
        // `Msg::Shutdown` or when the child exits, and keeping the handle would
        // only tempt someone into blocking the UI thread on it.
        let _ = event_loop.spawn();

        Ok(Terminal {
            term,
            pid,
            notifier,
            shared,
            size,
            scratch: Snapshot::default(),
            search: None,
            selected: Mutex::new(None),
        })
    }

    /// The cursor a program gets until it asks for another with DECSCUSR,
    /// and goes back to when it resets (`ESC [ 0 SP q`).
    pub fn set_cursor_default(&self, shape: CursorShape, blinking: bool) {
        self.term.lock().set_options(cursor_config(shape, blinking));
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// True when content has changed since the last snapshot. One atomic load —
    /// cheap enough to call every frame.
    pub fn is_dirty(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }

    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }

    pub fn drain_events(&self) -> Vec<TermEvent> {
        self.shared
            .events
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    pub fn columns(&self) -> usize {
        self.size.columns
    }

    pub fn rows(&self) -> usize {
        self.size.screen_lines
    }

    /// Send bytes to the shell, exactly as a keystroke would.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        use alacritty_terminal::event::Notify;
        self.notifier.notify(bytes);
        // Typing jumps back to the prompt, which is what every terminal does.
        self.term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Positive scrolls towards history, negative towards the live tail.
    pub fn scroll(&self, lines: i32) {
        self.term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn scroll_to_bottom(&self) {
        self.term
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// The process the PTY started (on macOS, `login`, whose child is the shell).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// What deck knows about block `n`.
    pub fn block(&self, n: u64) -> Option<Block> {
        self.shared.blocks.lock().ok()?.get(&n).cloned()
    }

    /// The newest block, the one the prompt at the bottom belongs to.
    pub fn last_block(&self) -> Option<(u64, Block)> {
        let blocks = self.shared.blocks.lock().ok()?;
        blocks.last_key_value().map(|(n, b)| (*n, b.clone()))
    }

    /// The newest block that ran a command: the one running now, or the
    /// last one to finish.
    pub fn last_command(&self) -> Option<Block> {
        let blocks = self.shared.blocks.lock().ok()?;
        blocks.values().rev().find(|b| b.command.is_some()).cloned()
    }

    /// The last `lines` lines of scrollback and screen, as text, without
    /// the blank rows under the prompt.
    pub fn tail_text(&self, lines: usize) -> String {
        let term = self.term.lock();
        let top = term.topmost_line();
        let mut bottom = term.bottommost_line();
        while bottom > top && term.grid()[bottom].is_clear() {
            bottom -= 1;
        }
        let first = Line((bottom.0 - lines as i32 + 1).max(top.0));
        term.bounds_to_string(Point::new(first, Column(0)), Point::new(bottom, term.last_column()))
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The command lines run at this shell, newest first.
    pub fn commands(&self) -> Vec<String> {
        let Ok(blocks) = self.shared.blocks.lock() else {
            return Vec::new();
        };
        blocks.values().rev().filter_map(|b| b.command.clone()).collect()
    }

    /// Whether the shell marks its prompts (deck's integration is loaded).
    pub fn has_blocks(&self) -> bool {
        self.shared.blocks.lock().is_ok_and(|b| !b.is_empty())
    }

    /// The shell's folder, as it last said (OSC 7).
    pub fn shell_cwd(&self) -> Option<String> {
        self.shared.cwd.lock().ok()?.clone()
    }

    /// Scroll so the previous (`up`) or next prompt is at the top. Past
    /// the last one, back to the live tail. Whether anything moved.
    pub fn jump_block(&mut self, up: bool) -> bool {
        use alacritty_terminal::grid::Scroll;
        let mut term = self.term.lock();
        let marks = prompt_lines(&term);
        let offset = term.grid().display_offset() as i32;
        let top = -offset;
        let target = if up {
            marks.iter().rev().find(|(l, _)| *l < top)
        } else {
            marks.iter().find(|(l, _)| *l > top)
        };
        let want = match target {
            Some(&(line, _)) => (-line).max(0),
            None if up => return false,
            None => 0,
        };
        if want == offset {
            return false;
        }
        term.scroll_display(Scroll::Delta(want - offset));
        drop(term);
        self.shared.mark_dirty();
        true
    }

    /// The scrollback and screen above the cursor's line, as text with its
    /// colours (SGR), at most `max_lines` of it, to show again under a new
    /// shell after a relaunch. Palette colours stay palette colours, so a
    /// theme change still applies; soft-wrapped lines stay joined, so they
    /// reflow at a new width. `None` while a full-screen program runs.
    pub fn history_ansi(&self, max_lines: usize) -> Option<String> {
        let term = self.term.lock();
        if term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
            return None;
        }
        let grid = term.grid();
        let cursor = grid.cursor.point.line;
        let mut last = cursor.0 - 1;
        while last >= term.topmost_line().0 && grid[Line(last)].is_clear() {
            last -= 1;
        }
        let first = term.topmost_line().0.max(last - max_lines as i32 + 1);
        if last < first {
            return Some(String::new());
        }
        let cols = term.columns();
        let mut out = String::new();
        let mut style = String::new();
        for l in first..=last {
            let row = &grid[Line(l)];
            // Trailing blank cells say nothing; a wrapped line has none.
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let mut end = cols;
            if !wrapped {
                while end > 0 && {
                    let c = &row[Column(end - 1)];
                    (c.c == ' ' || c.c == '\0') && c.bg == AnsiColor::Named(NamedColor::Background)
                } {
                    end -= 1;
                }
            }
            for c in 0..end {
                let cell = &row[Column(c)];
                if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                    continue;
                }
                let sgr = sgr_of(cell);
                if sgr != style {
                    out.push_str("\x1b[0");
                    out.push_str(&sgr);
                    out.push('m');
                    style = sgr;
                }
                out.push(if cell.c == '\0' { ' ' } else { cell.c });
                for z in cell.zerowidth().unwrap_or(&[]) {
                    out.push(*z);
                }
            }
            if !wrapped {
                if !style.is_empty() {
                    out.push_str("\x1b[0m");
                    style.clear();
                }
                out.push_str("\r\n");
            }
        }
        if !style.is_empty() {
            out.push_str("\x1b[0m\r\n");
        }
        Some(out)
    }

    /// The OSC 8 link a program put on viewport cell `(row, col)`. Deck's
    /// own prompt marks are links too, and are not one.
    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        let term = self.term.lock();
        let link = term.grid()[viewport_point(&term, row, col)].hyperlink()?;
        let uri = link.uri();
        (!uri.starts_with(tap::BLOCK_LINK)).then(|| uri.to_string())
    }

    /// The line through viewport cell `(row, col)` with its soft wraps
    /// joined, one char per column (`'\0'` for a wide char's spacer), and
    /// that cell's index in it: a URL too long for one row is still one URL.
    pub fn wrapped_line_at(&self, row: usize, col: usize) -> (String, usize) {
        // A minified file is one line; a link is never this long.
        const MAX_ROWS: i32 = 32;
        let term = self.term.lock();
        let grid = term.grid();
        let last = term.last_column();
        let at = viewport_point(&term, row, col);
        let wraps = |l: i32| grid[Line(l)][last].flags.contains(Flags::WRAPLINE);
        let mut first = at.line.0;
        while first > term.topmost_line().0 && at.line.0 - first < MAX_ROWS && wraps(first - 1) {
            first -= 1;
        }
        let mut end = at.line.0;
        while end < term.bottommost_line().0 && end - at.line.0 < MAX_ROWS && wraps(end) {
            end += 1;
        }
        let spacer = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        let text = (first..=end)
            .flat_map(|l| grid[Line(l)].into_iter())
            .map(|c| if c.flags.intersects(spacer) { '\0' } else { c.c })
            .collect();
        let ix = (at.line.0 - first) as usize * term.columns() + at.column.0;
        (text, ix)
    }

    /// Scroll so block `n`'s prompt is at the top.
    pub fn scroll_to_block(&mut self, n: u64) -> bool {
        use alacritty_terminal::grid::Scroll;
        let mut term = self.term.lock();
        let Some(&(line, _)) = prompt_lines(&term).iter().find(|(_, m)| *m == n) else {
            return false;
        };
        let offset = term.grid().display_offset() as i32;
        let want = (-line).max(0);
        term.scroll_display(Scroll::Delta(want - offset));
        drop(term);
        self.shared.mark_dirty();
        true
    }

    /// Block `n`'s output, as text: the lines between its command line and
    /// the next prompt, without the blank ones at the end. `None` once it
    /// has left the scrollback.
    pub fn block_output(&self, n: u64) -> Option<String> {
        let term = self.term.lock();
        let marks = prompt_lines(&term);
        let at = marks.iter().position(|(_, m)| *m == n)?;
        // The command line ends where the prompt's link does.
        let mut last = marks[at].0;
        let bottom = term.bottommost_line().0;
        while last < bottom && line_has_block(&term, Line(last + 1), n) {
            last += 1;
        }
        let end = marks.get(at + 1).map(|(l, _)| l - 1).unwrap_or(bottom);
        if end <= last {
            return Some(String::new());
        }
        let text = term.bounds_to_string(
            Point::new(Line(last + 1), Column(0)),
            Point::new(Line(end), term.last_column()),
        );
        Some(text.trim_end().to_string())
    }

    /// A stream that yields whenever the screen changed or an event arrived.
    /// Call [`Terminal::take_wake`] on each one to re-arm it.
    pub fn wakeups(&self) -> futures::channel::mpsc::UnboundedReceiver<()> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        if let Ok(mut w) = self.shared.wake.lock() {
            *w = Some(tx);
        }
        self.shared.wake_pending.store(false, Ordering::Release);
        rx
    }

    pub fn take_wake(&self) {
        self.shared.wake_pending.store(false, Ordering::Release);
    }

    /// Start a selection at a viewport cell. `right` is which half of the cell
    /// was hit, so dragging back over the start cell deselects it.
    pub fn select_start(&self, row: usize, col: usize, right: bool, mode: SelectMode) {
        use alacritty_terminal::selection::{Selection, SelectionType};
        let mut term = self.term.lock();
        let point = viewport_point(&term, row, col);
        let ty = match mode {
            SelectMode::Char => SelectionType::Simple,
            SelectMode::Word => SelectionType::Semantic,
            SelectMode::Line => SelectionType::Lines,
        };
        term.selection = Some(Selection::new(ty, point, side(right)));
        self.hold_selection(&term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn select_to(&self, row: usize, col: usize, right: bool) {
        let mut term = self.term.lock();
        let point = viewport_point(&term, row, col);
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side(right));
        }
        self.hold_selection(&term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Scrollback and screen, down to the last line with anything on it.
    pub fn select_all(&self) {
        let mut term = self.term.lock();
        select_everything(&mut term);
        self.hold_selection(&term);
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn select_clear(&self) {
        let mut term = self.term.lock();
        if let Ok(mut held) = self.selected.lock() {
            *held = None;
        }
        if term.selection.take().is_some() {
            self.shared.dirty.store(true, Ordering::Release);
        }
    }

    /// Remember what the user selected, so [`Terminal::prune_selection`] can
    /// tell when the program has changed it.
    fn hold_selection(&self, term: &Term<Proxy>) {
        if let Ok(mut held) = self.selected.lock() {
            *held = term.selection_to_string();
        }
    }

    /// Drop the selection once the program's output has changed what it
    /// covers. Call after output arrives; returns whether it was dropped.
    ///
    /// The emulator carries a selection along with scrolled text and clamps
    /// it to the scroll region's edges when it would leave them. A full-screen
    /// app that scrolls its own region (Claude Code's transcript) then
    /// stretches the selection further with every step, until the whole
    /// screen is highlighted. Checking the text is still what was selected
    /// catches that and any overwrite, while a selection that simply scrolls
    /// with its text survives, as in iTerm.
    pub fn prune_selection(&self) -> bool {
        let mut term = self.term.lock();
        if term.selection.is_none() {
            return false;
        }
        let Ok(mut held) = self.selected.lock() else {
            return false;
        };
        if !selection_changed(&term, held.as_deref()) {
            return false;
        }
        term.selection = None;
        *held = None;
        self.shared.dirty.store(true, Ordering::Release);
        true
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term
            .lock()
            .selection_to_string()
            .filter(|s| !s.is_empty())
    }

    /// DECCKM: the program (vim, less, fzf) wants `ESC O A` for arrows rather
    /// than `ESC [ A`. Sending the wrong form makes arrows type letters.
    pub fn app_cursor(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// The program asked for pastes to be fenced, so pasted text containing a
    /// newline is not executed line by line.
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Drop all scrollback, as ⌘K does in Terminal.app and iTerm (and as
    /// `ESC [ 3 J` does): the visible screen stays, history and any selection
    /// inside it go, and the view returns to the live tail.
    pub fn clear_scrollback(&self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        let mut term = self.term.lock();
        term.clear_screen(ClearMode::Saved);
        term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        drop(term);
        self.shared.mark_dirty();
    }

    /// Search scrollback for `query` as typed, scrolling to the match. Typing
    /// refines in place: the search starts where the focused match starts,
    /// else at the bottom, newest first. An empty query ends the search.
    /// Returns whether anything matched.
    pub fn search(&mut self, query: &str) -> bool {
        let mut term = self.term.lock();
        let from = self.search.take().and_then(|mut s| {
            s.rebase(&term);
            s.focus.map(|m| *m.start())
        });
        self.shared.dirty.store(true, Ordering::Release);
        if query.is_empty() {
            return false;
        }
        let Some(mut search) = Search::new(query, &term) else {
            return false;
        };
        let origin = from.unwrap_or_else(|| bottom_right(&term));
        let found = search.find(&mut term, origin, Direction::Left);
        self.search = Some(search);
        found
    }

    /// The next match up (towards older output) or down, wrapping.
    pub fn search_step(&mut self, up: bool) -> bool {
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        let found = search.step(&mut self.term.lock(), up);
        self.shared.dirty.store(true, Ordering::Release);
        found
    }

    pub fn search_clear(&mut self) {
        if self.search.take().is_some() {
            self.shared.dirty.store(true, Ordering::Release);
        }
    }

    pub fn mouse_modes(&self) -> MouseModes {
        modes_of(*self.term.lock().mode())
    }

    /// Bytes to send for a click, drag or move at a viewport cell, or `None`
    /// when the program did not ask for this kind of event (the caller then
    /// handles it itself, e.g. as a selection).
    ///
    /// `Motion` with a button is a drag (needs 1002 or 1003); without one it is
    /// plain movement (needs 1003). Press and release need any reporting mode.
    pub fn mouse_report(
        &self,
        button: Option<MouseButton>,
        action: MouseAction,
        row: usize,
        col: usize,
        mods: Mods,
    ) -> Option<Vec<u8>> {
        report_bytes(self.mouse_modes(), button, action, row, col, mods)
    }

    /// What the wheel should send. `lines > 0` scrolls up, toward history.
    ///
    /// A program that reports the mouse gets wheel buttons (64 up, 65 down);
    /// one in the alternate screen with alternate scroll gets arrow keys,
    /// honouring application cursor mode. `None` means nobody asked: scroll our
    /// own scrollback. Repeats are capped at ten per event.
    pub fn scroll_report(&self, lines: i32, row: usize, col: usize, mods: Mods) -> Option<Vec<u8>> {
        use alacritty_terminal::term::TermMode;
        let mode = *self.term.lock().mode();
        scroll_bytes(
            modes_of(mode),
            mode.contains(TermMode::APP_CURSOR),
            lines,
            row,
            col,
            mods,
        )
    }

    /// Resize the grid. A no-op when the dimensions are unchanged, because this
    /// is driven from layout and layout runs constantly.
    pub fn resize(&mut self, columns: u16, rows: u16, cell_width: u16, cell_height: u16) -> bool {
        let columns = columns.max(2) as usize;
        let rows = rows.max(2) as usize;
        if columns == self.size.columns && rows == self.size.screen_lines {
            return false;
        }
        self.size = Size {
            columns,
            screen_lines: rows,
        };
        self.term.lock().resize(self.size);
        let window = WindowSize {
            num_cols: columns as u16,
            num_lines: rows as u16,
            cell_width: cell_width.max(1),
            cell_height: cell_height.max(1),
        };
        if let Ok(mut w) = self.shared.window.lock() {
            *w = window;
        }
        let _ = self.notifier.0.send(Msg::Resize(window));
        self.shared.dirty.store(true, Ordering::Release);
        true
    }

    /// Build a frame, coalescing runs of identically styled cells.
    ///
    /// Returns `false` when nothing has changed and `force` is not set, which is
    /// the case on the overwhelming majority of frames. Read the result with
    /// [`Terminal::last`].
    pub fn snapshot(&mut self, palette: &Palette, force: bool) -> bool {
        if let Ok(mut p) = self.shared.palette.lock() {
            *p = Some(*palette);
        }
        // A switch between a dark and a light theme, told to a program that
        // asked to hear of one, so its colours follow instead of going
        // unreadable on the new background.
        let scheme = palette.scheme();
        let was = self.shared.scheme.swap(scheme, Ordering::Relaxed);
        if was != 0 && was != scheme && self.shared.scheme_updates.load(Ordering::Relaxed) {
            self.shared.send_scheme(scheme);
        }
        if !self.shared.dirty.swap(false, Ordering::AcqRel) && !force {
            return false;
        }

        let term = self.term.lock();
        // Only the viewport is scanned, so a find costs a screenful per frame
        // however long the scrollback is.
        let (matches, focus) = match self.search.as_mut() {
            Some(s) => {
                s.rebase(&term);
                (visible_matches(&term, &mut s.regex), s.focus.clone())
            }
            None => (Vec::new(), None),
        };
        // Cells arrive in grid order, as do matches: one cursor walks both.
        let mut next_match = 0;
        self.scratch.cursor_blink = term.cursor_style().blinking;
        let content = term.renderable_content();
        let columns = self.size.columns;
        let rows = self.size.screen_lines;

        // Reuse the row vector and each row's run vector; clearing keeps the
        // allocations that a scrolling build log would otherwise churn through.
        self.scratch.rows.resize_with(rows, Row::default);
        self.scratch.rows.truncate(rows);
        for row in &mut self.scratch.rows {
            row.runs.clear();
            row.block = None;
        }
        let alt = content
            .mode
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
        self.scratch.columns = columns;
        self.scratch.scrolled_back = content.display_offset;

        let selection = content.selection;
        let cursor_point = content.cursor.point;
        self.scratch.cursor_shape = cursor_shape_of(content.cursor.shape);
        let cursor_visible = content
            .mode
            .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR);
        let top = -(content.display_offset as i32);

        for cell in content.display_iter {
            let Point { line, column } = cell.point;
            let row_ix = line.0 - top;
            if row_ix < 0 || row_ix as usize >= rows || column.0 >= columns {
                continue;
            }
            let flags = cell.flags;
            let row = &mut self.scratch.rows[row_ix as usize];
            if row.block.is_none() && !alt {
                row.block = block_of(&cell);
            }
            // The right half of a wide char has no glyph of its own, but it is
            // still a column: emit `'\0'` in the wide char's run so every later
            // column lines up and its background spans both cells.
            if flags.contains(Flags::WIDE_CHAR_SPACER) {
                if let Some(run) = row.runs.last_mut() {
                    run.text.push('\0');
                    continue;
                }
            }

            let mut fg = palette.resolve(cell.fg);
            let mut bg = palette.resolve(cell.bg);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            while matches
                .get(next_match)
                .is_some_and(|m| *m.end() < cell.point)
            {
                next_match += 1;
            }
            if focus.as_ref().is_some_and(|m| m.contains(&cell.point)) {
                fg = palette.bg;
                bg = palette.search_focus;
            } else if selection.is_some_and(|s| s.contains(cell.point)) {
                bg = palette.selection;
            } else if matches
                .get(next_match)
                .is_some_and(|m| *m.start() <= cell.point)
            {
                bg = palette.search;
            }
            let ch = if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                // A leading spacer pads the last column before a wide char that
                // wrapped; it draws nothing but is a column all the same.
                '\0'
            } else if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };

            let style = Style {
                fg,
                // Comparing against the theme's own background is what lets the
                // renderer skip the fill for the ~95% of cells that have none.
                bg: (bg != palette.bg).then_some(bg),
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
            };

            match row.runs.last_mut() {
                Some(run) if run.style == style => run.text.push(ch),
                _ => row.runs.push(Run {
                    text: ch.to_string(),
                    style,
                }),
            }
        }

        // Which commands are on screen, and which one the top row belongs to
        // when its prompt is above: the one before the first prompt shown;
        // the newest when no prompt is shown at the live tail; else found by
        // looking up the scrollback.
        self.scratch.alt_screen = alt;
        self.scratch.blocks.clear();
        self.scratch.above = None;
        if !alt {
            let blocks = self.shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
            if !blocks.is_empty() {
                let first = self.scratch.rows.iter().position(|r| r.block.is_some());
                self.scratch.above = match first {
                    Some(0) => None,
                    Some(ix) => self.scratch.rows[ix]
                        .block
                        .and_then(|n| n.checked_sub(1))
                        .filter(|n| blocks.contains_key(n)),
                    None if self.scratch.scrolled_back == 0 => blocks.last_key_value().map(|(n, _)| *n),
                    None => block_above(&term, Line(top), 4000),
                };
                let mut seen: Vec<u64> = self.scratch.above.into_iter().collect();
                for n in self.scratch.rows.iter().filter_map(|r| r.block) {
                    if seen.last() != Some(&n) && !seen.contains(&n) {
                        seen.push(n);
                    }
                }
                self.scratch.blocks = seen
                    .into_iter()
                    .filter_map(|n| blocks.get(&n).map(|b| (n, b.clone())))
                    .collect();
            }
        }

        self.scratch.cursor = if cursor_visible {
            let row_ix = cursor_point.line.0 - top;
            (row_ix >= 0 && (row_ix as usize) < rows)
                .then(|| (row_ix as usize, cursor_point.column.0.min(columns)))
        } else {
            None
        };

        drop(term);
        true
    }

    /// The most recent frame, without rebuilding.
    pub fn last(&self) -> &Snapshot {
        &self.scratch
    }

    pub fn shutdown(&self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// External terminals
// ---------------------------------------------------------------------------

/// Terminal emulators worth handing a directory to.
///
/// `libghostty` is not a published, embeddable library, so Ghostty cannot be an
/// in-process backend the way `alacritty_terminal` is. Launching it is the
/// honest version of "use Ghostty" — and it is what people usually want anyway,
/// since they get their own config, fonts and keybindings.
pub const EXTERNAL_TERMINALS: &[(&str, &str)] = &[
    ("ghostty", "ghostty"),
    ("alacritty", "alacritty"),
    ("kitty", "kitty"),
    ("wezterm", "wezterm"),
    ("foot", "foot"),
];

/// The external terminals actually installed on this machine.
pub fn available_externals() -> Vec<&'static str> {
    EXTERNAL_TERMINALS
        .iter()
        .filter(|(_, bin)| which(bin).is_some())
        .map(|(id, _)| *id)
        .collect()
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

/// Open `id` at `cwd`. Returns false when that terminal is not installed.
pub fn open_external(id: &str, cwd: &Path) -> bool {
    let Some((_, bin)) = EXTERNAL_TERMINALS.iter().find(|(name, _)| *name == id) else {
        return false;
    };
    if which(bin).is_none() {
        return false;
    }
    std::process::Command::new(bin)
        .current_dir(cwd)
        .spawn()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette {
            fg: 0xecedf1,
            bg: 0x000000,
            selection: 0x333344,
            search: 0x555555,
            search_focus: 0xeeeeee,
            ansi: [
                0x000000, 0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xcccccc,
                0x555555, 0xff5555, 0x55ff55, 0xffff55, 0x5555ff, 0xff55ff, 0x55ffff, 0xffffff,
            ],
        }
    }

    #[test]
    fn the_colour_cube_maps_the_documented_corners() {
        let p = palette();
        assert_eq!(indexed(&p, 16), 0x000000);
        assert_eq!(indexed(&p, 231), 0xffffff);
        // 196 is pure red in the cube: 196 - 16 = 180 = 5*36 + 0*6 + 0.
        assert_eq!(indexed(&p, 196), 0xff0000);
    }

    #[test]
    fn the_low_sixteen_defer_to_the_theme_rather_than_a_hardcoded_set() {
        let p = palette();
        for i in 0..16u8 {
            assert_eq!(indexed(&p, i), p.ansi[i as usize]);
        }
    }

    #[test]
    fn the_grey_ramp_is_monotonic_and_neutral() {
        let p = palette();
        let mut last = 0;
        for i in 232..=255u8 {
            let v = indexed(&p, i);
            assert!(v > last, "grey ramp went backwards at {i}");
            let (r, g, b) = (v >> 16 & 0xff, v >> 8 & 0xff, v & 0xff);
            assert!(r == g && g == b, "grey {i} is not neutral");
            last = v;
        }
    }

    #[test]
    fn dim_darkens_without_changing_hue_or_overflowing() {
        assert_eq!(dim(0x000000), 0x000000);
        assert_eq!(dim(0xffffff), 0xaaaaaa);
        let d = dim(0x9060c0);
        assert!(d >> 16 & 0xff < 0x90 && d & 0xff < 0xc0);
    }

    #[test]
    fn named_colors_resolve_through_the_supplied_palette() {
        let p = palette();
        assert_eq!(p.resolve(AnsiColor::Named(NamedColor::Red)), 0xff0000);
        assert_eq!(p.resolve(AnsiColor::Named(NamedColor::Foreground)), p.fg);
        assert_eq!(p.resolve(AnsiColor::Named(NamedColor::Background)), p.bg);
        // A dim named colour maps to its base; the DIM *flag* does the dimming,
        // and applying both would darken twice.
        assert_eq!(p.resolve(AnsiColor::Named(NamedColor::DimRed)), 0xff0000);
    }

    #[test]
    fn the_scheme_follows_the_background() {
        let mut p = palette();
        for (bg, scheme) in [(0x000000, 1), (0x0a0a0a, 1), (0x1e1f2b, 1), (0xffffff, 2), (0xf5f5f7, 2)] {
            p.bg = bg;
            assert_eq!(p.scheme(), scheme, "{bg:06x}");
        }
    }

    #[test]
    fn a_direct_rgb_spec_passes_through_untouched() {
        let rgb = alacritty_terminal::vte::ansi::Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        assert_eq!(palette().resolve(AnsiColor::Spec(rgb)), 0x123456);
    }

    #[test]
    fn a_shell_command_leaves_you_at_a_live_prompt() {
        let s = Spawn::command("cargo build", Path::new("/tmp"));
        let script = s.args.last().unwrap();
        assert!(script.starts_with("cargo build"));
        assert!(
            script.contains("exec "),
            "the pane must not die with the command"
        );
    }

    #[test]
    fn external_terminal_lookup_rejects_unknown_ids() {
        assert!(!open_external("not-a-terminal", Path::new("/tmp")));
    }

    #[test]
    fn which_finds_a_binary_that_exists_and_misses_one_that_does_not() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyzzy").is_none());
    }

    /// Spawn a real shell, run a command, read it back out of the grid.
    #[test]
    fn a_spawned_shell_produces_output_that_lands_in_a_snapshot() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'hello-from-pty'; sleep 1".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        // No PTY available in some sandboxes; the pure tests still cover the
        // rest of the crate.
        let Ok(mut term) = Terminal::spawn(spawn, 40, 8) else {
            return;
        };

        let p = palette();
        let mut text = String::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            text = term
                .last()
                .rows
                .iter()
                .flat_map(|r| r.runs.iter().map(|run| run.text.as_str()))
                .collect();
            if text.contains("hello-from-pty") {
                break;
            }
        }
        assert!(
            text.contains("hello-from-pty"),
            "shell output never reached the grid; got {text:?}"
        );
    }

    /// A URL longer than the row wraps; clicking its second half still
    /// finds all of it. A program's OSC 8 link is found under its text.
    #[test]
    fn a_wrapped_line_and_a_hyperlink_read_back_from_their_cells() {
        let url = "https://example.com/a/very/long/path?with=query";
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec![
                "-c".into(),
                format!("printf '{url}\\n\\033]8;;https://x.dev/\\033\\\\link\\033]8;;\\033\\\\'; sleep 1"),
            ],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 20, 8) else {
            return;
        };
        let p = palette();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            if term.hyperlink_at(3, 1).is_some() {
                break;
            }
        }
        let (line, at) = term.wrapped_line_at(1, 4);
        assert_eq!(at, 24);
        assert!(line.starts_with(url), "got {line:?}");
        assert_eq!(term.wrapped_line_at(3, 0).0.trim_end(), "link");
        assert_eq!(term.hyperlink_at(3, 1).as_deref(), Some("https://x.dev/"));
        assert_eq!(term.hyperlink_at(0, 1), None);
    }

    /// A program that asks to hear of theme switches, through a real PTY: it
    /// is told the scheme when it asks, that the mode is on when it checks,
    /// and that the theme turned light when the palette does.
    #[test]
    fn a_program_hears_the_theme_turn_light() {
        let read = |n: u32| format!("$(dd bs=1 count={n} 2>/dev/null | od -An -c | tr -d ' \\n')");
        let script = format!(
            "stty -echo -icanon min 1; printf '\\033[?2031h\\033[?996n'; printf 'first:%s;' \"{}\"; \
             printf '\\033[?2031$p'; printf 'mode:%s;' \"{}\"; printf 'second:%s;' \"{}\"; sleep 1",
            read(9),
            read(11),
            read(9)
        );
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 80, 8) else {
            return;
        };
        let mut wait_for = |p: &Palette, what: &str| {
            let mut text = String::new();
            for _ in 0..150 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                term.snapshot(p, true);
                text = term
                    .last()
                    .rows
                    .iter()
                    .flat_map(|r| r.runs.iter().map(|run| run.text.as_str()))
                    .collect();
                if text.contains(what) {
                    break;
                }
            }
            text
        };
        let dark = palette();
        let text = wait_for(&dark, "mode:");
        assert!(text.contains("first:033[?997;1n;"), "{text:?}");
        assert!(text.contains("mode:033[?2031;1$y;"), "{text:?}");
        let light = Palette { bg: 0xffffff, ..dark };
        let text = wait_for(&light, "second:");
        assert!(text.contains("second:033[?997;2n;"), "{text:?}");
    }

    /// The shell's marks, through a real PTY: each prompt's row carries its
    /// block, the table has the command and its status, the output reads
    /// back, and jumping moves between prompts.
    #[test]
    fn marked_commands_become_blocks_in_the_grid() {
        // As a shell does it: the Enter that ran the command is echoed (the
        // newline) before preexec marks it.
        let script = r"printf '\033]133;A\007$ echo one\r\n\033]633;E;echo one\007\033]133;C\007one\r\n\033]133;D;0\007'; \
            printf '\033]133;A\007$ false\r\n\033]633;E;false\007\033]133;C\007two\r\nthree\r\n\033]133;D;1\007'; \
            printf '\033]133;A\007$ '; sleep 2";
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script.into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 10) else {
            return;
        };
        let p = palette();
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            if term.last().rows.iter().filter(|r| r.block.is_some()).count() >= 3 {
                break;
            }
        }
        let rows: Vec<Option<u64>> = term.last().rows.iter().map(|r| r.block).collect();
        assert_eq!(&rows[..6], &[Some(1), None, Some(2), None, None, Some(3)], "rows: {rows:?}");
        let first = term.block(1).unwrap();
        assert_eq!(first.command.as_deref(), Some("echo one"));
        assert_eq!(first.exit, Some(0));
        let second = term.block(2).unwrap();
        assert_eq!((second.command.as_deref(), second.exit), (Some("false"), Some(1)));
        assert!(term.block(3).unwrap().started.is_none());
        assert_eq!(term.block_output(2).as_deref(), Some("two\nthree"));
        assert_eq!(term.block_output(1).as_deref(), Some("one"));
        // Nothing hidden on screen: every char the program printed is there.
        let text: String = term.last().rows[2].runs.iter().map(|r| r.text.as_str()).collect();
        assert!(text.starts_with("$ "), "{text:?}");
    }

    #[test]
    fn history_comes_back_with_its_colours() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), r"printf 'plain\r\n\033[1;31mred\033[0m and \033[38;2;1;2;3mrgb\033[0m\r\n'; sleep 2".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 30, 6) else {
            return;
        };
        let p = palette();
        let mut text = String::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            text = term.history_ansi(100).unwrap_or_default();
            if text.contains("rgb") {
                break;
            }
        }
        assert!(text.starts_with("plain\r\n"), "{text:?}");
        assert!(text.contains("\x1b[0;1;31mred\x1b[0m and \x1b[0;38;2;1;2;3mrgb\x1b[0m\r\n"), "{text:?}");
    }

    #[test]
    fn adjacent_cells_with_one_style_collapse_into_a_single_run() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'aaaaaaaaaa'; sleep 1".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 8) else {
            return;
        };
        let p = palette();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            if let Some(row) = term
                .last()
                .rows
                .iter()
                .find(|r| r.runs.iter().any(|run| run.text.contains("aaaaaaaaaa")))
            {
                // Ten identical characters must be one run, not ten. This is the
                // whole performance argument for the snapshot layer.
                assert!(
                    row.runs.len() <= 3,
                    "expected the run to coalesce, got {} runs: {:?}",
                    row.runs.len(),
                    row.runs
                );
                return;
            }
        }
    }

    /// Rows must line up with the grid exactly.
    ///
    /// Regression guard for a real bug: output rows were being rendered with the
    /// tail of an *earlier* row glued to the front (`printf` + `X1` came out as
    /// `printfX1`), because the scratch rows were reused without being cleared
    /// for every line the frame did not touch.
    #[test]
    fn each_row_holds_only_its_own_line() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'AAA\\nBBB\\nCCC\\n'; sleep 1".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 8) else {
            return;
        };
        let p = palette();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            let lines: Vec<String> = term
                .last()
                .rows
                .iter()
                .map(|r| {
                    r.runs
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect();
            if lines.iter().any(|l| l == "CCC") {
                let seen: Vec<&String> = lines.iter().filter(|l| !l.is_empty()).collect();
                assert_eq!(
                    seen,
                    vec!["AAA", "BBB", "CCC"],
                    "rows bled into each other: {lines:?}"
                );
                return;
            }
        }
        panic!("the shell never produced its output");
    }

    /// Every column is one char, wide chars included, so column arithmetic in
    /// the renderer (cursor, fills, URL hit-testing) never drifts.
    #[test]
    fn wide_chars_keep_one_char_per_column() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "printf 'a\u{4e2d}b\u{1f600}c'; sleep 1".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 8) else {
            return;
        };
        let p = palette();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            let rows: Vec<Vec<char>> = term
                .last()
                .rows
                .iter()
                .map(|r| r.runs.iter().flat_map(|run| run.text.chars()).collect())
                .collect();
            let Some(row) = rows.iter().find(|r| r.contains(&'c')) else {
                continue;
            };
            for r in &rows {
                assert_eq!(
                    r.len(),
                    40,
                    "every row is exactly one char per column: {r:?}"
                );
            }
            assert_eq!(
                &row[..7],
                &['a', '\u{4e2d}', '\0', 'b', '\u{1f600}', '\0', 'c']
            );
            assert_eq!(row.iter().position(|&c| c == 'b'), Some(3));
            return;
        }
        panic!("the shell never produced its output");
    }

    fn mode_after(bytes: &[u8]) -> alacritty_terminal::term::TermMode {
        let window = WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 8,
            cell_height: 16,
        };
        let proxy = Proxy(Arc::new(Shared::new(window)));
        let size = Size {
            columns: 80,
            screen_lines: 24,
        };
        let mut term = Term::new(Config::default(), &size, proxy);
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, bytes);
        *term.mode()
    }

    #[test]
    fn find_goes_newest_first_wraps_and_follows_a_match_into_history() {
        let window = WindowSize {
            num_cols: 20,
            num_lines: 3,
            cell_width: 8,
            cell_height: 16,
        };
        let size = Size {
            columns: 20,
            screen_lines: 3,
        };
        let config = Config {
            scrolling_history: 100,
            ..Default::default()
        };
        let mut term = Term::new(config, &size, Proxy(Arc::new(Shared::new(window))));
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, b"foo one\r\nFOO two\r\nbar\r\n1+1 foo three\r\n");
        let line = |term: &Term<Proxy>, s: &Search| {
            let at = s.focus.as_ref().expect("a match").start().line;
            let text = term.bounds_to_string(
                Point::new(at, Column(0)),
                Point::new(at, term.last_column()),
            );
            text.trim_end().to_string()
        };

        // Lower case matches any case; newest first, then up, then wrap.
        let mut s = Search::new("foo", &term).unwrap();
        let order: Vec<String> = (0..4)
            .map(|_| {
                assert!(s.step(&mut term, true));
                line(&term, &s)
            })
            .collect();
        assert_eq!(
            order,
            ["1+1 foo three", "FOO two", "foo one", "1+1 foo three"]
        );
        assert!(
            s.step(&mut term, false),
            "down from the newest wraps to the top"
        );
        assert_eq!(line(&term, &s), "foo one");
        assert!(
            term.grid().display_offset() > 0,
            "the view scrolls to a match in history"
        );

        // New output pushes the focused line further back; the focus follows it.
        s.step(&mut term, false);
        assert_eq!(line(&term, &s), "FOO two");
        parser.advance(&mut term, b"x\r\ny\r\n");
        s.rebase(&term);
        assert_eq!(line(&term, &s), "FOO two");

        // Only the matches on screen are highlighted.
        term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        assert_eq!(visible_matches(&term, &mut s.regex).len(), 2);
        term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
        assert_eq!(visible_matches(&term, &mut s.regex).len(), 0);

        // A capital makes it case-sensitive; regex syntax is taken literally.
        let mut s = Search::new("FOO", &term).unwrap();
        s.step(&mut term, true);
        assert_eq!(line(&term, &s), "FOO two");
        s.step(&mut term, true);
        assert_eq!(
            line(&term, &s),
            "FOO two",
            "the only match wraps onto itself"
        );
        let mut s = Search::new("1+1", &term).unwrap();
        assert!(s.step(&mut term, true));
        assert_eq!(line(&term, &s), "1+1 foo three");
        assert!(!Search::new("zzz", &term).unwrap().step(&mut term, true));
    }

    #[test]
    fn decscusr_picks_the_cursor_shape() {
        let shape = |bytes: &[u8]| {
            let window = WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 8,
                cell_height: 16,
            };
            let proxy = Proxy(Arc::new(Shared::new(window)));
            let size = Size {
                columns: 80,
                screen_lines: 24,
            };
            let mut term = Term::new(Config::default(), &size, proxy);
            let mut parser: alacritty_terminal::vte::ansi::Processor =
                alacritty_terminal::vte::ansi::Processor::new();
            parser.advance(&mut term, bytes);
            cursor_shape_of(term.renderable_content().cursor.shape)
        };
        assert_eq!(shape(b""), CursorShape::Block);
        assert_eq!(shape(b"\x1b[6 q"), CursorShape::Beam);
        assert_eq!(shape(b"\x1b[4 q"), CursorShape::Underline);
        assert_eq!(shape(b"\x1b[6 q\x1b[2 q"), CursorShape::Block);
    }

    #[test]
    fn the_default_cursor_holds_until_a_program_asks_and_returns_on_reset() {
        let style = |bytes: &[u8]| {
            let window = WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 8,
                cell_height: 16,
            };
            let proxy = Proxy(Arc::new(Shared::new(window)));
            let size = Size {
                columns: 80,
                screen_lines: 24,
            };
            let mut term = Term::new(cursor_config(CursorShape::Beam, true), &size, proxy);
            let mut parser: alacritty_terminal::vte::ansi::Processor =
                alacritty_terminal::vte::ansi::Processor::new();
            parser.advance(&mut term, bytes);
            (
                cursor_shape_of(term.renderable_content().cursor.shape),
                term.cursor_style().blinking,
            )
        };
        assert_eq!(style(b""), (CursorShape::Beam, true));
        assert_eq!(style(b"\x1b[2 q"), (CursorShape::Block, false));
        assert_eq!(style(b"\x1b[2 q\x1b[0 q"), (CursorShape::Beam, true));
    }

    #[test]
    fn clearing_scrollback_drops_history_and_returns_to_the_tail() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "seq 1 200; printf done; sleep 2".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 8) else {
            return;
        };
        let p = palette();
        let visible = |term: &Terminal| -> String {
            term.last()
                .rows
                .iter()
                .flat_map(|r| r.runs.iter().map(|run| run.text.as_str()))
                .collect()
        };
        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            if visible(&term).contains("done") {
                break;
            }
        }
        assert!(visible(&term).contains("done"), "output never arrived");
        assert!(term.term.lock().grid().history_size() > 100);

        term.scroll(10);
        term.snapshot(&p, true);
        assert_eq!(term.last().scrolled_back, 10);

        term.clear_scrollback();
        assert!(term.is_dirty());
        term.snapshot(&p, true);
        assert_eq!(term.term.lock().grid().history_size(), 0);
        assert_eq!(term.last().scrolled_back, 0);
        assert!(
            visible(&term).contains("done"),
            "the visible screen must survive"
        );
    }

    #[test]
    fn sgr_mouse_encodes_buttons_mods_and_release() {
        let shift_ctrl = Mods {
            shift: true,
            alt: false,
            ctrl: true,
        };
        assert_eq!(
            encode_mouse(true, 0, false, 4, 9).unwrap(),
            b"\x1b[<0;10;5M"
        );
        assert_eq!(encode_mouse(true, 0, true, 4, 9).unwrap(), b"\x1b[<0;10;5m");
        let sgr = MouseModes {
            report: true,
            drag: true,
            sgr: true,
            ..Default::default()
        };
        assert_eq!(
            report_bytes(
                sgr,
                Some(MouseButton::Right),
                MouseAction::Press,
                0,
                0,
                shift_ctrl
            )
            .unwrap(),
            b"\x1b[<22;1;1M"
        );
        assert_eq!(
            report_bytes(
                sgr,
                Some(MouseButton::Left),
                MouseAction::Motion,
                2,
                3,
                Mods::default()
            )
            .unwrap(),
            b"\x1b[<32;4;3M"
        );
        // Plain movement needs 1003; drag alone is not enough.
        assert_eq!(
            report_bytes(sgr, None, MouseAction::Motion, 2, 3, Mods::default()),
            None
        );
        let any = MouseModes {
            motion: true,
            ..sgr
        };
        assert_eq!(
            report_bytes(any, None, MouseAction::Motion, 2, 3, Mods::default()).unwrap(),
            b"\x1b[<35;4;3M"
        );
        // Nothing asked for: the click is ours.
        assert_eq!(
            report_bytes(
                MouseModes::default(),
                Some(MouseButton::Left),
                MouseAction::Press,
                0,
                0,
                Mods::default()
            ),
            None
        );
    }

    #[test]
    fn normal_mouse_encodes_bytes_and_refuses_unrepresentable_cells() {
        assert_eq!(
            encode_mouse(false, 0, false, 0, 0).unwrap(),
            vec![0x1b, b'[', b'M', 32, 33, 33]
        );
        // Release is button 3, modifiers kept.
        assert_eq!(
            encode_mouse(false, 2 | 4, true, 1, 1).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 7, 34, 34]
        );
        assert_eq!(encode_mouse(false, 0, false, 0, 222).unwrap()[4], 255);
        assert_eq!(encode_mouse(false, 0, false, 0, 223), None);
        assert_eq!(encode_mouse(false, 0, false, 300, 0), None);
    }

    #[test]
    fn the_wheel_follows_what_the_program_asked_for() {
        let none = Mods::default();
        // Plain shell: nobody asked, scroll our own history.
        let plain = mode_after(b"");
        assert_eq!(scroll_bytes(modes_of(plain), false, 3, 0, 0, none), None);

        // Alternate screen (less, vim without mouse): arrows, and alternate
        // scroll is on by default.
        let alt = mode_after(b"\x1b[?1049h");
        assert!(modes_of(alt).alt_scroll && !modes_of(alt).report);
        assert_eq!(
            scroll_bytes(modes_of(alt), false, 2, 0, 0, none).unwrap(),
            b"\x1b[A\x1b[A"
        );
        assert_eq!(
            scroll_bytes(modes_of(alt), true, -1, 0, 0, none).unwrap(),
            b"\x1bOB"
        );
        let app_cursor = mode_after(b"\x1b[?1049h\x1b[?1h");
        assert!(app_cursor.contains(alacritty_terminal::term::TermMode::APP_CURSOR));

        // A mouse-reporting program gets wheel buttons, capped per event.
        let sgr = mode_after(b"\x1b[?1000h\x1b[?1006h");
        let m = modes_of(sgr);
        assert!(m.report && m.sgr && !m.drag && !m.motion);
        assert_eq!(
            scroll_bytes(m, false, 1, 4, 9, none).unwrap(),
            b"\x1b[<64;10;5M"
        );
        assert_eq!(
            scroll_bytes(m, false, -2, 0, 0, none).unwrap(),
            b"\x1b[<65;1;1M\x1b[<65;1;1M"
        );
        assert_eq!(
            scroll_bytes(m, false, 50, 0, 0, none).unwrap().len(),
            b"\x1b[<64;1;1M".len() * MAX_SCROLL_REPEAT
        );
        assert_eq!(scroll_bytes(m, false, 0, 0, 0, none), None);

        let drag = modes_of(mode_after(b"\x1b[?1002h"));
        assert!(drag.report && drag.drag && !drag.motion && !drag.sgr);
    }

    #[test]
    fn a_shrinking_grid_drops_the_rows_it_no_longer_has() {
        let Ok(mut term) = Terminal::spawn(Spawn::shell(Path::new("/tmp")), 40, 20) else {
            return;
        };
        let p = palette();
        term.snapshot(&p, true);
        assert_eq!(term.last().rows.len(), 20);
        term.resize(40, 6, 8, 16);
        term.snapshot(&p, true);
        assert_eq!(
            term.last().rows.len(),
            6,
            "a shorter terminal must not keep rendering its old rows"
        );
    }

    /// The regression this file's `TERM_NAME` comment describes.
    ///
    /// Runs under whatever `TERM` the test process inherited — including a tmux
    /// one, which is exactly the case that broke — and asserts the shell's
    /// output is not polluted by a title sequence we do not implement.
    #[test]
    fn a_shell_is_told_what_terminal_it_is_in() {
        let Ok(mut term) = Terminal::spawn(Spawn::shell(Path::new("/tmp")), 60, 8) else {
            return;
        };
        let p = palette();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        term.write(b"printf 'ZZ1\\n'\r".to_vec());

        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, true);
            let rows: Vec<String> = term
                .last()
                .rows
                .iter()
                .map(|r| {
                    r.runs
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        .trim()
                        .to_string()
                })
                .collect();
            if rows.iter().any(|r| r.contains("ZZ1")) {
                let line = rows.iter().find(|r| r.contains("ZZ1")).unwrap();
                // The output line must be the output, not the output with a
                // title-sequence payload glued to the front of it.
                assert!(
                    line == "ZZ1" || line.ends_with("printf 'ZZ1\\n'"),
                    "the shell's output was polluted: {line:?}"
                );
                return;
            }
        }
    }

    #[test]
    fn the_terminal_identity_is_declared_and_stale_multiplexer_vars_are_cleared() {
        let env = base_env();
        assert_eq!(env.get("TERM").map(String::as_str), Some(TERM_NAME));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        for key in STALE_ENV {
            assert_eq!(
                env.get(*key).map(String::as_str),
                Some(""),
                "{key} would tell the shell it is somewhere it is not"
            );
        }
    }

    #[test]
    fn a_clean_terminal_reports_no_work_to_do() {
        let Ok(mut term) = Terminal::spawn(Spawn::shell(Path::new("/tmp")), 20, 4) else {
            return;
        };
        let p = palette();
        // Drain whatever the shell printed on startup.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            term.snapshot(&p, false);
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        term.snapshot(&p, false);
        assert!(
            !term.snapshot(&p, false),
            "an idle terminal must not rebuild its grid every frame"
        );
    }
    #[test]
    fn select_all_takes_scrollback_and_stops_at_the_last_written_line() {
        let window = WindowSize {
            num_cols: 20,
            num_lines: 4,
            cell_width: 8,
            cell_height: 16,
        };
        let size = Size {
            columns: 20,
            screen_lines: 4,
        };
        let mut term = Term::new(
            Config::default(),
            &size,
            Proxy(Arc::new(Shared::new(window))),
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        // Five lines on a four-line screen: "one" has scrolled into history,
        // and the cursor sits on a blank last row.
        parser.advance(&mut term, b"one\r\ntwo\r\nthree\r\nfour\r\n");
        select_everything(&mut term);
        assert_eq!(
            term.selection_to_string().as_deref(),
            Some("one\ntwo\nthree\nfour")
        );
    }

    #[test]
    fn a_selection_stretched_by_a_scrolling_region_is_caught_and_one_that_moves_with_its_text_is_not(
    ) {
        use alacritty_terminal::index::{Column, Line, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        let window = WindowSize {
            num_cols: 20,
            num_lines: 6,
            cell_width: 8,
            cell_height: 16,
        };
        let size = Size {
            columns: 20,
            screen_lines: 6,
        };
        let select = |term: &mut Term<Proxy>, from: (i32, usize), to: (i32, usize)| {
            let mut s = Selection::new(
                SelectionType::Simple,
                Point::new(Line(from.0), Column(from.1)),
                Side::Left,
            );
            s.update(Point::new(Line(to.0), Column(to.1)), Side::Right);
            term.selection = Some(s);
            term.selection_to_string()
        };
        // Rows 1–3 are the app's scroll region, like a transcript between a
        // header and an input box. It scrolls up two lines, as Claude Code
        // does when you wheel through its history.
        let screen: &[u8] = b"head\r\nmsg one\r\nmsg two\r\nmsg three\r\ninput box\r\nstatus";
        let scroll: &[u8] = b"\x1b[2;4r\x1b[4;1H\n\n";

        // Reaching out of the region: the emulator clamps it to the region
        // edge, so it now covers lines the user never selected.
        let mut term = Term::new(
            Config::default(),
            &size,
            Proxy(Arc::new(Shared::new(window))),
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, screen);
        let held = select(&mut term, (3, 0), (4, 8));
        assert!(!selection_changed(&term, held.as_deref()));
        parser.advance(&mut term, scroll);
        assert!(
            selection_changed(&term, held.as_deref()),
            "stretched selection went unnoticed: {:?}",
            term.selection_to_string()
        );

        // Wholly inside the region and scrolled by one line: it moves with
        // its text, so it is still exactly what was selected.
        let mut term = Term::new(
            Config::default(),
            &size,
            Proxy(Arc::new(Shared::new(window))),
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, screen);
        let held = select(&mut term, (3, 0), (3, 8));
        assert_eq!(held.as_deref(), Some("msg three"));
        parser.advance(&mut term, b"\x1b[2;4r\x1b[4;1H\n");
        assert!(
            !selection_changed(&term, held.as_deref()),
            "{:?}",
            term.selection_to_string()
        );
    }
}
