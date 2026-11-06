//! [`Engine::Ghostty`]: a terminal run by `libghostty-vt`, the emulator
//! factored out of Ghostty.
//!
//! ## Why this shape
//!
//! libghostty's handles are not thread-safe (`!Send`), so one thread owns
//! the emulator and everything that touches it, and the rest of deck talks
//! to it over a channel:
//!
//! - **reader**: blocking reads from the PTY, through deck's scanner
//!   (`tap.rs`, the same one the Alacritty engine reads through), handed to
//!   the owner;
//! - **owner**: applies output, answers the program's queries, and runs the
//!   UI's calls ([`GhosttyTerminal::call`]) against the emulator, a
//!   snapshot included;
//! - **writer**: keystrokes and replies to the PTY, so a program that stops
//!   reading never blocks the UI or the owner.
//!
//! A call waits for the owner, which only ever runs one chunk of output
//! (at most 64 KiB) before looking at its queue, so the wait is short.

use super::*;
use ghostty_vt as gv;
use ghostty_vt::render::{CellIterator, RowIterator};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::mpsc;

/// Bytes read from the PTY at a time: also the most output the owner
/// applies before answering a call.
const READ_CHUNK: usize = 64 * 1024;

/// Scrollback, in bytes: libghostty bounds it by memory, not lines. About
/// what Alacritty's ten thousand lines hold at a typical width.
const SCROLLBACK_BYTES: usize = 10 * 1024 * 1024;

/// A program that begins synchronized output (mode 2026) and never ends it
/// must not freeze the pane: past this the frame is drawn anyway.
const HOLD_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

type Call = Box<dyn FnOnce(&mut State) + Send>;

enum Msg {
    /// From the PTY, after the scanner.
    Output(Vec<u8>),
    Call(Call),
    /// The child is gone; its exit code when it has one.
    Exited(Option<i32>),
    Shutdown,
}

/// Everything the owner thread holds.
struct State {
    term: gv::Terminal,
    render: gv::RenderState,
    rows: RowIterator,
    cells: CellIterator,
    pty: tty::Pty,
    shared: Arc<Shared>,
    /// The palette last given to the emulator, for its colour answers and
    /// for resolving cells.
    palette: Option<Palette>,
    /// Since when the program has held output back (mode 2026).
    held: Option<std::time::Instant>,
}

pub(crate) struct GhosttyTerminal {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
    pid: u32,
    columns: usize,
    rows: usize,
    scratch: Snapshot,
}

impl GhosttyTerminal {
    pub(crate) fn spawn(spawn: Spawn, columns: u16, rows: u16) -> anyhow::Result<GhosttyTerminal> {
        let columns = columns.max(2);
        let rows = rows.max(2);
        let window = WindowSize {
            num_cols: columns,
            num_lines: rows,
            cell_width: 8,
            cell_height: 16,
        };
        let mut env = base_env();
        env.extend(spawn.env);
        let options = tty::Options {
            shell: match spawn.program {
                Some(program) => Some(tty::Shell::new(program, spawn.args)),
                None => login_shell(&env),
            },
            working_directory: spawn.cwd,
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&options, window, 0)?;
        let pid = pty.child().id();
        // alacritty opens the PTY non-blocking for its poller; these threads
        // block instead. The flag is on the shared file description, which
        // nothing else here reads.
        let fd = pty.file().as_raw_fd();
        // SAFETY: `fd` is the PTY's open descriptor, owned by `pty`.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let mut reader = pty.file().try_clone()?;
        let mut writer = pty.file().try_clone()?;

        let shared = Arc::new(Shared::new(window));
        let (wtx, wrx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("ghostty-pty-write".into()).spawn(move || {
            for bytes in wrx {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        })?;
        if let Ok(mut w) = shared.writer.lock() {
            *w = Some(Box::new(move |bytes| {
                let _ = wtx.send(bytes);
            }));
        }

        let (tx, rx) = mpsc::channel::<Msg>();
        let owner_shared = shared.clone();
        std::thread::Builder::new().name("ghostty-term".into()).spawn(move || {
            match State::new(pty, owner_shared.clone(), columns, rows) {
                Ok(state) => state.run(rx),
                Err(e) => {
                    tracing::warn!("ghostty terminal could not start: {e:?}");
                    owner_shared.exited.store(true, Ordering::Release);
                    if let Ok(mut q) = owner_shared.events.lock() {
                        q.push(TermEvent::Exited(None));
                    }
                    owner_shared.mark_dirty();
                }
            }
        })?;

        let out = tx.clone();
        let tap_shared = shared.clone();
        std::thread::Builder::new().name("ghostty-pty-read".into()).spawn(move || {
            let mut scan = tap::Scanner::default();
            let mut found = Vec::new();
            let mut raw = vec![0u8; READ_CHUNK];
            loop {
                let n = match reader.read(&mut raw) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    // EIO: the child closed its side.
                    Err(_) => break,
                };
                let mut bytes = Vec::with_capacity(n);
                scan.feed(&raw[..n], &mut bytes, &mut found);
                if !found.is_empty() {
                    tap_shared.signals(found.drain(..));
                }
                if !bytes.is_empty() && out.send(Msg::Output(bytes)).is_err() {
                    return;
                }
            }
            let _ = out.send(Msg::Exited(wait_child(pid)));
        })?;

        Ok(GhosttyTerminal {
            tx,
            shared,
            pid,
            columns: columns as usize,
            rows: rows as usize,
            scratch: Snapshot::default(),
        })
    }

    /// Run `f` on the owner thread and wait for its answer. `None` once the
    /// terminal has shut down.
    fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut State) -> R + Send + 'static) -> Option<R> {
        let (rtx, rrx) = mpsc::sync_channel(1);
        self.tx
            .send(Msg::Call(Box::new(move |st| {
                let _ = rtx.send(f(st));
            })))
            .ok()?;
        rrx.recv().ok()
    }

    /// Run `f` on the owner thread without waiting, then redraw.
    fn post(&self, f: impl FnOnce(&mut State) + Send + 'static) {
        let _ = self.tx.send(Msg::Call(Box::new(move |st| {
            f(st);
            st.shared.mark_dirty();
        })));
    }

    fn mode(&self, mode: gv::terminal::Mode) -> bool {
        self.call(move |st| st.term.mode(mode).unwrap_or(false)).unwrap_or(false)
    }
}

/// The child's exit code, once it has exited.
fn wait_child(pid: u32) -> Option<i32> {
    let mut status = 0;
    // SAFETY: waits on our own child; `status` is a valid out-pointer.
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
    (r == pid as libc::pid_t && libc::WIFEXITED(status)).then(|| libc::WEXITSTATUS(status))
}

fn rgb(c: gv::style::RgbColor) -> Rgb {
    ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32
}

fn color(c: Rgb) -> gv::style::RgbColor {
    gv::style::RgbColor {
        r: (c >> 16) as u8,
        g: (c >> 8) as u8,
        b: c as u8,
    }
}

impl State {
    fn new(pty: tty::Pty, shared: Arc<Shared>, columns: u16, rows: u16) -> gv::error::Result<State> {
        let mut term = gv::Terminal::new(gv::terminal::Options {
            cols: columns,
            rows,
            max_scrollback: SCROLLBACK_BYTES,
        })?;
        term.resize(columns, rows, 8, 16)?;
        // Programs are told `xterm-256color` (see `TERM_NAME`).
        term.set_terminfo_name(Some(TERM_NAME))?;
        Ok(State {
            term,
            render: gv::RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            pty,
            shared,
            palette: None,
            held: None,
        })
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Output(bytes) => {
                    self.term.vt_write(&bytes);
                    // Everything already queued goes in before one redraw.
                    let mut next = None;
                    while let Ok(m) = rx.try_recv() {
                        match m {
                            Msg::Output(more) => self.term.vt_write(&more),
                            other => {
                                next = Some(other);
                                break;
                            }
                        }
                    }
                    self.effects();
                    self.shared.mark_dirty();
                    match next {
                        Some(Msg::Call(f)) => f(&mut self),
                        Some(Msg::Exited(code)) => self.exited(code),
                        Some(Msg::Shutdown) => return,
                        Some(Msg::Output(_)) | None => {}
                    }
                }
                Msg::Call(f) => f(&mut self),
                Msg::Exited(code) => self.exited(code),
                Msg::Shutdown => return,
            }
        }
    }

    fn exited(&mut self, code: Option<i32>) {
        self.shared.exited.store(true, Ordering::Release);
        if let Ok(mut q) = self.shared.events.lock() {
            q.push(TermEvent::Exited(code));
        }
        self.shared.mark_dirty();
    }

    /// What the output asked for besides drawing.
    fn effects(&mut self) {
        let mut events = Vec::new();
        for ev in self.term.events() {
            match ev {
                // libghostty doesn't know mode 2031 (deck answers it from the
                // scanner), so its reply to a DECRQM for it is replaced by deck's.
                gv::terminal::Event::PtyWrite(bytes) if bytes.starts_with(b"\x1b[?2031;") => {
                    let state = if self.shared.scheme_updates.load(Ordering::Relaxed) { 1 } else { 2 };
                    self.shared.send(format!("\x1b[?2031;{state}$y").into_bytes());
                }
                gv::terminal::Event::PtyWrite(bytes) => self.shared.send(bytes),
                gv::terminal::Event::Bell => events.push(TermEvent::Bell),
                gv::terminal::Event::TitleChanged(t) => events.push(TermEvent::Title(t)),
                gv::terminal::Event::RenderHold(on) => {
                    self.held = on.then(std::time::Instant::now);
                }
                // The folder, notifications and progress come from the
                // scanner, the same for both engines; clipboard writes are
                // not wired up in either.
                _ => {}
            }
        }
        if !events.is_empty() {
            if let Ok(mut q) = self.shared.events.lock() {
                q.extend(events);
            }
        }
    }

    /// Give the emulator the theme, for its answers to colour queries and
    /// for the colours cells resolve to.
    fn use_palette(&mut self, palette: &Palette) {
        if self.palette == Some(*palette) {
            return;
        }
        self.palette = Some(*palette);
        let mut table = [gv::style::RgbColor { r: 0, g: 0, b: 0 }; 256];
        for (i, c) in table.iter_mut().enumerate() {
            *c = color(indexed(palette, i as u8));
        }
        let _ = self.term.set_default_color_palette(Some(gv::style::Palette(table)));
        let _ = self.term.set_default_fg_color(Some(color(palette.fg)));
        let _ = self.term.set_default_bg_color(Some(color(palette.bg)));
        let _ = self.term.set_default_cursor_color(Some(color(palette.fg)));
    }

    /// Fill `snap` with the viewport. False, and `snap` untouched, while the
    /// program holds output back.
    fn fill(&mut self, snap: &mut Snapshot, palette: &Palette, columns: usize, rows: usize) -> bool {
        self.use_palette(palette);
        if self.held.is_some_and(|t| t.elapsed() < HOLD_LIMIT) {
            return false;
        }
        let alt = self.term.active_screen().ok() == Some(gv::screen::Screen::Alternate);
        let Ok(render) = self.render.update(&self.term) else {
            return false;
        };

        snap.rows.resize_with(rows, Row::default);
        snap.rows.truncate(rows);
        for row in &mut snap.rows {
            row.runs.clear();
            row.block = None;
        }
        snap.columns = columns;
        snap.alt_screen = alt;

        let Ok(mut row_iter) = self.rows.update(&render) else {
            return false;
        };
        let mut y = 0usize;
        while let Some(row_it) = row_iter.next() {
            if y >= rows {
                break;
            }
            let row = &mut snap.rows[y];
            let mut x = 0usize;
            if let Ok(mut cell_iter) = self.cells.update(row_it) {
                while let Some(cell) = cell_iter.next() {
                    if x >= columns {
                        break;
                    }
                    let raw = cell.raw_cell().ok();
                    let wide = raw.and_then(|r| r.wide().ok());
                    // The right half of a wide char: a column with no glyph,
                    // in the wide char's run.
                    if wide == Some(gv::screen::CellWide::SpacerTail) {
                        if let Some(run) = row.runs.last_mut() {
                            run.text.push('\0');
                            x += 1;
                            continue;
                        }
                    }
                    if !alt && x < MARK_COLUMNS && row.block.is_none() && raw.is_some_and(|r| r.has_hyperlink().unwrap_or(false)) {
                        row.block = block_at(&self.term, x, y);
                    }
                    let style = cell.style().ok();
                    let mut fg = cell.fg_color().ok().flatten().map(rgb).unwrap_or(palette.fg);
                    let mut bg = cell.bg_color().ok().flatten().map(rgb).unwrap_or(palette.bg);
                    let st = style.unwrap_or_default();
                    if st.inverse {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                    if st.faint {
                        fg = dim(fg);
                    }
                    if cell.is_selected().unwrap_or(false) {
                        bg = palette.selection;
                    }
                    let ch = if wide == Some(gv::screen::CellWide::SpacerHead) || wide == Some(gv::screen::CellWide::SpacerTail) {
                        '\0'
                    } else if st.invisible {
                        ' '
                    } else {
                        match cell.graphemes_len() {
                            Ok(0) | Err(_) => ' ',
                            Ok(_) => cell.graphemes().ok().and_then(|g| g.first().copied()).unwrap_or(' '),
                        }
                    };
                    let style = Style {
                        fg,
                        bg: (bg != palette.bg).then_some(bg),
                        bold: st.bold,
                        italic: st.italic,
                        underline: st.underline != gv::style::Underline::None,
                    };
                    match row.runs.last_mut() {
                        Some(run) if run.style == style => run.text.push(ch),
                        _ => row.runs.push(Run { text: ch.to_string(), style }),
                    }
                    x += 1;
                }
            }
            // A row shorter than the grid (it shouldn't be) is padded, so
            // columns still line up.
            if x < columns {
                let pad = Style { fg: palette.fg, ..Default::default() };
                row.runs.push(Run { text: " ".repeat(columns - x), style: pad });
            }
            y += 1;
        }

        let cursor = render.cursor().ok();
        snap.cursor_blink = cursor.as_ref().is_some_and(|c| c.blinking);
        snap.cursor_shape = match cursor.as_ref().map(|c| c.visual_style) {
            Some(gv::render::CursorVisualStyle::Bar) => CursorShape::Beam,
            Some(gv::render::CursorVisualStyle::Underline) => CursorShape::Underline,
            _ => CursorShape::Block,
        };
        snap.cursor = cursor
            .filter(|c| c.visible)
            .and_then(|c| c.viewport)
            .map(|v| (v.y as usize, (v.x as usize).min(columns)))
            .filter(|(r, _)| *r < rows);
        snap.scrolled_back = self
            .term
            .scrollbar()
            .map(|s| s.total.saturating_sub(s.offset + s.len) as usize)
            .unwrap_or(0);
        let _ = render.clean();

        fill_blocks(snap, &self.shared);
        true
    }

    /// The viewport rows' text, top to bottom, soft wraps kept as breaks.
    fn plain(&self) -> String {
        let opts = gv::fmt::FormatterOptions::new().with_format(gv::fmt::Format::Plain).with_trim(true);
        gv::fmt::Formatter::new(&self.term, opts)
            .and_then(|mut f| f.format_alloc())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }
}

/// The block whose prompt mark is on viewport cell `(x, y)`.
fn block_at(term: &gv::Terminal, x: usize, y: usize) -> Option<u64> {
    let uri = link_at(term, x, y)?;
    uri.strip_prefix(tap::BLOCK_LINK)?.parse().ok()
}

/// The OSC 8 link on viewport cell `(x, y)`.
fn link_at(term: &gv::Terminal, x: usize, y: usize) -> Option<String> {
    let at = term
        .grid_ref(gv::terminal::Point::Viewport(gv::terminal::PointCoordinate { x: x as u16, y: y as u32 }))
        .ok()?;
    let mut buf = vec![0u8; 256];
    let n = match at.hyperlink_uri(&mut buf) {
        Ok(n) => n,
        Err(gv::error::Error::OutOfSpace { required }) => {
            buf.resize(required, 0);
            at.hyperlink_uri(&mut buf).ok()?
        }
        Err(_) => return None,
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Which commands are on screen, by the rows' marks: the same rules as the
/// Alacritty engine's, except that a view scrolled into history with no
/// prompt in it names no block above.
fn fill_blocks(snap: &mut Snapshot, shared: &Shared) {
    snap.blocks.clear();
    snap.above = None;
    if snap.alt_screen {
        return;
    }
    let blocks = shared.blocks.lock().unwrap_or_else(|e| e.into_inner());
    if blocks.is_empty() {
        return;
    }
    let first = snap.rows.iter().position(|r| r.block.is_some());
    snap.above = match first {
        Some(0) => None,
        Some(ix) => snap.rows[ix].block.and_then(|n| n.checked_sub(1)).filter(|n| blocks.contains_key(n)),
        None if snap.scrolled_back == 0 => blocks.last_key_value().map(|(n, _)| *n),
        None => None,
    };
    let mut seen: Vec<u64> = snap.above.into_iter().collect();
    for n in snap.rows.iter().filter_map(|r| r.block) {
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    snap.blocks = seen.into_iter().filter_map(|n| blocks.get(&n).map(|b| (n, b.clone()))).collect();
}

impl Backend for GhosttyTerminal {
    fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn set_cursor_default(&self, shape: CursorShape, blinking: bool) {
        self.post(move |st| {
            let style = match shape {
                CursorShape::Block => gv::terminal::CursorStyle::Block,
                CursorShape::Beam => gv::terminal::CursorStyle::Bar,
                CursorShape::Underline => gv::terminal::CursorStyle::Underline,
            };
            let _ = st.term.set_default_cursor_style(Some(style));
            let _ = st.term.set_default_cursor_blink(Some(blinking));
        });
    }

    fn write(&self, bytes: Cow<'static, [u8]>) {
        self.shared.send(bytes.into_owned());
        // Typing jumps back to the prompt, which is what every terminal does.
        self.post(|st| st.term.scroll_viewport(gv::terminal::ScrollViewport::Bottom));
    }

    fn scroll(&self, lines: i32) {
        // Deck's positive is towards history; libghostty's is towards the tail.
        self.post(move |st| st.term.scroll_viewport(gv::terminal::ScrollViewport::Delta(-(lines as isize))));
    }

    fn scroll_to_bottom(&self) {
        self.post(|st| st.term.scroll_viewport(gv::terminal::ScrollViewport::Bottom));
    }

    fn tail_text(&self, lines: usize) -> String {
        self.call(move |st| {
            let text = st.plain();
            let all: Vec<&str> = text.lines().map(str::trim_end).collect();
            let mut end = all.len();
            while end > 0 && all[end - 1].is_empty() {
                end -= 1;
            }
            all[end.saturating_sub(lines)..end].join("\n")
        })
        .unwrap_or_default()
    }

    // Prompt navigation and block output need the marks in scrollback,
    // which this engine doesn't search yet.
    fn jump_block(&mut self, _up: bool) -> bool {
        false
    }

    fn history_ansi(&self, _max_lines: usize) -> Option<String> {
        None
    }

    fn hyperlink_at(&self, row: usize, col: usize) -> Option<String> {
        self.call(move |st| link_at(&st.term, col, row))
            .flatten()
            .filter(|uri| !uri.starts_with(tap::BLOCK_LINK))
    }

    fn wrapped_line_at(&self, row: usize, col: usize) -> (String, usize) {
        // The row as the snapshot has it: wrapped links are joined only by
        // the Alacritty engine for now.
        let text = self
            .scratch
            .rows
            .get(row)
            .map(|r| r.runs.iter().map(|run| run.text.as_str()).collect::<String>())
            .unwrap_or_default();
        (text, col)
    }

    fn scroll_to_block(&mut self, _n: u64) -> bool {
        false
    }

    fn block_output(&self, _n: u64) -> Option<String> {
        None
    }

    // Selection and find come next for this engine; until then a drag
    // selects nothing rather than something wrong.
    fn select_start(&self, _row: usize, _col: usize, _right: bool, _mode: SelectMode) {}

    fn select_to(&self, _row: usize, _col: usize, _right: bool) {}

    fn select_all(&self) {}

    fn select_clear(&self) {}

    fn prune_selection(&self) -> bool {
        false
    }

    fn selection_text(&self) -> Option<String> {
        None
    }

    fn app_cursor(&self) -> bool {
        self.mode(gv::terminal::Mode::DECCKM)
    }

    fn bracketed_paste(&self) -> bool {
        self.mode(gv::terminal::Mode::BRACKETED_PASTE)
    }

    fn clear_scrollback(&self) {
        self.post(|st| {
            st.term.vt_write(b"\x1b[3J");
            st.effects();
            st.term.scroll_viewport(gv::terminal::ScrollViewport::Bottom);
        });
    }

    fn search(&mut self, _query: &str) -> bool {
        false
    }

    fn search_step(&mut self, _up: bool) -> bool {
        false
    }

    fn search_clear(&mut self) {}

    fn mouse_modes(&self) -> MouseModes {
        use gv::terminal::Mode as M;
        self.call(|st| {
            let on = |m| st.term.mode(m).unwrap_or(false);
            let alt = st.term.active_screen().ok() == Some(gv::screen::Screen::Alternate);
            MouseModes {
                report: on(M::X10_MOUSE) || on(M::NORMAL_MOUSE) || on(M::BUTTON_MOUSE) || on(M::ANY_MOUSE),
                drag: on(M::BUTTON_MOUSE),
                motion: on(M::ANY_MOUSE),
                sgr: on(M::SGR_MOUSE),
                alt_scroll: alt && on(M::ALT_SCROLL),
            }
        })
        .unwrap_or_default()
    }

    fn resize(&mut self, columns: u16, rows: u16, cell_width: u16, cell_height: u16) -> bool {
        let columns = columns.max(2);
        let rows = rows.max(2);
        if columns as usize == self.columns && rows as usize == self.rows {
            return false;
        }
        self.columns = columns as usize;
        self.rows = rows as usize;
        let window = WindowSize {
            num_cols: columns,
            num_lines: rows,
            cell_width: cell_width.max(1),
            cell_height: cell_height.max(1),
        };
        if let Ok(mut w) = self.shared.window.lock() {
            *w = window;
        }
        self.post(move |st| {
            let _ = st.term.resize(columns, rows, window.cell_width as u32, window.cell_height as u32);
            use alacritty_terminal::event::OnResize;
            st.pty.on_resize(window);
        });
        true
    }

    fn snapshot(&mut self, palette: &Palette, force: bool) -> bool {
        if let Ok(mut p) = self.shared.palette.lock() {
            *p = Some(*palette);
        }
        let scheme = palette.scheme();
        let was = self.shared.scheme.swap(scheme, Ordering::Relaxed);
        if was != 0 && was != scheme && self.shared.scheme_updates.load(Ordering::Relaxed) {
            self.shared.send_scheme(scheme);
        }
        if !self.shared.dirty.swap(false, Ordering::AcqRel) && !force {
            return false;
        }
        let snap = std::mem::take(&mut self.scratch);
        let palette = *palette;
        let (columns, rows) = (self.columns, self.rows);
        match self.call(move |st| {
            let mut snap = snap;
            let changed = st.fill(&mut snap, &palette, columns, rows);
            if !changed && st.held.is_some() {
                // Held: look again once the hold can have ended.
                st.shared.dirty.store(true, Ordering::Release);
            }
            (snap, changed)
        }) {
            Some((snap, changed)) => {
                self.scratch = snap;
                changed
            }
            None => false,
        }
    }

    fn last(&self) -> &Snapshot {
        &self.scratch
    }

    fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

impl Drop for GhosttyTerminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn palette() -> Palette {
        Palette {
            fg: 0xecedf1,
            bg: 0x101010,
            selection: 0x333344,
            search: 0x555555,
            search_focus: 0xeeeeee,
            ansi: [
                0x000000, 0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xcccccc,
                0x555555, 0xff5555, 0x55ff55, 0xffff55, 0x5555ff, 0xff55ff, 0x55ffff, 0xffffff,
            ],
        }
    }

    fn sh(script: &str, columns: u16, rows: u16) -> Option<Terminal> {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script.into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
            engine: Engine::Ghostty,
        };
        // No PTY in some sandboxes.
        let term = Terminal::spawn(spawn, columns, rows).ok()?;
        assert_eq!(term.engine(), Engine::Ghostty);
        Some(term)
    }

    pub(super) fn text(term: &Terminal) -> String {
        text_of(term.last())
    }

    pub(super) fn text_of(snap: &Snapshot) -> String {
        snap
            .rows
            .iter()
            .map(|r| r.runs.iter().map(|run| run.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Snapshot until `done` holds, for up to three seconds.
    fn until(term: &mut Terminal, done: impl Fn(&Terminal) -> bool) -> bool {
        let p = palette();
        for _ in 0..150 {
            term.snapshot(&p, true);
            if done(term) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn output_lands_in_the_snapshot_with_its_colours() {
        let Some(mut term) = sh("printf 'plain \\033[31mred\\033[0m \\033[1mbold\\033[0m'; sleep 2", 40, 6) else {
            return;
        };
        assert!(until(&mut term, |t| text(t).contains("bold")), "got {:?}", text(&term));
        let snap = term.last();
        assert_eq!(snap.columns, 40);
        assert_eq!(snap.rows.len(), 6);
        // Every row is exactly as wide as the grid.
        for r in &snap.rows {
            assert_eq!(r.runs.iter().map(|run| run.text.chars().count()).sum::<usize>(), 40);
        }
        let runs = &snap.rows[0].runs;
        let red = runs.iter().find(|r| r.text.contains("red")).expect("a red run");
        assert_eq!(red.style.fg, 0xff0000);
        let bold = runs.iter().find(|r| r.text.contains("bold")).expect("a bold run");
        assert!(bold.style.bold);
        let plain = runs.iter().find(|r| r.text.contains("plain")).expect("a plain run");
        assert_eq!(plain.style.fg, palette().fg);
        assert_eq!(plain.style.bg, None, "the default background is not drawn");
    }

    #[test]
    fn wide_chars_keep_the_columns_lined_up() {
        let Some(mut term) = sh("printf '漢字x'; sleep 2", 20, 4) else {
            return;
        };
        assert!(until(&mut term, |t| text(t).contains('x')));
        let row: String = term.last().rows[0].runs.iter().map(|r| r.text.as_str()).collect();
        assert!(row.starts_with("漢\0字\0x"), "{row:?}");
    }

    #[test]
    fn scrollback_scrolls_and_clears() {
        let Some(mut term) = sh("i=0; while [ $i -lt 200 ]; do echo line$i; i=$((i+1)); done; echo done; sleep 3", 30, 8) else {
            return;
        };
        assert!(until(&mut term, |t| text(t).contains("done")));
        term.scroll(10);
        assert!(until(&mut term, |t| t.last().scrolled_back == 10), "scrolled {}", term.last().scrolled_back);
        // Rows from history have no background of their own.
        let bgs: Vec<_> = term.last().rows.iter().flat_map(|r| r.runs.iter().filter_map(|run| run.style.bg)).collect();
        assert!(bgs.is_empty(), "history rows carry backgrounds: {bgs:x?}");
        term.scroll_to_bottom();
        assert!(until(&mut term, |t| t.last().scrolled_back == 0));
        term.clear_scrollback();
        term.scroll(10);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(until(&mut term, |t| t.last().scrolled_back == 0));
        assert!(text(&term).contains("done"), "the visible screen must survive");
        assert!(term.tail_text(3).contains("done"));
    }

    #[test]
    fn a_resize_reaches_the_program() {
        let Some(mut term) = sh("sleep 0.3; stty size; sleep 2", 40, 6) else {
            return;
        };
        assert!(term.resize(50, 9, 8, 16));
        assert!(!term.resize(50, 9, 8, 16), "unchanged is a no-op");
        assert!(until(&mut term, |t| text(t).contains("9 50")), "got {:?}", text(&term));
        assert_eq!(term.last().rows.len(), 9);
    }

    #[test]
    fn the_exit_code_is_reported() {
        let Some(mut term) = sh("exit 3", 20, 4) else {
            return;
        };
        assert!(until(&mut term, |t| t.has_exited()));
        assert!(term.drain_events().contains(&TermEvent::Exited(Some(3))));
    }

    #[test]
    fn a_background_query_is_answered_with_the_theme() {
        // The reply comes back as input: echo it, with the escape made visible.
        let script = "stty raw -echo; printf '\\033]11;?\\033\\\\'; dd bs=1 count=24 2>/dev/null | od -An -c | tr -d ' \\n'; sleep 2";
        let Some(mut term) = sh(script, 80, 6) else {
            return;
        };
        term.snapshot(&palette(), true);
        assert!(
            until(&mut term, |t| text(t).contains("rgb:1010/1010/1010")),
            "got {:?}",
            text(&term)
        );
    }

    #[test]
    fn modes_are_read_from_the_emulator() {
        let Some(mut term) = sh("printf '\\033[?1h\\033[?2004h\\033[?1002h\\033[?1006hready'; sleep 2", 20, 4) else {
            return;
        };
        assert!(until(&mut term, |t| text(t).contains("ready")));
        assert!(term.app_cursor());
        assert!(term.bracketed_paste());
        let m = term.mouse_modes();
        assert!(m.report && m.drag && m.sgr && !m.motion, "{m:?}");
    }
}

#[cfg(test)]
mod replay {
    //! Plays a recorded program (`DECK_REPLAY=file`, cut at `DECK_REPLAY_AT`
    //! bytes when set) through both engines, prints where each paints a
    //! background, and compares them. For chasing a difference seen in the
    //! window; skipped without the file.
    use super::*;

    fn play(engine: Engine, file: &str) -> Snapshot {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), format!("stty -echo; cat '{file}'; sleep 1; printf '\\n@END@'; sleep 2")],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
            engine,
        };
        let mut term = Terminal::spawn(spawn, 100, 30).expect("pty");
        let p = tests::palette();
        for _ in 0..200 {
            term.snapshot(&p, true);
            if tests::text(&term).contains("@END@") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        term.snapshot(&p, true);
        term.last().clone()
    }

    /// One char per column: `.` default background, else a letter per
    /// distinct background colour, listed in the returned table.
    fn map(snap: &Snapshot) -> (Vec<String>, Vec<Rgb>) {
        let mut colours: Vec<Rgb> = Vec::new();
        let rows = snap
            .rows
            .iter()
            .map(|r| {
                let mut line = String::new();
                for run in &r.runs {
                    let c = match run.style.bg {
                        None => '.',
                        Some(bg) => {
                            let ix = match colours.iter().position(|c| *c == bg) {
                                Some(ix) => ix,
                                None => {
                                    colours.push(bg);
                                    colours.len() - 1
                                }
                            };
                            (b'A' + ix as u8) as char
                        }
                    };
                    line.extend(run.text.chars().map(|_| c));
                }
                line
            })
            .collect();
        (rows, colours)
    }

    #[test]
    fn both_engines_paint_the_same_backgrounds() {
        let Ok(file) = std::env::var("DECK_REPLAY") else { return };
        let file = match std::env::var("DECK_REPLAY_AT").ok().and_then(|at| at.parse::<usize>().ok()) {
            Some(at) => {
                let bytes = std::fs::read(&file).expect("recording");
                let cut = std::env::temp_dir().join(format!("deck-replay-{}-{at}.bin", std::process::id()));
                std::fs::write(&cut, &bytes[..at.min(bytes.len())]).expect("cut");
                cut.to_string_lossy().into_owned()
            }
            None => file,
        };
        let a = play(Engine::Alacritty, &file);
        let g = play(Engine::Ghostty, &file);
        let (am, ac) = map(&a);
        let (gm, gc) = map(&g);
        println!("alacritty backgrounds {:x?}\nghostty backgrounds {:x?}", ac, gc);
        let gt = tests::text_of(&g);
        for (y, (row, text)) in gm.iter().zip(gt.lines()).enumerate() {
            println!("{y:2} {row} | {}", text.trim_end());
        }
        for (y, (ar, gr)) in am.iter().zip(&gm).enumerate() {
            if ar != gr {
                println!("DIFF row {y:2} A {ar}\nDIFF row {y:2} G {gr}");
            }
        }
        if am != gm {
            println!("--- alacritty text\n{}", tests::text_of(&a));
        }
        assert_eq!(am, gm);
    }
}

#[cfg(test)]
mod alt_screen {
    use super::tests::{palette, text};
    use super::*;

    #[test]
    fn the_alternate_screen_is_never_scrolled_back() {
        let spawn = Spawn {
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "i=0; while [ $i -lt 120 ]; do echo line$i; i=$((i+1)); done; printf '\\033[?1049h\\033[2J\\033[HALT'; sleep 2; printf '\\033[?1049l'; echo back; sleep 2".into()],
            cwd: Some(PathBuf::from("/tmp")),
            env: HashMap::new(),
            engine: Engine::Ghostty,
        };
        let Ok(mut term) = Terminal::spawn(spawn, 40, 10) else { return };
        let p = palette();
        let mut seen = false;
        for _ in 0..150 {
            term.snapshot(&p, true);
            if text(&term).contains("ALT") {
                seen = true;
                let s = term.last();
                assert!(s.alt_screen, "alt screen not reported");
                assert_eq!(s.scrolled_back, 0, "alt screen counted as scrolled back");
                let bgs: Vec<_> = s.rows.iter().flat_map(|r| r.runs.iter().filter_map(|run| run.style.bg)).collect();
                assert!(bgs.is_empty(), "alt screen cells carry backgrounds: {bgs:x?}");
            }
            if text(&term).contains("back") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen, "the program never reached the alternate screen");
        assert!(!term.last().alt_screen);
        assert_eq!(term.last().scrolled_back, 0);
    }
}
