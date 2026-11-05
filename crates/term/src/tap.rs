//! A look at the bytes on their way to the emulator, for the escape codes
//! alacritty's parser drops but deck acts on: desktop notifications (OSC 9 as
//! iTerm2 has it, OSC 777 as Ghostty and rxvt do) and progress (OSC 9;4, as
//! ConEmu and Windows Terminal have it). That is how an agent CLI says it
//! needs you, or that it is working.
//!
//! The PTY is wrapped, not replaced: alacritty still polls it and writes
//! to it; the reader it is handed reads the PTY's own output (a descriptor
//! on Unix, ConPTY's pipe on Windows) and scans the bytes first.

use crate::Shared;
use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Pty};
use polling::{Event, PollMode, Poller};
use std::io::{self, Read};
use std::sync::Arc;

/// What a program said that deck acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Notify {
        title: Option<String>,
        body: String,
    },
    /// OSC 9;4: `Some(state)` while working (1 a percentage, 2 an error,
    /// 3 indeterminate, 4 paused), `None` once it clears; and the
    /// percentage when it gives one.
    Progress(Option<u8>, Option<u8>),
    /// OSC 133;A: the shell is about to draw a prompt, the start of block
    /// `n` (numbered here, from 1).
    Prompt(u64),
    /// OSC 633;E: the command line block `n` runs (VS Code's mark; ours
    /// comes from zsh's preexec).
    Command(u64, String),
    /// OSC 133;C: block `n`'s command started; its output follows.
    Started(u64),
    /// OSC 133;D: block `n`'s command finished, with its exit status.
    Finished(u64, Option<i32>),
    /// OSC 7: the shell's folder, as a path.
    Cwd(String),
    /// Deck's own (OSC 6973;ask): "# …" typed at the prompt, a request for
    /// a command.
    Ask(String),
    /// Deck's own (OSC 6973;report;STATE;MESSAGE): a program says how it
    /// is doing: working, blocked (on you), done, error, or clear.
    Report(String, String),
    /// Deck's own (OSC 6973;line): the line being edited at the prompt and
    /// where the cursor is in it, in characters; `None` once it is done.
    Line(Option<(usize, String)>),
    /// CSI ? 2031 h / l: the program wants, or no longer wants, to hear when
    /// the theme turns dark or light (`CSI ? 997 ; 1|2 n`). Claude Code's and
    /// Neovim's automatic themes listen for it, as they do in Ghostty and kitty.
    SchemeUpdates(bool),
    /// CSI ? 996 n: is the theme dark or light right now?
    SchemeQuery,
}

/// An OSC longer than this is not one we want, and is passed on unread.
const MAX_OSC: usize = 4096;

/// A CSI's parameters past this are not looked at (they still pass through).
const MAX_CSI: usize = 32;

#[derive(Default, Clone, Copy, PartialEq)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    /// An ESC inside an OSC: `\` ends it, anything else abandons it.
    OscEsc,
    /// Inside an OSC too long to be ours (an inline image, a clipboard
    /// write): passed on as it arrives, up to its end.
    OscPass,
    OscPassEsc,
    /// Inside a CSI: passed on as it arrives, its parameters kept to spot the
    /// two about the colour scheme.
    Csi,
}

/// A byte-at-a-time recogniser that survives a sequence split across reads.
///
/// It sits between the PTY and the emulator and copies everything through,
/// holding back only an OSC still being read. Two of the shell's marks are
/// rewritten on the way: a prompt (133;A) opens an OSC 8 hyperlink to
/// `deck:block:N` and a command starting (133;C) closes it, so the prompt and
/// the command line typed into it carry block N's number *in the grid*. That
/// number then moves with its cells through scrolling, resizing and reflow,
/// and leaves with them when scrollback is trimmed; the emulator does all
/// the bookkeeping. Everything else about a block (its command, timing and
/// exit status) is kept beside the grid, under the same number.
#[derive(Default)]
pub struct Scanner {
    state: State,
    /// The OSC's text, between `ESC ]` and its terminator.
    buf: Vec<u8>,
    /// The OSC's bytes as they came, to pass on if it isn't ours.
    raw: Vec<u8>,
    /// The last block number given out.
    seq: u64,
    /// The CSI's parameter and intermediate bytes, up to [`MAX_CSI`].
    csi: Vec<u8>,
}

/// The hyperlink scheme a prompt's cells carry: `deck:block:N`.
pub const BLOCK_LINK: &str = "deck:block:";

impl Scanner {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>, found: &mut Vec<Signal>) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                // Plain output is nearly everything: copy up to the next ESC
                // at memchr speed rather than looking at every byte.
                State::Ground => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(at) => {
                        out.extend_from_slice(&bytes[i..i + at]);
                        i += at + 1;
                        self.state = State::Esc;
                    }
                    None => {
                        out.extend_from_slice(&bytes[i..]);
                        return;
                    }
                },
                State::Esc => {
                    let b = bytes[i];
                    i += 1;
                    match b {
                        b']' => {
                            self.buf.clear();
                            self.raw.clear();
                            self.raw.extend_from_slice(b"\x1b]");
                            self.state = State::Osc;
                        }
                        b'[' => {
                            out.extend_from_slice(b"\x1b[");
                            self.csi.clear();
                            self.state = State::Csi;
                        }
                        0x1b => out.push(0x1b),
                        _ => {
                            out.push(0x1b);
                            out.push(b);
                            self.state = State::Ground;
                        }
                    }
                }
                State::Osc => {
                    let b = bytes[i];
                    i += 1;
                    self.raw.push(b);
                    match b {
                        0x07 => self.end(out, found),
                        0x1b => self.state = State::OscEsc,
                        // CAN and SUB abandon a sequence.
                        0x18 | 0x1a => {
                            out.append(&mut self.raw);
                            self.state = State::Ground;
                        }
                        _ => {
                            self.buf.push(b);
                            if self.buf.len() > MAX_OSC {
                                out.append(&mut self.raw);
                                self.buf.clear();
                                self.state = State::OscPass;
                            }
                        }
                    }
                }
                State::OscEsc => {
                    let b = bytes[i];
                    if b == b'\\' {
                        i += 1;
                        self.raw.push(b);
                        self.end(out, found);
                    } else {
                        // Abandoned: what came so far goes on as it was, and
                        // the ESC starts over (it may open the next OSC).
                        self.raw.pop();
                        out.append(&mut self.raw);
                        self.state = State::Esc;
                    }
                }
                State::OscPass => match memchr::memchr2(0x07, 0x1b, &bytes[i..]) {
                    Some(at) => {
                        let b = bytes[i + at];
                        out.extend_from_slice(&bytes[i..=i + at]);
                        i += at + 1;
                        self.state = if b == 0x07 { State::Ground } else { State::OscPassEsc };
                    }
                    None => {
                        out.extend_from_slice(&bytes[i..]);
                        return;
                    }
                },
                State::OscPassEsc => {
                    out.push(bytes[i]);
                    i += 1;
                    self.state = State::Ground;
                }
                // SGR and cursor moves are most of these: copy the parameters
                // through in one go, then look at the final byte.
                State::Csi => {
                    let rest = &bytes[i..];
                    let end = rest.iter().position(|b| !(0x20..=0x3f).contains(b));
                    let n = end.unwrap_or(rest.len());
                    out.extend_from_slice(&rest[..n]);
                    let room = MAX_CSI.saturating_sub(self.csi.len());
                    self.csi.extend_from_slice(&rest[..n.min(room)]);
                    i += n;
                    if end.is_none() {
                        return;
                    }
                    let b = bytes[i];
                    i += 1;
                    match b {
                        // Abandoned; the ESC may open the next sequence.
                        0x1b => self.state = State::Esc,
                        0x40..=0x7e => {
                            out.push(b);
                            self.state = State::Ground;
                            found.extend(scheme(&self.csi, b));
                        }
                        0x18 | 0x1a => {
                            out.push(b);
                            self.state = State::Ground;
                        }
                        // A control inside a CSI is carried out as usual.
                        _ => out.push(b),
                    }
                }
            }
        }
    }

    fn end(&mut self, out: &mut Vec<u8>, found: &mut Vec<Signal>) {
        self.state = State::Ground;
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        match parse(&text) {
            Some(Mark::Prompt) => {
                self.seq += 1;
                let seq = self.seq;
                out.extend_from_slice(format!("\x1b]8;id=deck{seq};{BLOCK_LINK}{seq}\x1b\\").as_bytes());
                found.push(Signal::Prompt(seq));
                self.raw.clear();
            }
            Some(Mark::Started) => {
                out.extend_from_slice(b"\x1b]8;;\x1b\\");
                found.push(Signal::Started(self.seq));
                self.raw.clear();
            }
            Some(Mark::Finished(code)) => {
                found.push(Signal::Finished(self.seq, code));
                self.raw.clear();
            }
            Some(Mark::Command(cmd)) => {
                found.push(Signal::Command(self.seq, cmd));
                self.raw.clear();
            }
            Some(Mark::Signal(signal)) => {
                found.push(signal);
                out.append(&mut self.raw);
            }
            None => out.append(&mut self.raw),
        }
    }
}

/// An OSC deck reads: a mark it rewrites or drops, or a signal it passes on.
enum Mark {
    Prompt,
    Started,
    Finished(Option<i32>),
    Command(String),
    Signal(Signal),
}

fn parse(osc: &str) -> Option<Mark> {
    if let Some(rest) = osc.strip_prefix("133;") {
        let mut it = rest.split(';');
        return match it.next()? {
            "A" => Some(Mark::Prompt),
            "C" => Some(Mark::Started),
            "D" => Some(Mark::Finished(it.next().and_then(|c| c.trim().parse().ok()))),
            _ => None,
        };
    }
    if let Some(ask) = osc.strip_prefix("6973;ask;") {
        let ask = ask.trim();
        return (!ask.is_empty()).then(|| Mark::Signal(Signal::Ask(ask.to_string())));
    }
    if let Some(rest) = osc.strip_prefix("6973;report;") {
        let (state, msg) = rest.split_once(';').unwrap_or((rest, ""));
        let state = state.trim().to_lowercase();
        return matches!(state.as_str(), "working" | "blocked" | "done" | "error" | "clear")
            .then(|| Mark::Signal(Signal::Report(state, msg.trim().to_string())));
    }
    if let Some(line) = osc.strip_prefix("6973;line;") {
        let (cursor, text) = line.split_once(';')?;
        return Some(Mark::Signal(Signal::Line(
            cursor.parse().ok().map(|c| (c, unescape_633(text))),
        )));
    }
    if let Some(cmd) = osc.strip_prefix("633;E;") {
        return Some(Mark::Command(unescape_633(cmd)));
    }
    if let Some(url) = osc.strip_prefix("7;") {
        // file://host/path, the path percent-encoded.
        let path = url.strip_prefix("file://")?;
        let path = &path[path.find('/')?..];
        return Some(Mark::Signal(Signal::Cwd(percent_decode(path))));
    }
    if let Some(rest) = osc.strip_prefix("9;") {
        if let Some(progress) = rest.strip_prefix("4;") {
            let mut it = progress.split(';');
            let state = it.next()?.parse::<u8>().ok()?;
            let percent = it.next().and_then(|v| v.trim().parse::<u8>().ok()).map(|v| v.min(100));
            return Some(Mark::Signal(Signal::Progress((state != 0).then_some(state), percent)));
        }
        let body = rest.trim();
        return (!body.is_empty()).then(|| {
            Mark::Signal(Signal::Notify {
                title: None,
                body: body.to_string(),
            })
        });
    }
    if let Some(rest) = osc.strip_prefix("777;notify;") {
        let (title, body) = rest.split_once(';').unwrap_or(("", rest));
        let (title, body) = (title.trim(), body.trim());
        if body.is_empty() && title.is_empty() {
            return None;
        }
        return Some(Mark::Signal(Signal::Notify {
            title: (!title.is_empty()).then(|| title.to_string()),
            body: body.to_string(),
        }));
    }
    None
}

/// VS Code's escaping in 633;E: `\\` for a backslash, `\xAB` for a byte.
fn unescape_633(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if b[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if b[i + 1] == b'x' && i + 3 < b.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The reader alacritty is handed: what the PTY says, after the scanner.
pub struct Tap {
    /// alacritty's PTY, read here and handed back for polling and writes.
    pty: Pty,
    scan: Scanner,
    found: Vec<Signal>,
    /// Read from the PTY, before the scanner.
    raw: Vec<u8>,
    /// After the scanner, waiting for alacritty to take it.
    pending: Vec<u8>,
    taken: usize,
    shared: Arc<Shared>,
}

impl Read for Tap {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.taken < self.pending.len() {
                let n = buf.len().min(self.pending.len() - self.taken);
                buf[..n].copy_from_slice(&self.pending[self.taken..self.taken + n]);
                self.taken += n;
                if self.taken == self.pending.len() {
                    self.pending.clear();
                    self.taken = 0;
                }
                return Ok(n);
            }
            // Nothing to hand over: read more. All of it can be held back
            // (an OSC split across reads), and 0 would mean the end, so
            // read again until there is something, or nothing to read.
            let n = self.pty.reader().read(&mut self.raw)?;
            if n == 0 {
                return Ok(0);
            }
            self.scan.feed(&self.raw[..n], &mut self.pending, &mut self.found);
            if !self.found.is_empty() {
                self.shared.signals(self.found.drain(..));
            }
        }
    }
}

/// alacritty's PTY with [`Tap`] as its reader.
pub struct Tapped {
    reader: Tap,
}

impl Tapped {
    pub fn new(pty: Pty, shared: Arc<Shared>) -> io::Result<Tapped> {
        Ok(Tapped {
            reader: Tap {
                pty,
                scan: Scanner::default(),
                found: Vec::new(),
                raw: vec![0; 64 * 1024],
                pending: Vec::new(),
                taken: 0,
                shared,
            },
        })
    }
}

impl EventedReadWrite for Tapped {
    type Reader = Tap;
    type Writer = <Pty as EventedReadWrite>::Writer;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        event: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // SAFETY: the PTY is owned here and outlives its registration, as
        // alacritty's own contract asks.
        unsafe { self.reader.pty.register(poll, event, mode) }
    }

    fn reregister(&mut self, poll: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.reader.pty.reregister(poll, event, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.reader.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Tap {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.reader.pty.writer()
    }
}

impl EventedPty for Tapped {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.reader.pty.next_child_event()
    }
}

impl OnResize for Tapped {
    fn on_resize(&mut self, size: WindowSize) {
        self.reader.pty.on_resize(size);
    }
}

/// The colour-scheme request a CSI makes, if it is one: DEC mode 2031 set or
/// reset (possibly among other modes), or the DSR 996 query.
fn scheme(params: &[u8], fin: u8) -> Option<Signal> {
    let p = params.strip_prefix(b"?")?;
    match fin {
        b'h' | b'l' if p.split(|&c| c == b';').any(|m| m == b"2031") => {
            Some(Signal::SchemeUpdates(fin == b'h'))
        }
        b'n' if p == b"996" => Some(Signal::SchemeQuery),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Signal> {
        through(chunks).1
    }

    /// What reaches the emulator, and what deck heard.
    fn through(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Signal>) {
        let mut s = Scanner::default();
        let (mut out, mut found) = (Vec::new(), Vec::new());
        for c in chunks {
            s.feed(c, &mut out, &mut found);
        }
        (out, found)
    }

    #[test]
    fn everything_else_passes_through_unchanged() {
        let input: &[&[u8]] = &[
            b"plain \x1b[31mred\x1b[0m \x1b]0;title\x07 \x1b]8;;http://a\x1b\\link\x1b]8;;\x07",
            b" \x1b]52;c;aGk=\x07 \x1b\x1b[1m bold \x1b]9;half\x1b[31m ",
            b"end\x1b",
            b"[0m",
        ];
        let (out, _) = through(input);
        assert_eq!(out, input.concat());
        // An OSC too long to read still arrives whole.
        let long = [b"\x1b]1337;File=:".as_slice(), &vec![b'A'; MAX_OSC * 3], b"\x07after"].concat();
        let split: Vec<&[u8]> = long.chunks(1000).collect();
        assert_eq!(through(&split).0, long);
    }

    #[test]
    fn the_prompt_line_is_heard() {
        assert_eq!(
            scan(&[b"\x1b]6973;line;6;git ch\x07", b"\x1b]6973;line;3;a\\x3bb\x07\x1b]6973;line;;\x07"]),
            vec![
                Signal::Line(Some((6, "git ch".into()))),
                Signal::Line(Some((3, "a;b".into()))),
                Signal::Line(None),
            ]
        );
    }

    #[test]
    fn shell_marks_become_blocks() {
        let (out, found) = through(&[
            b"\x1b]133;D;0\x07\x1b]7;file://mac/Users/me/my%20app\x07\x1b]133;A\x07$ ",
            b"\x1b]633;E;npm test \\x3b ls\x07\x1b]13",
            b"3;C\x07PASS\r\n\x1b]133;D;1\x07",
        ]);
        assert_eq!(
            found,
            vec![
                Signal::Finished(0, Some(0)),
                Signal::Cwd("/Users/me/my app".into()),
                Signal::Prompt(1),
                Signal::Command(1, "npm test ; ls".into()),
                Signal::Started(1),
                Signal::Finished(1, Some(1)),
            ]
        );
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "\x1b]7;file://mac/Users/me/my%20app\x07\x1b]8;id=deck1;deck:block:1\x1b\\$ \x1b]8;;\x1b\\PASS\r\n"
        );
    }

    #[test]
    fn notifications_in_both_dialects_and_split_reads() {
        assert_eq!(
            scan(&[b"hi \x1b]9;Claude needs your permission to use Bash\x07 x"]),
            vec![Signal::Notify {
                title: None,
                body: "Claude needs your permission to use Bash".into()
            }]
        );
        assert_eq!(
            scan(&[
                b"\x1b]777;notify;Claude Code;",
                b"Waiting for your input\x1b",
                b"\\"
            ]),
            vec![Signal::Notify {
                title: Some("Claude Code".into()),
                body: "Waiting for your input".into()
            }]
        );
    }

    #[test]
    fn progress_sets_and_clears() {
        assert_eq!(
            scan(&[b"\x1b]9;4;3;\x07\x1b]9;4;1;40\x1b\\\x1b]9;4;0;\x07"]),
            vec![
                Signal::Progress(Some(3), None),
                Signal::Progress(Some(1), Some(40)),
                Signal::Progress(None, None)
            ]
        );
    }

    #[test]
    fn other_sequences_and_aborts_are_ignored() {
        // A title, a hyperlink, an OSC abandoned by CSI, and a lone ESC.
        assert!(scan(&[
            b"\x1b]0;\xe2\x9c\xb3 topic\x07\x1b]8;;http://a\x07x\x1b]8;;\x07",
            b"\x1b]9;half\x1b[31m red \x1b",
        ])
        .is_empty());
        let long = [b"\x1b]9;".as_slice(), &vec![b'a'; MAX_OSC + 10], b"\x07"].concat();
        assert!(scan(&[&long]).is_empty());
    }

    #[test]
    fn colour_scheme_requests_are_heard_and_passed_on() {
        let input: &[&[u8]] = &[
            b"\x1b[?2031h\x1b[38;2;1;2;3mx\x1b[?99",
            b"6n\x1b[?1049;2031l\x1b[",
            b"?2031$p\x1b[2031h\x1b[?2031\x1b[?996n",
        ];
        let (out, found) = through(input);
        assert_eq!(out, input.concat());
        assert_eq!(
            found,
            vec![
                Signal::SchemeUpdates(true),
                Signal::SchemeQuery,
                Signal::SchemeUpdates(false),
                // DECRQM, a non-private 2031 and a CSI abandoned by ESC are not
                // requests; the query after them is.
                Signal::SchemeQuery,
            ]
        );
    }
}
