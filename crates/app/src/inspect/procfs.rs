//! The kernel half of `inspect` on Linux: processes, working directories
//! and listening sockets, read from /proc. Elsewhere (Windows, for now)
//! there is no /proc, so every read comes back empty.

use super::{is_version, Port};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// The fields of `/proc/<pid>/stat` after the command name, which is in
/// parentheses and may itself hold spaces and parentheses. Index 0 is the
/// state, the third field of the file.
fn stat(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &raw[raw.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Field `n` of the stat file, counted as proc(5) counts them (pid is 1).
fn field(stat: &[String], n: usize) -> Option<u64> {
    stat.get(n.checked_sub(3)?)?.parse().ok()
}

pub(super) fn parent_of(pid: u32) -> Option<u32> {
    stat(pid).and_then(|s| field(&s, 4)).map(|p| p as u32)
}

/// Cumulative CPU time in nanoseconds, and resident bytes.
pub(super) fn task_usage(pid: u32) -> Option<(Option<u64>, u64)> {
    let s = stat(pid)?;
    let ticks = field(&s, 14)?.saturating_add(field(&s, 15)?);
    let rss = field(&s, 24)?.saturating_mul(page_size());
    Some((Some(ticks.saturating_mul(1_000_000_000) / clock_ticks()), rss))
}

#[cfg(target_os = "linux")]
fn clock_ticks() -> u64 {
    // SAFETY: sysconf reads a constant.
    let n = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if n > 0 { n as u64 } else { 100 }
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf reads a constant.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 { n as u64 } else { 4096 }
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks() -> u64 {
    100
}

#[cfg(not(target_os = "linux"))]
fn page_size() -> u64 {
    4096
}

/// The process in front of `shell`'s terminal: the leader of its
/// foreground process group (`claude` while Claude Code runs, the shell
/// itself at a prompt).
pub fn foreground(shell: u32) -> Option<u32> {
    let tpgid = stat(shell).and_then(|s| field(&s, 8))?;
    (tpgid != 0 && tpgid != u32::MAX as u64).then_some(tpgid as u32)
}

/// A process's name, as the inspector lists it.
pub fn process_name(pid: u32) -> String {
    name_of(pid)
}

/// A process's arguments, `argv[0]` first. Empty for one that has exited.
pub fn argv(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    raw.split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

pub(super) fn name_of(pid: u32) -> String {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default();
    // Claude Code's binary is its version (`…/claude/versions/2.1.300`);
    // the name it was started by says what it is.
    if is_version(&name) {
        if let Some(cmd) = argv(pid).first().and_then(|a| a.rsplit('/').next()) {
            if !cmd.is_empty() {
                return cmd.to_string();
            }
        }
    }
    name
}

/// Each thread's `children` file (CONFIG_PROC_CHILDREN, on in every
/// distribution kernel); a scan of /proc when it is missing.
pub(super) fn child_pids(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut found = false;
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for t in tasks.flatten() {
            if let Ok(kids) = std::fs::read_to_string(t.path().join("children")) {
                found = true;
                out.extend(kids.split_whitespace().filter_map(|k| k.parse::<u32>().ok()));
            }
        }
    }
    if found {
        return out;
    }
    let Ok(all) = std::fs::read_dir("/proc") else {
        return out;
    };
    all.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&p| parent_of(p) == Some(pid))
        .collect()
}

/// The current directory of `pid`, as the kernel tracks it: this follows a
/// shell's `cd` without asking the shell anything.
pub fn cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// TCP sockets in LISTEN owned by any of `pids`, sorted by port. An IPv4
/// and an IPv6 listener on one port are one service and appear once.
pub fn listening_ports(pids: &[u32]) -> Vec<Port> {
    let listeners = tcp_listeners();
    if listeners.is_empty() {
        return Vec::new();
    }
    let mut ports = Vec::new();
    let mut seen = HashSet::new();
    for &pid in pids {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        let mut name = None;
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = link
                .to_str()
                .and_then(|l| l.strip_prefix("socket:["))
                .and_then(|l| l.strip_suffix(']'))
                .and_then(|l| l.parse::<u64>().ok())
            else {
                continue;
            };
            let Some((port, addr)) = listeners.get(&inode) else {
                continue;
            };
            if !seen.insert((*port, pid)) {
                continue;
            }
            let process = name.get_or_insert_with(|| name_of(pid)).clone();
            ports.push(Port {
                port: *port,
                pid,
                process,
                addr: addr.clone(),
            });
        }
    }
    ports.sort_by_key(|p| (p.port, p.pid));
    ports
}

/// Every listening TCP socket the kernel shows us, by inode.
fn tcp_listeners() -> HashMap<u64, (u16, String)> {
    let mut out = HashMap::new();
    for (file, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(table) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in table.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // sl, local_address, rem_address, st, …, inode is the tenth.
            if cols.len() < 10 || cols[3] != "0A" {
                continue;
            }
            let Some((addr, port)) = cols[1].split_once(':') else {
                continue;
            };
            let (Ok(port), Ok(inode)) = (u16::from_str_radix(port, 16), cols[9].parse::<u64>()) else {
                continue;
            };
            out.insert(inode, (port, parse_addr(addr, v6)));
        }
    }
    out
}

/// /proc/net/tcp's address column: the bytes in host order, in hex, one
/// 32-bit word at a time.
fn parse_addr(hex: &str, v6: bool) -> String {
    let words: Vec<u32> = (0..hex.len() / 8)
        .filter_map(|i| u32::from_str_radix(&hex[i * 8..i * 8 + 8], 16).ok())
        .collect();
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let addr = if v6 {
        let Ok(raw) = <[u8; 16]>::try_from(bytes.as_slice()) else {
            return "*".into();
        };
        std::net::IpAddr::from(std::net::Ipv6Addr::from(raw))
    } else {
        let Ok(raw) = <[u8; 4]>::try_from(bytes.as_slice()) else {
            return "*".into();
        };
        std::net::IpAddr::from(raw)
    };
    if addr.is_unspecified() {
        "*".into()
    } else {
        addr.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        assert_eq!(parse_addr("0100007F", false), "127.0.0.1");
        assert_eq!(parse_addr("00000000", false), "*");
        assert_eq!(parse_addr("00000000000000000000000001000000", true), "::1");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn our_own_process_reads() {
        let me = std::process::id();
        assert!(argv(me).first().is_some_and(|a| a.contains("deck")), "{:?}", argv(me));
        assert!(parent_of(me).is_some());
        assert!(task_usage(me).is_some_and(|(_, rss)| rss > 0));
        assert_eq!(cwd(me), std::env::current_dir().ok());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = listening_ports(&[me]);
        assert!(found.iter().any(|p| p.port == port && p.addr == "127.0.0.1"), "{found:?}");
    }
}
