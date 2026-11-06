//! The kernel half of `inspect` on Windows: processes from a ToolHelp
//! snapshot, names and usage from the process itself, listening sockets
//! from the IP helper's owner-pid tables.
//!
//! Two reads go past the documented API, through ntdll:
//! - **argv**: `NtQueryInformationProcess(ProcessCommandLineInformation)`
//!   (Windows 8.1 and later), split the way the MSVC runtime splits it.
//! - **cwd**: the target's PEB, `ProcessParameters.CurrentDirectory`, read
//!   with `ReadProcessMemory`. The offsets are the 64-bit layout, stable
//!   since Vista; a 32-bit (WOW64) process, or one we may not read, gives
//!   `None` and the inspector falls back to what the shell said (OSC 7).
//!
//! Windows has no foreground process group: [`foreground`] is the shell's
//! newest child (what it is running now), else the shell.

use super::{is_version, split_windows_args, Port};
use std::collections::HashSet;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_LISTEN,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    GetProcessTimes, IsWow64Process, OpenProcess, QueryFullProcessImageNameW, PROCESS_ACCESS_RIGHTS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::core::{s, w, BOOL, PWSTR};

/// One row of the process snapshot.
#[derive(Clone)]
struct Row {
    pid: u32,
    ppid: u32,
    /// The executable's file name, `.exe` and all.
    exe: String,
}

/// A snapshot is a walk of every process on the machine; the inspector
/// asks for one per process of a tree, so one is shared for a moment.
const SNAPSHOT_TTL: Duration = Duration::from_millis(250);

fn snapshot() -> Vec<Row> {
    static CACHE: Mutex<Option<(Instant, Vec<Row>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, rows)) = cache.as_ref() {
        if at.elapsed() < SNAPSHOT_TTL {
            return rows.clone();
        }
    }
    let rows = take_snapshot();
    *cache = Some((Instant::now(), rows.clone()));
    rows
}

fn take_snapshot() -> Vec<Row> {
    let mut out = Vec::new();
    // SAFETY: the snapshot handle is closed below; the entry is sized as
    // the API requires.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(Row {
                pid: e.th32ProcessID,
                ppid: e.th32ParentProcessID,
                exe: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// A process handle, closed on drop.
struct Process(HANDLE);

impl Process {
    fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Process> {
        // SAFETY: a plain open; the handle is owned by the result.
        unsafe { OpenProcess(access, false, pid).ok().map(Process) }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub(super) fn parent_of(pid: u32) -> Option<u32> {
    snapshot().into_iter().find(|r| r.pid == pid).map(|r| r.ppid)
}

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Cumulative CPU time in nanoseconds, and the working set in bytes.
pub(super) fn task_usage(pid: u32) -> Option<(Option<u64>, u64)> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: valid handle and out pointers.
    let cpu = unsafe { GetProcessTimes(p.0, &mut created, &mut exited, &mut kernel, &mut user) }
        .ok()
        // FILETIME counts 100 ns.
        .map(|()| (filetime(kernel) + filetime(user)).saturating_mul(100));
    let mut mem = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: valid handle; `mem` is sized in `cb`.
    let rss = unsafe { GetProcessMemoryInfo(p.0, &mut mem, mem.cb) }
        .map(|()| mem.WorkingSetSize as u64)
        .unwrap_or(0);
    Some((cpu, rss))
}

/// When a process started, in FILETIME units, to tell its children apart.
fn started(pid: u32) -> u64 {
    let Some(p) = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return 0;
    };
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: valid handle and out pointers.
    match unsafe { GetProcessTimes(p.0, &mut created, &mut exited, &mut kernel, &mut user) } {
        Ok(()) => filetime(created),
        Err(_) => 0,
    }
}

/// Helpers every console program gets, never what the user is running.
fn is_helper(exe: &str) -> bool {
    let exe = exe.to_ascii_lowercase();
    matches!(exe.as_str(), "conhost.exe" | "openconsole.exe")
}

/// The process in front of `shell`: its newest child that is not a console
/// helper (`node` while Claude Code runs), or the shell itself at a prompt.
pub fn foreground(shell: u32) -> Option<u32> {
    let rows = snapshot();
    if !rows.iter().any(|r| r.pid == shell) {
        return None;
    }
    let newest = rows
        .iter()
        .filter(|r| r.ppid == shell && r.pid != shell && !is_helper(&r.exe))
        .max_by_key(|r| started(r.pid))
        .map(|r| r.pid);
    Some(newest.unwrap_or(shell))
}

/// A process's name, as the inspector lists it.
pub fn process_name(pid: u32) -> String {
    name_of(pid)
}

/// The full path of a process's executable.
fn image_path(pid: u32) -> Option<String> {
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` u16s.
    unsafe { QueryFullProcessImageNameW(p.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

type NtQueryInformationProcess =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

fn nt_query() -> Option<NtQueryInformationProcess> {
    // SAFETY: ntdll is in every process; the transmute is to its documented
    // signature.
    unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let f = GetProcAddress(ntdll, s!("NtQueryInformationProcess"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, NtQueryInformationProcess>(f))
    }
}

/// `UNICODE_STRING` as the kernel lays it out on 64-bit Windows.
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

/// A process's command line, as it was started.
fn command_line(pid: u32) -> Option<String> {
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    let query = nt_query()?;
    let p = Process::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut need = 0u32;
    // SAFETY: a size query with no buffer; then a buffer of that size,
    // aligned for the UNICODE_STRING at its head.
    unsafe {
        query(p.0, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut need);
        if (need as usize) < size_of::<UnicodeString>() {
            return None;
        }
        let mut buf = vec![0u64; (need as usize).div_ceil(8)];
        let status = query(p.0, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr().cast(), need, &mut need);
        if status < 0 {
            return None;
        }
        let us = &*(buf.as_ptr() as *const UnicodeString);
        if us.buffer.is_null() || us.length == 0 {
            return None;
        }
        let text = std::slice::from_raw_parts(us.buffer, us.length as usize / 2);
        Some(String::from_utf16_lossy(text))
    }
}

/// A process's arguments, `argv[0]` first. Empty for one that has exited
/// or that we may not read.
pub fn argv(pid: u32) -> Vec<String> {
    command_line(pid).map(|c| split_windows_args(&c)).unwrap_or_default()
}

pub(super) fn name_of(pid: u32) -> String {
    let exe = snapshot()
        .into_iter()
        .find(|r| r.pid == pid)
        .map(|r| r.exe)
        .or_else(|| image_path(pid).and_then(|p| p.rsplit('\\').next().map(str::to_string)))
        .unwrap_or_default();
    let name = strip_exe(&exe).to_string();
    // Claude Code's binary can be its version (`...\claude\versions\2.1.300`);
    // the name it was started by says what it is.
    if is_version(&name) {
        if let Some(cmd) = argv(pid).first().and_then(|a| a.rsplit(['\\', '/']).next()) {
            if !cmd.is_empty() {
                return strip_exe(cmd).to_string();
            }
        }
    }
    name
}

fn strip_exe(name: &str) -> &str {
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

pub(super) fn child_pids(pid: u32) -> Vec<u32> {
    snapshot()
        .into_iter()
        .filter(|r| r.ppid == pid && r.pid != pid && !is_helper(&r.exe))
        .map(|r| r.pid)
        .collect()
}

/// The current directory of `pid`, from its process parameters. `None` for
/// a 32-bit process, one we may not read, or one that has exited.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(not(target_pointer_width = "64"))]
    {
        let _ = pid;
        return None;
    }
    #[cfg(target_pointer_width = "64")]
    {
        // PROCESS_BASIC_INFORMATION: ExitStatus, PebBaseAddress, ...
        #[repr(C)]
        #[derive(Default)]
        struct BasicInfo {
            exit_status: isize,
            peb: usize,
            affinity: usize,
            priority: isize,
            pid: usize,
            ppid: usize,
        }
        // PEB.ProcessParameters, and in RTL_USER_PROCESS_PARAMETERS the
        // CurrentDirectory.DosPath UNICODE_STRING.
        const PEB_PARAMS: usize = 0x20;
        const PARAMS_CWD: usize = 0x38;

        let query = nt_query()?;
        let p = Process::open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
        let mut wow = BOOL(0);
        // SAFETY: valid handle and out pointer.
        if unsafe { IsWow64Process(p.0, &mut wow) }.is_err() || wow.as_bool() {
            return None;
        }
        let mut info = BasicInfo::default();
        let mut len = 0u32;
        // SAFETY: class 0 (ProcessBasicInformation) fills a BasicInfo.
        let status = unsafe {
            query(p.0, 0, (&mut info as *mut BasicInfo).cast(), size_of::<BasicInfo>() as u32, &mut len)
        };
        if status < 0 || info.peb == 0 {
            return None;
        }
        let read = |addr: usize, out: *mut c_void, size: usize| -> Option<()> {
            let mut got = 0usize;
            // SAFETY: `out` has room for `size` bytes; the read is checked.
            unsafe { ReadProcessMemory(p.0, addr as *const c_void, out, size, Some(&mut got)) }.ok()?;
            (got == size).then_some(())
        };
        let mut params = 0usize;
        read(info.peb + PEB_PARAMS, (&mut params as *mut usize).cast(), size_of::<usize>())?;
        if params == 0 {
            return None;
        }
        let mut dir = UnicodeString { length: 0, maximum_length: 0, buffer: std::ptr::null() };
        read(params + PARAMS_CWD, (&mut dir as *mut UnicodeString).cast(), size_of::<UnicodeString>())?;
        if dir.buffer.is_null() || dir.length == 0 || dir.length > 32 * 1024 {
            return None;
        }
        let mut text = vec![0u16; dir.length as usize / 2];
        read(dir.buffer as usize, text.as_mut_ptr().cast(), dir.length as usize)?;
        let mut path = String::from_utf16_lossy(&text);
        // Kept with a trailing separator, except at a drive's root.
        if path.len() > 3 && (path.ends_with('\\') || path.ends_with('/')) {
            path.pop();
        }
        Some(PathBuf::from(path))
    }
}

/// TCP sockets in LISTEN owned by any of `pids`, sorted by port. An IPv4
/// and an IPv6 listener on one port are one service and appear once.
pub fn listening_ports(pids: &[u32]) -> Vec<Port> {
    let want: HashSet<u32> = pids.iter().copied().collect();
    if want.is_empty() {
        return Vec::new();
    }
    let mut ports = Vec::new();
    let mut seen = HashSet::new();
    let mut names = std::collections::HashMap::new();
    let mut add = |port: u16, pid: u32, addr: String| {
        if !want.contains(&pid) || !seen.insert((port, pid)) {
            return;
        }
        let process = names.entry(pid).or_insert_with(|| name_of(pid)).clone();
        ports.push(Port { port, pid, process, addr });
    };
    for row in table::<MIB_TCPROW_OWNER_PID>(AF_INET.0 as u32) {
        if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
            continue;
        }
        let ip = std::net::Ipv4Addr::from(u32::from_be(row.dwLocalAddr));
        add(port_of(row.dwLocalPort), row.dwOwningPid, show(ip.into()));
    }
    for row in table::<MIB_TCP6ROW_OWNER_PID>(AF_INET6.0 as u32) {
        if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
            continue;
        }
        let ip = std::net::Ipv6Addr::from(row.ucLocalAddr);
        add(port_of(row.dwLocalPort), row.dwOwningPid, show(ip.into()));
    }
    ports.sort_by_key(|p| (p.port, p.pid));
    ports
}

/// The port in a table row: network byte order in the low 16 bits.
fn port_of(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

fn show(addr: std::net::IpAddr) -> String {
    if addr.is_unspecified() { "*".into() } else { addr.to_string() }
}

/// The listening rows of one family's owner-pid TCP table.
fn table<Row: Copy>(family: u32) -> Vec<Row> {
    let mut size = 0u32;
    // SAFETY: a size query, then a buffer of that size (with headroom for
    // sockets opened in between), aligned for the rows; the count is read
    // from its head and bounded by the size returned.
    unsafe {
        let _ = GetExtendedTcpTable(None, &mut size, false, family, TCP_TABLE_OWNER_PID_LISTENER, 0);
        for _ in 0..3 {
            size += 4096;
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let r = GetExtendedTcpTable(Some(buf.as_mut_ptr().cast()), &mut size, false, family, TCP_TABLE_OWNER_PID_LISTENER, 0);
            if r == 122 {
                // ERROR_INSUFFICIENT_BUFFER: it grew; try again.
                continue;
            }
            if r != 0 {
                return Vec::new();
            }
            let base = buf.as_ptr() as *const u8;
            let count = *(base as *const u32) as usize;
            // The rows start after the count, at the rows' alignment.
            let first = align_of::<Row>().max(4);
            let room = (size as usize).saturating_sub(first) / size_of::<Row>();
            let rows = base.add(first) as *const Row;
            return (0..count.min(room)).map(|i| *rows.add(i)).collect();
        }
    }
    Vec::new()
}
