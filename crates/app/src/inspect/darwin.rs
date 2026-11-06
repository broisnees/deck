//! The kernel half of `inspect` on macOS: processes, working directories
//! and listening sockets through libproc and sysctl.

use super::{is_version, Port};
use std::collections::HashSet;
use std::ffi::{c_int, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Working directory
// ---------------------------------------------------------------------------

const PROC_PIDVNODEPATHINFO: c_int = 9;
const MAXPATHLEN: usize = 1024;

/// `struct vnode_info_path` from <sys/proc_info.h>: a 152-byte `vnode_info`
/// (kept opaque, u64-aligned like the real one) followed by the path.
#[repr(C)]
struct VnodeInfoPath {
    _vip_vi: [u64; 19],
    vip_path: [u8; MAXPATHLEN],
}

#[repr(C)]
struct ProcVnodePathInfo {
    pvi_cdir: VnodeInfoPath,
    _pvi_rdir: VnodeInfoPath,
}

extern "C" {
    // libproc and mach_time live in libSystem, which every binary links.
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidfdinfo(
        pid: c_int,
        fd: c_int,
        flavor: c_int,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    fn sysctl(
        name: *mut c_int,
        namelen: u32,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> c_int;
}

const PROC_PIDLISTFDS: c_int = 1;
const PROC_PIDTBSDINFO: c_int = 3;
const PROC_PIDFDSOCKETINFO: c_int = 3;
const PROC_PIDTASKINFO: c_int = 4;
const PROC_PIDT_SHORTBSDINFO: c_int = 13;
const PROC_PPID_ONLY: u32 = 6;
const PROX_FDTYPE_SOCKET: u32 = 2;
const SOCKINFO_TCP: i32 = 2;
const TSI_S_LISTEN: i32 = 1;
const INI_IPV6: u8 = 2;

/// `struct proc_taskinfo` (96 bytes). Times are in mach absolute units, which
/// are not nanoseconds on Apple silicon (the timebase there is 125/3).
#[repr(C)]
#[derive(Default)]
struct ProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    _counters: [i32; 12],
}

/// `struct proc_bsdshortinfo` (64 bytes): readable for any process, unlike
/// the task info, which is why it is the liveness and parent check.
#[repr(C)]
#[derive(Default)]
struct ProcBsdShortInfo {
    pbsi_pid: u32,
    pbsi_ppid: u32,
    pbsi_pgid: u32,
    pbsi_status: u32,
    pbsi_comm: [u8; 16],
    pbsi_flags: u32,
    _ids: [u32; 6],
    _rfu: u32,
}

/// `struct proc_fdinfo` (8 bytes).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcFdInfo {
    proc_fd: i32,
    proc_fdtype: u32,
}

/// `struct socket_fdinfo` (792 bytes), kept as bytes and read at fixed
/// offsets: the real type is a deep tree of unions, and only five fields of
/// it matter. Offsets are from <sys/proc_info.h> and pinned by a test.
#[repr(C, align(8))]
struct SocketFdInfo([u8; SOCKET_FDINFO_SIZE]);

const SOCKET_FDINFO_SIZE: usize = 792;
/// `psi` (socket_info) starts at 24; inside it `soi_kind` is at 232 and the
/// protocol union at 240, whose TCP arm is `tcp_sockinfo`: an `in_sockinfo`
/// at 0 (local port at 4, `insi_vflag` at 24, local address at 48) and
/// `tcpsi_state` at 80.
const SOI_KIND: usize = 24 + 232;
const INSI: usize = 24 + 240;
const INSI_LPORT: usize = INSI + 4;
const INSI_VFLAG: usize = INSI + 24;
const INSI_LADDR: usize = INSI + 48;
const TCPSI_STATE: usize = INSI + 80;

#[repr(C)]
#[derive(Default)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

fn pidinfo<T: Default>(pid: u32, flavor: c_int) -> Option<T> {
    let mut v = T::default();
    let size = std::mem::size_of::<T>() as c_int;
    // SAFETY: `v` is a plain-integer struct of exactly `size` bytes, and the
    // kernel writes at most `size` bytes into it.
    let n = unsafe { proc_pidinfo(pid as c_int, flavor, 0, (&mut v as *mut T).cast(), size) };
    (n == size).then_some(v)
}

fn short_info(pid: u32) -> Option<ProcBsdShortInfo> {
    pidinfo::<ProcBsdShortInfo>(pid, PROC_PIDT_SHORTBSDINFO).filter(|i| i.pbsi_pid == pid)
}

pub(super) fn parent_of(pid: u32) -> Option<u32> {
    short_info(pid).map(|i| i.pbsi_ppid)
}

/// Cumulative CPU time in nanoseconds, and resident bytes.
pub(super) fn task_usage(pid: u32) -> Option<(Option<u64>, u64)> {
    let ti = pidinfo::<ProcTaskInfo>(pid, PROC_PIDTASKINFO)?;
    let ticks = ti.pti_total_user.saturating_add(ti.pti_total_system);
    Some((Some(ticks_to_ns(ticks)), ti.pti_resident_size))
}

fn ticks_to_ns(ticks: u64) -> u64 {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let &(numer, denom) = TB.get_or_init(|| {
        let mut tb = MachTimebaseInfo::default();
        // SAFETY: writes one small struct we own.
        let ok = unsafe { mach_timebase_info(&mut tb) } == 0 && tb.denom != 0;
        if ok {
            (tb.numer as u64, tb.denom as u64)
        } else {
            (1, 1)
        }
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

/// `struct proc_bsdinfo` (136 bytes); only the terminal fields are read.
#[repr(C)]
pub(super) struct ProcBsdInfo {
    _ids: [u32; 12],
    _comm: [u8; 16],
    _name: [u8; 32],
    _nfiles: u32,
    _pgid: u32,
    _pjobc: u32,
    _tdev: u32,
    /// The foreground process group of the process's terminal.
    tpgid: u32,
    _nice: i32,
    _start_sec: u64,
    _start_usec: u64,
}

/// The process in front of `shell`'s terminal: the leader of its
/// foreground process group (`claude` while Claude Code runs, the shell
/// itself at a prompt). Exact, unlike guessing from the process tree, where
/// an agent's helpers sit below it.
pub fn foreground(shell: u32) -> Option<u32> {
    // SAFETY: an all-zero proc_bsdinfo is valid; the kernel fills it.
    let mut info: ProcBsdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<ProcBsdInfo>() as c_int;
    // SAFETY: `info` is exactly the size passed.
    let n = unsafe {
        proc_pidinfo(
            shell as c_int,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut ProcBsdInfo).cast(),
            size,
        )
    };
    (n == size && info.tpgid != 0).then_some(info.tpgid)
}

/// A process's name, as the inspector lists it.
pub fn process_name(pid: u32) -> String {
    name_of(pid)
}

/// A process's arguments, `argv[0]` first. Empty for one we may not read
/// (another user's) or that has exited.
pub fn argv(pid: u32) -> Vec<String> {
    const CTL_KERN: c_int = 1;
    const KERN_PROCARGS2: c_int = 49;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
    let mut size = 0usize;
    let null = std::ptr::null_mut();
    // SAFETY: a size query; the kernel writes only `size`.
    if unsafe { sysctl(mib.as_mut_ptr(), 3, null, &mut size, null, 0) } != 0 || size < 4 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    // SAFETY: the kernel writes at most `size` bytes and updates `size`.
    if unsafe {
        sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            null,
            0,
        )
    } != 0
    {
        return Vec::new();
    }
    buf.truncate(size);
    parse_procargs(&buf)
}

/// KERN_PROCARGS2: `argc`, the executable's path, NUL padding, then the
/// arguments (and the environment after them).
pub(super) fn parse_procargs(buf: &[u8]) -> Vec<String> {
    let Some(argc) = buf
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(i32::from_ne_bytes)
    else {
        return Vec::new();
    };
    let rest = &buf[4..];
    let Some(path_end) = rest.iter().position(|&b| b == 0) else {
        return Vec::new();
    };
    let rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0).unwrap_or(rest.len());
    rest[start..]
        .split(|&b| b == 0)
        .take(argc.max(0) as usize)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

pub(super) fn name_of(pid: u32) -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the kernel writes at most `buf.len()` bytes and returns the length.
    let n = unsafe { proc_name(pid as c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n > 0 {
        let name = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        // Claude Code's binary is its version (`…/claude/versions/2.1.300`);
        // the name it was started by says what it is.
        if is_version(&name) {
            if let Some(cmd) = argv(pid).first().and_then(|a| a.rsplit('/').next()) {
                if !cmd.is_empty() {
                    return cmd.to_string();
                }
            }
        }
        return name;
    }
    // Another user's process (setuid `login`): the 16-byte `comm` is public.
    short_info(pid)
        .map(|i| {
            let comm = &i.pbsi_comm;
            let len = comm.iter().position(|&b| b == 0).unwrap_or(comm.len());
            String::from_utf8_lossy(&comm[..len]).into_owned()
        })
        .unwrap_or_default()
}

pub(super) fn child_pids(pid: u32) -> Vec<u32> {
    let mut buf = vec![0i32; 64];
    loop {
        let bytes = (buf.len() * 4) as c_int;
        // SAFETY: the buffer holds `bytes` bytes; the call returns how many it
        // filled (bytes, not a count).
        let n = unsafe { proc_listpids(PROC_PPID_ONLY, pid, buf.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        // A full buffer may have been truncated; grow and ask again.
        if n >= bytes && buf.len() < 1 << 16 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return buf[..n as usize / 4]
            .iter()
            .filter(|&&p| p > 0)
            .map(|&p| p as u32)
            .collect();
    }
}

fn fds(pid: u32) -> Vec<ProcFdInfo> {
    // SAFETY: a null buffer asks for the size needed.
    let need = unsafe { proc_pidinfo(pid as c_int, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return Vec::new();
    }
    // Slack for descriptors opened between the two calls.
    let mut buf = vec![ProcFdInfo::default(); need as usize / 8 + 16];
    let bytes = (buf.len() * 8) as c_int;
    // SAFETY: `buf` holds `bytes` bytes of plain-integer structs.
    let n = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDLISTFDS,
            0,
            buf.as_mut_ptr().cast(),
            bytes,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    buf.truncate(n as usize / 8);
    buf
}

/// The local port and address of `fd` if it is a TCP socket in LISTEN.
fn tcp_listener(pid: u32, fd: i32) -> Option<(u16, String)> {
    let mut info = SocketFdInfo([0; SOCKET_FDINFO_SIZE]);
    // SAFETY: the buffer is exactly the struct's size; the kernel writes at
    // most that many bytes.
    let n = unsafe {
        proc_pidfdinfo(
            pid as c_int,
            fd,
            PROC_PIDFDSOCKETINFO,
            info.0.as_mut_ptr().cast(),
            SOCKET_FDINFO_SIZE as c_int,
        )
    };
    if n != SOCKET_FDINFO_SIZE as c_int {
        return None;
    }
    let b = &info.0;
    let i32_at = |o: usize| i32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    if i32_at(SOI_KIND) != SOCKINFO_TCP || i32_at(TCPSI_STATE) != TSI_S_LISTEN {
        return None;
    }
    // The port is stored in network order in the low 16 bits of an int.
    let port = u16::from_be(i32_at(INSI_LPORT) as u16);
    let raw: [u8; 16] = b[INSI_LADDR..INSI_LADDR + 16].try_into().ok()?;
    let addr = if b[INSI_VFLAG] & INI_IPV6 != 0 {
        let a = std::net::Ipv6Addr::from(raw);
        if a.is_unspecified() {
            "*".to_string()
        } else {
            a.to_string()
        }
    } else {
        // `in4in6_addr`: 12 bytes of padding, then the IPv4 address.
        let a = std::net::Ipv4Addr::new(raw[12], raw[13], raw[14], raw[15]);
        if a.is_unspecified() {
            "*".to_string()
        } else {
            a.to_string()
        }
    };
    Some((port, addr))
}

/// The current directory of `pid`, as the kernel tracks it: this follows a
/// shell's `cd` without asking the shell anything.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    let mut info = std::mem::MaybeUninit::<ProcVnodePathInfo>::zeroed();
    let size = std::mem::size_of::<ProcVnodePathInfo>() as c_int;
    // SAFETY: the buffer is exactly `size` bytes and the kernel writes at most
    // that many; zeroed memory is a valid value of this all-integer struct.
    let n = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: fully initialised: zeroed, then filled by a successful call.
    let info = unsafe { info.assume_init() };
    let raw = &info.pvi_cdir.vip_path;
    let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if len == 0 {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..len])))
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// TCP sockets in LISTEN owned by any of `pids`, sorted by port. An IPv4
/// and an IPv6 listener on one port are one service and appear once.
pub fn listening_ports(pids: &[u32]) -> Vec<Port> {
    let mut ports = Vec::new();
    let mut seen = HashSet::new();
    for &pid in pids {
        let mut name = None;
        for fd in fds(pid) {
            if fd.proc_fdtype != PROX_FDTYPE_SOCKET {
                continue;
            }
            let Some((port, addr)) = tcp_listener(pid, fd.proc_fd) else {
                continue;
            };
            if !seen.insert((port, pid)) {
                continue;
            }
            let process = name.get_or_insert_with(|| name_of(pid)).clone();
            ports.push(Port {
                port,
                pid,
                process,
                addr,
            });
        }
    }
    ports.sort_by_key(|p| (p.port, p.pid));
    ports
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libproc_struct_sizes_match_the_sdk() {
        // From `sizeof` in a C program built against <sys/proc_info.h>.
        assert_eq!(std::mem::size_of::<ProcTaskInfo>(), 96);
        assert_eq!(std::mem::size_of::<ProcBsdShortInfo>(), 64);
        assert_eq!(std::mem::size_of::<ProcFdInfo>(), 8);
        assert_eq!(std::mem::size_of::<SocketFdInfo>(), 792);
    }
}
