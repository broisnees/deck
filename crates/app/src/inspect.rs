//! What a pane is doing, read from the OS: its process tree, working
//! directory, listening ports, and the git state of the project it sits in.
//!
//! Pure data, no gpui. The inspector refreshes on a timer while the user
//! works, so processes and ports are read straight from the kernel through
//! libproc (microseconds) rather than by forking `ps` and `lsof` (tens of
//! milliseconds each). Only git still runs a child process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// Percent of one core, as `ps %cpu` reports it.
    pub cpu: f32,
    /// Resident set size in bytes.
    pub rss: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub port: u16,
    pub pid: u32,
    pub process: String,
    /// `127.0.0.1`, `::1` (brackets stripped) or `*` for every interface.
    pub addr: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitFile {
    pub path: String,
    /// `M`, `A`, `D`, `R`, `C`, `U` or `??`.
    pub status: String,
    /// Has changes in the index (staged), and in the working tree.
    pub staged: bool,
    pub unstaged: bool,
}

/// A checkout of the repository: the main one or one added with
/// `git worktree add`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub current: bool,
}

/// A commit in the recent history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    /// Unix seconds.
    pub date: i64,
    pub refs: Vec<String>,
    /// More than one parent.
    pub merge: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GitInfo {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFile>,
    pub worktrees: Vec<Worktree>,
    pub commits: Vec<Commit>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

const MAX_GIT_FILES: usize = 500;
const MAX_ENTRIES: usize = 1000;

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// `root` and every descendant, parent first (depth-first, root at index 0).
/// Empty when `root` is not running.
///
/// `cpu` is the share of one core used since the previous call that saw the
/// process (0 on the first sighting), so it reads like Activity Monitor's
/// live figure rather than `ps`'s lifetime average. Processes owned by
/// another user, such as the setuid `login` that wraps each shell, report 0
/// for `cpu` and `rss`: the kernel only discloses those to root.
pub fn descendants(root: u32) -> Vec<Proc> {
    let Some(root_ppid) = parent_of(root) else {
        return Vec::new();
    };
    // Collect the subtree (one listpids call per process), then order it.
    let mut procs = Vec::new();
    let mut pending = vec![(root, root_ppid)];
    let mut seen = HashSet::new();
    while let Some((pid, ppid)) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let (cpu_ns, rss) = task_usage(pid).unwrap_or((None, 0));
        procs.push((
            Proc {
                pid,
                ppid,
                name: name_of(pid),
                cpu: 0.,
                rss,
            },
            cpu_ns,
        ));
        pending.extend(child_pids(pid).into_iter().map(|c| (c, pid)));
    }
    rate_cpu(&mut procs, Instant::now());
    tree_order(&procs.into_iter().map(|(p, _)| p).collect::<Vec<_>>(), root)
}

/// Turn cumulative CPU time into a percentage against the last sample.
fn rate_cpu(procs: &mut [(Proc, Option<u64>)], now: Instant) {
    static LAST: OnceLock<Mutex<HashMap<u32, (u64, Instant)>>> = OnceLock::new();
    let Ok(mut last) = LAST.get_or_init(Default::default).lock() else {
        return;
    };
    for (p, cpu_ns) in procs.iter_mut() {
        let Some(ns) = *cpu_ns else { continue };
        if let Some(&(prev_ns, at)) = last.get(&p.pid) {
            let wall = now.saturating_duration_since(at).as_nanos() as f64;
            // A reused pid starts below the old counter; that reads as 0.
            if wall > 0. && ns >= prev_ns {
                p.cpu = ((ns - prev_ns) as f64 / wall * 100.) as f32;
            }
        }
        last.insert(p.pid, (ns, now));
    }
    // Several panes share this table, so a pid one caller did not visit is
    // not necessarily dead; drop only what nobody has sampled for a while.
    last.retain(|_, &mut (_, at)| now.saturating_duration_since(at) < Duration::from_secs(60));
}

fn tree_order(procs: &[Proc], root: u32) -> Vec<Proc> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    if !by_pid.contains_key(&root) {
        return Vec::new();
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        // launchd (pid 0/1) is its own parent; an edge to itself would loop.
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p.pid);
        }
    }
    for kids in children.values_mut() {
        // Stack pops from the end, so reverse order yields ascending pids.
        kids.sort_unstable_by(|a, b| b.cmp(a));
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(p) = by_pid.get(&pid) {
            out.push((*p).clone());
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    out
}

/// `2.1.300`: a file named after a release rather than a program.
fn is_version(name: &str) -> bool {
    name.contains('.') && name.chars().all(|c| c.is_ascii_digit() || c == '.')
}

// The kernel half: libproc on macOS, /proc on Linux.
#[cfg(target_os = "macos")]
#[path = "inspect/darwin.rs"]
mod kernel;
#[cfg(not(target_os = "macos"))]
#[path = "inspect/procfs.rs"]
mod kernel;

pub use kernel::{argv, cwd, foreground, listening_ports, process_name};
use kernel::{child_pids, name_of, parent_of, task_usage};

// ---------------------------------------------------------------------------
// Project and git
// ---------------------------------------------------------------------------

/// The nearest ancestor of `dir` (itself included) holding `.git`; `dir`
/// when there is none.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Branch, ahead/behind and changed files. `None` outside a repository or
/// without git.
pub fn git(dir: &Path) -> Option<GitInfo> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain=v2", "--branch", "-z"])
        // A status poll must never take `index.lock` out from under a commit
        // the user is running at the same moment.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut info = parse_git(&String::from_utf8_lossy(&out.stdout));
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    info.worktrees = parse_worktrees(&git(&["worktree", "list", "--porcelain"]), dir);
    info.commits = parse_log(&git(&["log", "-n", "12", "--format=%H%x1f%h%x1f%P%x1f%s%x1f%an%x1f%at%x1f%D%x1e"]));
    Some(info)
}

fn parse_worktrees(out: &str, dir: &Path) -> Vec<Worktree> {
    let mut list: Vec<Worktree> = out
        .split("\n\n")
        .filter_map(|block| {
            let mut w = Worktree::default();
            for line in block.lines() {
                let (k, v) = line.split_once(' ').unwrap_or((line, ""));
                match k {
                    "worktree" => w.path = PathBuf::from(v),
                    "branch" => w.branch = Some(v.strip_prefix("refs/heads/").unwrap_or(v).to_string()),
                    _ => {}
                }
            }
            (!w.path.as_os_str().is_empty()).then_some(w)
        })
        .collect();
    // The current one is the deepest worktree this folder is inside (one
    // worktree can sit inside another's folder).
    let here = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let current = list
        .iter()
        .enumerate()
        .filter_map(|(i, w)| {
            let p = std::fs::canonicalize(&w.path).unwrap_or_else(|_| w.path.clone());
            here.starts_with(&p).then_some((i, p.components().count()))
        })
        .max_by_key(|&(_, depth)| depth)
        .map(|(i, _)| i);
    if let Some(i) = current {
        list[i].current = true;
    }
    list
}

fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .map(|r| r.trim_start_matches('\n'))
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let f: Vec<&str> = r.split('\x1f').collect();
            Some(Commit {
                hash: f.first()?.to_string(),
                short: f.get(1)?.to_string(),
                merge: f.get(2)?.split_whitespace().count() > 1,
                subject: f.get(3)?.to_string(),
                author: f.get(4)?.to_string(),
                date: f.get(5)?.parse().unwrap_or(0),
                refs: f
                    .get(6)
                    .map(|d| d.split(", ").filter(|x| !x.is_empty()).map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Porcelain v2 with `-z`: NUL-terminated records, so paths need no
/// unquoting; a rename record is followed by its original path as a record
/// of its own.
fn parse_git(out: &str) -> GitInfo {
    let mut info = GitInfo::default();
    let mut oid = String::new();
    let mut records = out.split('\0').filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        if let Some(header) = rec.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" => oid = value.to_string(),
                "branch.head" => info.branch = value.to_string(),
                "branch.upstream" => info.upstream = Some(value.to_string()),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') {
                            info.ahead = n.parse().unwrap_or(0);
                        } else if let Some(n) = part.strip_prefix('-') {
                            info.behind = n.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let xy = rec.get(2..4).unwrap_or("..");
        let (staged, unstaged) = match rec.as_bytes().first() {
            Some(b'1' | b'2') => (xy.as_bytes()[0] != b'.', xy.as_bytes().get(1) != Some(&b'.')),
            Some(b'u') => (false, true),
            _ => (false, true),
        };
        let file = match rec.as_bytes().first() {
            Some(b'1') => rec
                .splitn(9, ' ')
                .nth(8)
                .map(|path| (path, xy_code(rec.get(2..4).unwrap_or("")))),
            Some(b'2') => {
                // The original path is the next record; consume it.
                records.next();
                rec.splitn(10, ' ')
                    .nth(9)
                    .map(|path| (path, "R".to_string()))
            }
            Some(b'u') => rec
                .splitn(11, ' ')
                .nth(10)
                .map(|path| (path, "U".to_string())),
            Some(b'?') => rec.get(2..).map(|path| (path, "??".to_string())),
            _ => None,
        };
        if let Some((path, status)) = file {
            if info.files.len() < MAX_GIT_FILES {
                info.files.push(GitFile {
                    path: path.to_string(),
                    status,
                    staged,
                    unstaged,
                });
            }
        }
    }
    if info.branch == "(detached)" && oid.len() >= 7 {
        info.branch = format!("detached@{}", &oid[..7]);
    }
    info
}

/// One letter for an `XY` pair (`.` = unchanged): the staged side when it has
/// a change, else the worktree side. A type change reads as a modification.
fn xy_code(xy: &str) -> String {
    let c = xy.chars().find(|&c| c != '.').unwrap_or('M');
    match c {
        'T' => "M".to_string(),
        c => c.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Directories first, then files, each alphabetical ignoring case.
pub fn list_dir(dir: &Path) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" || name == ".DS_Store" {
                return None;
            }
            let path = e.path();
            // `file_type` comes free with the directory read; only symlinks
            // cost a stat, to learn what they point at.
            let is_dir = match e.file_type() {
                Ok(t) if t.is_symlink() => path.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            };
            Some(Entry { name, path, is_dir })
        })
        .collect();
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    entries.truncate(MAX_ENTRIES);
    entries
}

/// `812 KB`, `3.4 MB`, `1.2 GB` — binary units, as Activity Monitor uses.
pub fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.;
    let f = n as f64;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / (K * K))
    } else {
        format!("{:.1} GB", f / (K * K * K))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32) -> Proc {
        Proc {
            pid,
            ppid,
            name: format!("p{pid}"),
            cpu: 0.,
            rss: 0,
        }
    }

    #[test]
    fn tree_orders_parent_first_with_ascending_siblings() {
        let procs = [
            p(1, 0),
            p(100, 1),
            p(300, 100),
            p(200, 100),
            p(201, 200),
            p(202, 201),
            p(999, 1),
        ];
        let order: Vec<u32> = tree_order(&procs, 100).iter().map(|p| p.pid).collect();
        assert_eq!(order, vec![100, 200, 201, 202, 300]);
        assert!(tree_order(&procs, 4242).is_empty());
        // launchd parents itself through pid 0 without looping.
        assert_eq!(tree_order(&[p(1, 1), p(5, 1)], 1).len(), 2);
    }

    #[test]
    fn cpu_is_a_rate_between_samples() {
        let t0 = Instant::now();
        let mut a = [(p(4_000_001, 1), Some(1_000_000_000u64))];
        rate_cpu(&mut a, t0);
        assert_eq!(a[0].0.cpu, 0.);
        // 0.5s of CPU over 1s of wall time is half a core.
        let mut b = [(p(4_000_001, 1), Some(1_500_000_000u64))];
        rate_cpu(&mut b, t0 + Duration::from_secs(1));
        assert!((b[0].0.cpu - 50.).abs() < 0.01, "{}", b[0].0.cpu);
        // A counter that went backwards (pid reuse) reads as idle, not huge.
        let mut c = [(p(4_000_001, 1), Some(10u64))];
        rate_cpu(&mut c, t0 + Duration::from_secs(2));
        assert_eq!(c[0].0.cpu, 0.);
    }

    #[test]
    fn live_busy_child_shows_cpu_and_rss() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "while :; do :; done"])
            .spawn()
            .unwrap();
        let me = std::process::id();
        let first = descendants(me);
        assert!(
            first.iter().any(|p| p.pid == child.id()),
            "child missing: {first:?}"
        );
        assert!(first[0].rss > 0);
        std::thread::sleep(Duration::from_millis(300));
        let second = descendants(me);
        let busy = second.iter().find(|p| p.pid == child.id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            busy.cpu > 50.,
            "a spinning shell should use most of a core: {busy:?}"
        );
        assert_eq!(busy.ppid, me);
        // `/bin/sh` is bash on macOS; what matters is that a name came back.
        assert!(!busy.name.is_empty());
    }

    #[test]
    fn live_listeners_are_found_on_v4_and_v6() {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let v6 = std::net::TcpListener::bind("[::1]:0").unwrap();
        let any = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let ports = listening_ports(&[std::process::id()]);
        let find = |port: u16| ports.iter().find(|p| p.port == port).cloned();
        let a = find(v4.local_addr().unwrap().port()).expect("v4 listener");
        assert_eq!((a.addr.as_str(), a.pid), ("127.0.0.1", std::process::id()));
        assert_eq!(
            find(v6.local_addr().unwrap().port())
                .expect("v6 listener")
                .addr,
            "::1"
        );
        assert_eq!(
            find(any.local_addr().unwrap().port())
                .expect("any listener")
                .addr,
            "*"
        );
        assert!(!a.process.is_empty());
        // A connected (non-listening) socket must not show up.
        let _conn = std::net::TcpStream::connect(v4.local_addr().unwrap()).unwrap();
        assert_eq!(listening_ports(&[std::process::id()]).len(), 3);
        assert!(listening_ports(&[]).is_empty());
    }

    #[test]
    fn git_porcelain_v2_parses_branch_counts_and_files() {
        let out = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaaa bbbb src/main.rs",
            "1 A. N... 000000 100644 100644 0000 cccc new file.rs",
            "1 D. N... 100644 000000 000000 dddd 0000 gone.rs",
            "2 R. N... 100644 100644 100644 eeee ffff R100 renamed.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 1111 2222 3333 conflict.rs",
            "? notes.txt",
            "",
        ]
        .join("\0");
        let g = parse_git(&out);
        assert_eq!(g.branch, "main");
        assert_eq!((g.ahead, g.behind), (2, 1));
        let files: Vec<(&str, &str)> = g
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status.as_str()))
            .collect();
        assert_eq!(
            files,
            vec![
                ("src/main.rs", "M"),
                ("new file.rs", "A"),
                ("gone.rs", "D"),
                ("renamed.rs", "R"),
                ("conflict.rs", "U"),
                ("notes.txt", "??"),
            ]
        );

        let detached =
            "# branch.oid 89abcdef0123456789abcdef0123456789abcdef\0# branch.head (detached)\0";
        assert_eq!(parse_git(detached).branch, "detached@89abcde");
    }

    #[test]
    fn bytes_read_like_activity_monitor() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(812 * 1024), "812 KB");
        assert_eq!(human_bytes(3_565_158), "3.4 MB");
        assert_eq!(human_bytes(548_510_105), "523.1 MB");
        assert_eq!(human_bytes(1_288_490_189), "1.2 GB");
    }

    #[test]
    fn live_process_tree_and_cwd_of_this_test() {
        let me = std::process::id();
        let tree = descendants(me);
        assert_eq!(tree.first().map(|p| p.pid), Some(me));
        assert!(tree.iter().all(|p| p.pid == me || p.ppid != 0));
        assert_eq!(cwd(me), std::env::current_dir().ok());
        assert_eq!(cwd(u32::MAX >> 1), None);
    }

    #[test]
    fn project_root_and_listing() {
        let base = std::env::temp_dir().join(format!("deck-inspect-{}", std::process::id()));
        let nested = base.join("repo/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        for f in ["b.txt", "A.txt", ".env", ".DS_Store"] {
            std::fs::write(base.join("repo").join(f), "").unwrap();
        }
        std::fs::create_dir_all(base.join("repo/Zdir")).unwrap();

        assert_eq!(project_root(&nested), base.join("repo"));
        assert_eq!(project_root(&base), base);

        let names: Vec<String> = list_dir(&base.join("repo"))
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["src", "Zdir", ".env", "A.txt", "b.txt"]);
        assert!(list_dir(&base.join("missing")).is_empty());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn procargs_parse_and_our_own_argv_reads() {
        use super::kernel::{parse_procargs, ProcBsdInfo};
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/opt/claude/versions/2.1.300\0\0\0claude\0--resume\0HOME=/x\0");
        assert_eq!(parse_procargs(&buf), vec!["claude", "--resume"]);
        assert!(parse_procargs(b"\x01").is_empty());
        let me = argv(std::process::id());
        assert!(!me.is_empty() && me[0].contains("deck"), "{me:?}");
        assert!(is_version("2.1.300") && !is_version("zsh") && !is_version("42"));
        assert_eq!(std::mem::size_of::<ProcBsdInfo>(), 136);
        // Our own row reads (the test may have no terminal, so no tpgid).
        let _ = foreground(std::process::id());
    }
}
