//! The background service's Windows half: jobs under a ConPTY pseudo
//! console in a job object, the attach client's console, and starting the
//! service hidden, now and at login.
//!
//! A job runs through a shell picked from its environment: `SHELL` when it
//! names a program that exists (Git Bash, MSYS: `SHELL -c CMD`), else
//! PowerShell 7 (`pwsh -NoLogo -NoProfile -Command CMD`) when it is on
//! PATH, else Windows PowerShell the same way, else `COMSPEC /D /S /C
//! "CMD"`. No profile or AutoRun is read, as `zsh -c` reads no `.zshrc`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GetConsoleMode, GetConsoleProcessList, GetConsoleScreenBufferInfo,
    GetConsoleWindow, GetStdHandle, ReadConsoleInputW, ResizePseudoConsole, SetConsoleMode, CONSOLE_MODE,
    CONSOLE_SCREEN_BUFFER_INFO, COORD, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, HPCON, INPUT_RECORD,
    KEY_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

/// Handles are kept as integers so `Term` can cross threads.
fn h(v: isize) -> HANDLE {
    HANDLE(v as _)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A job's pseudo console and process. The process is in a job object
/// that kills what is left of its tree when this is dropped.
pub struct Term {
    pub pid: i32,
    process: isize,
    job: isize,
    /// Keys go in here.
    input: isize,
    /// What the job prints comes out here, until the console is closed.
    output: isize,
    /// Closed once: when the process ends, the job is let go, or on drop.
    console: Mutex<Option<isize>>,
}

impl Term {
    /// Start `cmd` in a new pseudo console under the user's shell, in
    /// `cwd`, with exactly `vars` as its environment.
    pub fn spawn(cmd: &str, cwd: &Path, vars: &HashMap<String, String>, rows: u16, cols: u16) -> Result<Term, String> {
        let err = |e: windows::core::Error| e.message();
        let mut line = wide(&command_line(cmd, vars));
        let block = env_block(vars);
        // SAFETY: plain Win32 calls on handles made here; every handle ends
        // up in `term`, whose drop closes it on any early return.
        unsafe {
            let (mut in_r, mut in_w, mut out_r, mut out_w) = (HANDLE::default(), HANDLE::default(), HANDLE::default(), HANDLE::default());
            CreatePipe(&mut in_r, &mut in_w, None, 0).map_err(err)?;
            if let Err(e) = CreatePipe(&mut out_r, &mut out_w, None, 0) {
                let _ = CloseHandle(in_r);
                let _ = CloseHandle(in_w);
                return Err(err(e));
            }
            let hpc = CreatePseudoConsole(coord(rows, cols), in_r, out_w, 0);
            // The console holds its own copies of its ends.
            let _ = CloseHandle(in_r);
            let _ = CloseHandle(out_w);
            let mut term = Term {
                pid: 0,
                process: 0,
                job: 0,
                input: in_w.0 as isize,
                output: out_r.0 as isize,
                console: Mutex::new(None),
            };
            let hpc = hpc.map_err(err)?;
            term.console = Mutex::new(Some(hpc.0));

            let mut size = 0usize;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
            // usize-aligned, as the list holds pointers.
            let mut attrs = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
            let list = LPPROC_THREAD_ATTRIBUTE_LIST(attrs.as_mut_ptr().cast());
            InitializeProcThreadAttributeList(Some(list), 1, None, &mut size).map_err(err)?;
            let made = UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const std::ffi::c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )
            .and_then(|()| {
                let mut si = STARTUPINFOEXW::default();
                si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                // Without these the child takes the service's own standard
                // handles over the pseudo console's.
                si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
                si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
                si.lpAttributeList = list;
                let dir = cwd.is_dir().then(|| HSTRING::from(cwd.as_os_str()));
                let dir = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
                let mut pi = PROCESS_INFORMATION::default();
                CreateProcessW(
                    PCWSTR::null(),
                    Some(PWSTR(line.as_mut_ptr())),
                    None,
                    None,
                    false,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                    Some(block.as_ptr().cast()),
                    dir,
                    &si.StartupInfo,
                    &mut pi,
                )
                .map(|()| pi)
            });
            DeleteProcThreadAttributeList(list);
            let pi = made.map_err(err)?;
            term.process = pi.hProcess.0 as isize;
            term.pid = pi.dwProcessId as i32;
            // In a job object before it runs, so all it starts is in there too.
            if let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) {
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let _ = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const std::ffi::c_void,
                    std::mem::size_of_val(&limits) as u32,
                );
                if AssignProcessToJobObject(job, pi.hProcess).is_ok() {
                    term.job = job.0 as isize;
                } else {
                    let _ = CloseHandle(job);
                }
            }
            ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
            Ok(term)
        }
    }

    /// Output, blocking; 0 at its end.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let mut n = 0u32;
        match unsafe { ReadFile(h(self.output), Some(buf), Some(&mut n), None) } {
            Ok(()) => n as usize,
            Err(_) => 0,
        }
    }

    pub fn write(&self, mut data: &[u8]) {
        while !data.is_empty() {
            let mut n = 0u32;
            if unsafe { WriteFile(h(self.input), Some(data), Some(&mut n), None) }.is_err() || n == 0 {
                return;
            }
            data = &data[n as usize..];
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        if let Some(p) = *self.console.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = unsafe { ResizePseudoConsole(HPCON(p), coord(rows, cols)) };
        }
    }

    /// Wait for the shell to end; its exit code. The console outlives its
    /// last process, so it is closed here (after a moment for what is still
    /// on its way), which ends the output.
    pub fn wait(&self) -> i32 {
        let mut code = 0u32;
        unsafe {
            WaitForSingleObject(h(self.process), INFINITE);
            let _ = GetExitCodeProcess(h(self.process), &mut code);
        }
        std::thread::sleep(Duration::from_millis(200));
        self.close();
        code as i32
    }

    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(h(self.process), 0) == WAIT_TIMEOUT }
    }

    /// Ctrl-C through the console: what a console program takes as "stop".
    pub fn interrupt(&self) {
        self.write(b"\x03");
    }

    /// The whole tree, at once.
    pub fn kill(&self) {
        unsafe {
            if self.job != 0 {
                let _ = TerminateJobObject(h(self.job), 1);
            } else {
                let _ = TerminateProcess(h(self.process), 1);
            }
        }
    }

    /// Close the pseudo console. Never with the jobs locked: the output
    /// reader, which takes that lock, may need to drain it first.
    pub fn close(&self) {
        let p = self.console.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(p) = p {
            unsafe { ClosePseudoConsole(HPCON(p)) };
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.close();
        for v in [self.input, self.output, self.process, self.job] {
            if v != 0 {
                let _ = unsafe { CloseHandle(h(v)) };
            }
        }
    }
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD { X: cols.clamp(1, i16::MAX as u16) as i16, Y: rows.clamp(1, i16::MAX as u16) as i16 }
}

/// A variable of `vars`, by name in any case, as Windows reads them.
fn var<'a>(vars: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// `name` in a folder of `vars`' PATH.
fn on_path(vars: &HashMap<String, String>, name: &str) -> Option<PathBuf> {
    let path = var(vars, "PATH").map(String::from).or_else(|| std::env::var("PATH").ok())?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// The command line that runs `cmd` through the user's shell (see the top
/// of this file for which).
fn command_line(cmd: &str, vars: &HashMap<String, String>) -> String {
    let unix = var(vars, "SHELL").map(PathBuf::from).filter(|p| p.is_file());
    if let Some(sh) = unix {
        return format!("{} -c {}", quote(&sh.to_string_lossy()), quote(cmd));
    }
    let system = var(vars, "SystemRoot").map(String::from).or_else(|| std::env::var("SystemRoot").ok());
    let powershell = on_path(vars, "pwsh.exe").or_else(|| {
        system.as_ref().map(|r| Path::new(r).join(r"System32\WindowsPowerShell\v1.0\powershell.exe")).filter(|p| p.is_file())
    });
    if let Some(ps) = powershell {
        return format!("{} -NoLogo -NoProfile -Command {}", quote(&ps.to_string_lossy()), quote(cmd));
    }
    let comspec = var(vars, "COMSPEC").unwrap_or(r"C:\Windows\System32\cmd.exe");
    // `/S`: cmd drops the outer quotes and runs the rest as typed.
    format!("{} /D /S /C \"{cmd}\"", quote(comspec))
}

/// One argument, quoted the way programs split their command line
/// (CommandLineToArgvW's rules).
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        let n = if c == '"' { slashes * 2 + 1 } else { slashes };
        out.extend(std::iter::repeat_n('\\', n));
        slashes = 0;
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// `vars` as CreateProcessW takes them: sorted by name, case aside,
/// `NAME=value` each ended by a NUL, and one more NUL at the end.
fn env_block(vars: &HashMap<String, String>) -> Vec<u16> {
    let mut pairs: Vec<(&String, &String)> = vars.iter().filter(|(k, _)| !k.is_empty()).collect();
    pairs.sort_by_key(|(k, _)| k.to_uppercase());
    let mut out = Vec::new();
    for (k, v) in pairs {
        out.extend(format!("{k}={v}").replace('\0', "").encode_utf16());
        out.push(0);
    }
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

// -- the service itself ------------------------------------------------------

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
/// Set in the service run again without a console window, so it never
/// does that twice.
const HIDDEN: &str = "DECKD_HIDDEN";

/// Start `exe daemon` on its own with a console that has no window, so
/// nothing flashes and the consoles of the programs it starts (`cmd /C
/// start` for a link) are hidden too. (Detached, it would have no console
/// and each of those would open a window.) Out of Deck's job object when
/// that allows it, so ending Deck's tree does not end the service.
pub fn spawn_service(exe: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let cmd = |flags: u32| {
        let mut c = std::process::Command::new(exe);
        c.arg("daemon")
            .env(HIDDEN, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | flags);
        c
    };
    cmd(CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .or_else(|_| cmd(0).spawn())
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Started at login from the Run key, the service has a console window
/// that only it uses: start it again without one. Whether it did (and this
/// copy should end).
pub fn leave_console_window() -> bool {
    if std::env::var_os(HIDDEN).is_some() {
        return false;
    }
    // SAFETY: plain queries of this process's console.
    let alone = unsafe {
        let mut ids = [0u32; 4];
        !GetConsoleWindow().is_invalid() && GetConsoleProcessList(&mut ids) == 1
    };
    alone && std::env::current_exe().is_ok_and(|exe| spawn_service(&exe).is_ok())
}

/// Start the service at login: `"<exe>" daemon` as the "Deck service"
/// value of the user's Run key, as macOS has its LaunchAgent.
pub fn install_autostart(exe: &Path) {
    let value = wide(&format!("\"{}\" daemon", exe.display()));
    let bytes: Vec<u8> = value.iter().flat_map(|u| u.to_le_bytes()).collect();
    // SAFETY: a key opened here and closed before returning.
    unsafe {
        let mut key = HKEY::default();
        let opened = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if opened.is_ok() {
            let _ = RegSetValueExW(key, w!("Deck service"), None, REG_SZ, Some(&bytes));
            let _ = RegCloseKey(key);
        }
    }
}

// -- the attach client's console ---------------------------------------------

/// The size of this console's window: rows, columns.
pub fn console_size() -> Option<(u16, u16)> {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE).ok()?, &mut info).ok()? };
    let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
    let cols = info.srWindow.Right - info.srWindow.Left + 1;
    (rows > 0 && cols > 0).then_some((rows as u16, cols as u16))
}

/// The console in raw mode: keys as typed (Ctrl-C included) and as VT
/// sequences, output read as VT. Put back as it was when dropped.
pub struct Console {
    saved: Vec<(isize, CONSOLE_MODE)>,
}

impl Console {
    pub fn raw() -> Console {
        let mut saved = Vec::new();
        let mut set = |which, change: fn(u32) -> u32| unsafe {
            let Ok(handle) = GetStdHandle(which) else { return };
            let mut mode = CONSOLE_MODE::default();
            if GetConsoleMode(handle, &mut mode).is_ok() && SetConsoleMode(handle, CONSOLE_MODE(change(mode.0))).is_ok() {
                saved.push((handle.0 as isize, mode));
            }
        };
        set(STD_INPUT_HANDLE, |m| {
            (m & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0)) | ENABLE_VIRTUAL_TERMINAL_INPUT.0
        });
        set(STD_OUTPUT_HANDLE, |m| m | ENABLE_PROCESSED_OUTPUT.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0);
        Console { saved }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        for (handle, mode) in &self.saved {
            let _ = unsafe { SetConsoleMode(h(*handle), *mode) };
        }
    }
}

/// Keys typed at this console, as the bytes a terminal sends.
pub struct Keys {
    handle: Option<isize>,
    /// The first half of a character typed in two key events.
    high: Option<u16>,
}

impl Keys {
    pub fn new() -> Keys {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok().filter(|h| {
            let mut mode = CONSOLE_MODE::default();
            unsafe { GetConsoleMode(*h, &mut mode) }.is_ok()
        });
        Keys { handle: handle.map(|h| h.0 as isize), high: None }
    }

    /// What was typed, waiting up to `ms` (empty if nothing); None at the
    /// end of input. Input that is not a console is read as it comes.
    pub fn next(&mut self, ms: u32) -> Option<Vec<u8>> {
        let Some(handle) = self.handle else {
            let mut buf = [0u8; 4096];
            return match std::io::stdin().read(&mut buf) {
                Ok(0) | Err(_) => None,
                Ok(n) => Some(buf[..n].to_vec()),
            };
        };
        let mut records = [INPUT_RECORD::default(); 128];
        let mut n = 0u32;
        unsafe {
            if WaitForSingleObject(h(handle), ms) != WAIT_OBJECT_0 {
                return Some(Vec::new());
            }
            ReadConsoleInputW(h(handle), &mut records, &mut n).ok()?;
        }
        let mut units: Vec<u16> = self.high.take().into_iter().collect();
        for r in &records[..n as usize] {
            if u32::from(r.EventType) != KEY_EVENT {
                continue;
            }
            // SAFETY: the record says it is a key event.
            let key = unsafe { r.Event.KeyEvent };
            let c = unsafe { key.uChar.UnicodeChar };
            if !key.bKeyDown.as_bool() {
                continue;
            }
            // A console that sends no VT for a key gives only its code.
            let seq: Vec<u16> = match (c, vt_key(key.wVirtualKeyCode)) {
                (0, Some(s)) => s.encode_utf16().collect(),
                (0, None) => continue,
                (c, _) => vec![c],
            };
            for _ in 0..key.wRepeatCount.max(1) {
                units.extend(&seq);
            }
        }
        if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
            self.high = units.pop();
        }
        Some(String::from_utf16_lossy(&units).into_bytes())
    }
}

/// What an xterm sends for a key that types no character, by its virtual
/// key code.
fn vt_key(vk: u16) -> Option<&'static str> {
    Some(match vk {
        0x21 => "\x1b[5~",
        0x22 => "\x1b[6~",
        0x23 => "\x1b[F",
        0x24 => "\x1b[H",
        0x25 => "\x1b[D",
        0x26 => "\x1b[A",
        0x27 => "\x1b[C",
        0x28 => "\x1b[B",
        0x2D => "\x1b[2~",
        0x2E => "\x1b[3~",
        0x70 => "\x1bOP",
        0x71 => "\x1bOQ",
        0x72 => "\x1bOR",
        0x73 => "\x1bOS",
        0x74 => "\x1b[15~",
        0x75 => "\x1b[17~",
        0x76 => "\x1b[18~",
        0x77 => "\x1b[19~",
        0x78 => "\x1b[20~",
        0x79 => "\x1b[21~",
        0x7A => "\x1b[23~",
        0x7B => "\x1b[24~",
        _ => return None,
    })
}

/// Standard output that takes any bytes: a console write needs whole UTF-8
/// characters, and a job's past can start in the middle of one. A cut
/// character is kept for the next write, a bad one shown as U+FFFD.
#[derive(Default)]
pub struct Output {
    tail: Vec<u8>,
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tail.extend_from_slice(buf);
        let mut text = String::new();
        let mut at = 0;
        loop {
            match std::str::from_utf8(&self.tail[at..]) {
                Ok(s) => {
                    text.push_str(s);
                    at = self.tail.len();
                    break;
                }
                Err(e) => {
                    let good = at + e.valid_up_to();
                    text.push_str(std::str::from_utf8(&self.tail[at..good]).unwrap_or_default());
                    match e.error_len() {
                        Some(bad) => {
                            text.push('\u{FFFD}');
                            at = good + bad;
                        }
                        None => {
                            at = good;
                            break;
                        }
                    }
                }
            }
        }
        self.tail.drain(..at);
        std::io::stdout().write_all(text.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}
