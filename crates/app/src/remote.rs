//! Remote hosts: machines reached over SSH where terminals and coding
//! agents run, shown and driven from this Deck.
//!
//! The hosts are the aliases in the user's own SSH configuration (read,
//! never written) and the ones added in Deck (`deck remote add`, kept in
//! `remote-hosts.json`). A host added with its own name or a key (`deck
//! remote add user@1.2.3.4 --name box --key ~/box.pem`) gets a block in
//! Deck's own SSH config, which includes the user's; every `ssh` Deck runs
//! is given that file with `-F` once it exists, so `ssh box` works the same
//! from a terminal tab, an agent's chat and the MCP tools.
//!
//! - A terminal: a tab running `ssh -t HOST`, in a folder there.
//! - An agent in its own interface: the same, running the agent.
//! - A Deck chat: the agent process started on the host through `ssh -T`
//!   (see `agent_core::adapter::launch`), its conversation drawn here.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// What `ssh` is given: an alias or `user@host[:port]` as typed.
    pub name: String,
    /// `user@hostname` when the config says, for showing.
    #[serde(default)]
    pub detail: String,
    /// The folder to start in there; `~` when unset.
    #[serde(default)]
    pub dir: Option<String>,
    /// Added in Deck (so it can be removed there), not read from SSH's config.
    #[serde(default)]
    pub added: bool,
    /// For a host added in Deck under its own name: where it really is,
    /// `user@host[:port]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// For a host added in Deck: the private key to sign in with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn own_file() -> PathBuf {
    crate::control::socket_dir().join("remote-hosts.json")
}

fn ssh_config() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh").join("config"))
}

/// Every host: SSH's aliases first (in the order the file lists them), then
/// the ones added in Deck.
pub fn hosts() -> Vec<Host> {
    let mut out = ssh_config().map(|p| parse_config(&p, 0)).unwrap_or_default();
    for h in added() {
        if !out.iter().any(|o| o.name == h.name) {
            out.push(h);
        }
    }
    out
}

fn added() -> Vec<Host> {
    std::fs::read(own_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_added(all: &[Host]) -> std::io::Result<()> {
    let path = own_file();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(all)?)?;
    write_config(all)
}

/// Deck's SSH config (see `agent_core::adapter::ssh_config`): the user's
/// and the system's config first, then a block for each host added here
/// with an address or a key. Removed when no host needs it, so plain `ssh`
/// behaviour is untouched until one does.
fn write_config(all: &[Host]) -> std::io::Result<()> {
    let path = agent_core::adapter::ssh_config();
    let blocks: Vec<String> = all.iter().filter_map(config_block).collect();
    if blocks.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = format!(
        "# Written by Deck from remote-hosts.json; edits here are replaced.\nInclude ~/.ssh/config\nInclude /etc/ssh/ssh_config\n\n{}",
        blocks.join("\n")
    );
    let mut f = crate::os::create_private(&path)?;
    std::io::Write::write_all(&mut f, text.as_bytes())
}

/// `user@host:port` → (user, host, port).
fn split_address(a: &str) -> (Option<&str>, &str, Option<&str>) {
    let (user, rest) = match a.split_once('@') {
        Some((u, r)) => (Some(u), r),
        None => (None, a),
    };
    match rest.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (user, h, Some(p)),
        _ => (user, rest, None),
    }
}

fn config_block(h: &Host) -> Option<String> {
    if h.address.is_none() && h.key.is_none() {
        return None;
    }
    let mut b = format!("Host {}\n", h.name);
    if let Some(a) = &h.address {
        let (user, host, port) = split_address(a);
        b += &format!("  HostName {host}\n");
        if let Some(u) = user {
            b += &format!("  User {u}\n");
        }
        if let Some(p) = port {
            b += &format!("  Port {p}\n");
        }
    }
    if let Some(k) = &h.key {
        b += &format!("  IdentityFile \"{k}\"\n  IdentitiesOnly yes\n");
    }
    b += "  StrictHostKeyChecking accept-new\n";
    Some(b)
}

/// Add a host: `spec` is an alias or `user@host[:port]`. With `name`, or a
/// `key` (which needs a name to hang on), the host is kept under that name
/// and reached at `spec` with the key.
pub fn add(spec: &str, name: Option<&str>, key: Option<&str>, dir: Option<String>) -> std::io::Result<Host> {
    let bad = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
    let key = match key {
        Some(k) => {
            let p = match k.strip_prefix("~/") {
                Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|| k.into()),
                None => std::path::absolute(k)?,
            };
            if !p.is_file() {
                return Err(bad(format!("no key at {}", p.display())));
            }
            Some(p)
        }
        None => None,
    };
    let (name, address) = match name {
        Some(n) if n != spec => (n.to_string(), Some(spec.to_string())),
        _ if key.is_some() => (split_address(spec).1.to_string(), Some(spec.to_string())),
        _ => (spec.to_string(), None),
    };
    if name.is_empty() || name.contains(|c: char| c.is_whitespace() || "*?!@\"/".contains(c)) {
        return Err(bad(format!("`{name}` can't be a host name; pick one with --name")));
    }
    // A copy in Deck's own folder, readable only by you. The original is
    // often in Downloads or Desktop, which macOS keeps from processes not
    // granted them, and the background service that runs Deck's terminals
    // is one: `ssh` there failed with "Load key: Operation not permitted".
    let key = match key {
        Some(src) => {
            let dir = crate::control::socket_dir().join("keys");
            std::fs::create_dir_all(&dir)?;
            crate::os::private(&dir)?;
            let dest = dir.join(format!("{name}.pem"));
            let bytes = std::fs::read(&src).map_err(|e| bad(format!("could not read {}: {e}", src.display())))?;
            let tmp = dir.join(format!(".{name}.pem.new"));
            let mut f = crate::os::create_private(&tmp)?;
            std::io::Write::write_all(&mut f, &bytes)?;
            std::fs::rename(&tmp, &dest)?;
            Some(dest.to_string_lossy().into_owned())
        }
        None => None,
    };
    let mut all = added();
    all.retain(|h| h.name != name);
    let detail = address.clone().unwrap_or_default();
    let h = Host { name, detail, dir, added: true, address, key };
    all.push(h.clone());
    save_added(&all)?;
    Ok(h)
}

/// The `-F` flags every `ssh` Deck runs takes, so Deck's hosts resolve.
pub fn ssh_flags() -> Vec<String> {
    agent_core::adapter::ssh_config_flags()
}

pub fn remove(name: &str) -> std::io::Result<bool> {
    let mut all = added();
    let before = all.len();
    let keys = crate::control::socket_dir().join("keys");
    for h in all.iter().filter(|h| h.name == name) {
        // Only the copy Deck made; a key elsewhere is the user's.
        if let Some(k) = h.key.as_deref().map(Path::new).filter(|k| k.starts_with(&keys)) {
            let _ = std::fs::remove_file(k);
        }
    }
    all.retain(|h| h.name != name);
    if all.len() != before {
        save_added(&all)?;
    }
    Ok(all.len() != before)
}

/// `Host` blocks with a concrete alias (no `*` or `?`), their `HostName`
/// and `User`; `Include`d files are read too, a few levels deep.
fn parse_config(path: &Path, depth: u8) -> Vec<Host> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Host> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let flush = |out: &mut Vec<Host>, current: &[usize], fields: &[(String, String)]| {
        let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.clone());
        let detail = match (get("user"), get("hostname")) {
            (Some(u), Some(h)) => format!("{u}@{h}"),
            (None, Some(h)) => h,
            (Some(u), None) => format!("{u}@"),
            (None, None) => String::new(),
        };
        for &i in current {
            out[i].detail = detail.clone();
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_lowercase(), v.trim().trim_start_matches('=').trim().to_string()),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                flush(&mut out, &current, &fields);
                current.clear();
                fields.clear();
                for alias in value.split_whitespace() {
                    if alias.contains(['*', '?', '!']) || out.iter().any(|h| h.name == alias) {
                        continue;
                    }
                    out.push(Host { name: alias.to_string(), detail: String::new(), dir: None, added: false, address: None, key: None });
                    current.push(out.len() - 1);
                }
            }
            "match" => {
                flush(&mut out, &current, &fields);
                current.clear();
                fields.clear();
            }
            "include" if depth < 3 => {
                let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
                for pat in value.split_whitespace() {
                    let p = if let Some(rest) = pat.strip_prefix("~/") {
                        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_default()
                    } else if Path::new(pat).is_absolute() {
                        PathBuf::from(pat)
                    } else {
                        base.join(pat)
                    };
                    // A trailing `*` in a directory: every file in it.
                    let files: Vec<PathBuf> = match p.file_name().and_then(|f| f.to_str()) {
                        Some("*") => p
                            .parent()
                            .and_then(|d| std::fs::read_dir(d).ok())
                            .into_iter()
                            .flatten()
                            .flatten()
                            .map(|e| e.path())
                            .collect(),
                        _ => vec![p],
                    };
                    for f in files {
                        for h in parse_config(&f, depth + 1) {
                            if !out.iter().any(|o| o.name == h.name) {
                                out.push(h);
                            }
                        }
                    }
                }
            }
            k => fields.push((k.to_string(), value)),
        }
    }
    flush(&mut out, &current, &fields);
    out
}

/// A word for a remote shell, quoted.
pub fn q(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A folder as the remote shell should see it: `~`-paths left for it to
/// expand.
fn remote_dir(dir: &str) -> String {
    match dir.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) => format!("\"$HOME\"{}", q(rest)),
        None => q(dir),
    }
}

/// What the remote end runs: `cd` to `dir`, then `program` (if any), then
/// a login shell.
pub fn remote_line(dir: &str, program: Option<&str>) -> String {
    let cd = format!("cd {} 2>/dev/null || cd", remote_dir(dir));
    match program {
        Some(p) => format!("{cd}; {p}; exec \"$SHELL\" -l"),
        None => format!("{cd}; exec \"$SHELL\" -l"),
    }
}

/// The command a terminal tab runs for `host`: a login shell in `dir`, or
/// `program` (an agent's own interface) there, then a shell.
pub fn terminal_command(host: &str, dir: &str, program: Option<&str>) -> String {
    let cd = format!("cd {} 2>/dev/null || cd", remote_dir(dir));
    let inner = match program {
        Some(p) => format!("{cd}; {p}; exec \"$SHELL\" -l"),
        None => format!("{cd}; exec \"$SHELL\" -l"),
    };
    // Something on screen at once, and SSH's own error after ten seconds,
    // not a blank pane until the network gives up (over a minute).
    let flags: String = ssh_flags().iter().map(|f| format!("{} ", q(f))).collect();
    format!(
        "printf '\\033[2mConnecting to %s…\\033[0m\\r\\n' {h}; ssh -t -o ConnectTimeout=10 -o ServerAliveInterval=30 {flags}{h} {}",
        q(&inner),
        h = q(host)
    )
}

/// Deck's MCP tools for hosts: list them, run a command, read and write a
/// file, all over `ssh -T` with no prompts (BatchMode), so an agent here can
/// work on a machine there.
pub fn tool(rpc: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let n = |k: &str| args.get(k).and_then(|v| v.as_u64());
    match rpc {
        "remote.hosts" => Ok(json!({ "hosts": hosts() })),
        "remote.run" => {
            let host = s("host").ok_or("host is required")?;
            let command = s("command").ok_or("command is required")?;
            let dir = s("dir").unwrap_or_else(|| "~".into());
            let line = format!("cd {} && {}", remote_dir(&dir), command);
            let timeout = n("timeout").unwrap_or(120).clamp(1, 1800);
            let (code, out, err) = ssh(&host, &line, None, timeout)?;
            Ok(json!({ "exit": code, "stdout": clip(&out), "stderr": clip(&err) }))
        }
        "remote.read" => {
            let host = s("host").ok_or("host is required")?;
            let path = s("path").ok_or("path is required")?;
            let max = n("max_bytes").unwrap_or(256 * 1024).min(4 * 1024 * 1024);
            let line = format!("head -c {max} -- {}", remote_dir(&path));
            let (code, out, err) = ssh(&host, &line, None, 60)?;
            if code != Some(0) {
                return Err(format!("could not read {path}: {}", err.trim()));
            }
            Ok(json!({ "path": path, "content": out }))
        }
        "remote.write" => {
            let host = s("host").ok_or("host is required")?;
            let path = s("path").ok_or("path is required")?;
            let content = s("content").unwrap_or_default();
            let p = remote_dir(&path);
            let line = format!("mkdir -p -- \"$(dirname -- {p})\" && cat > {p}");
            let (code, _, err) = ssh(&host, &line, Some(content.as_bytes()), 60)?;
            if code != Some(0) {
                return Err(format!("could not write {path}: {}", err.trim()));
            }
            Ok(json!({ "path": path, "bytes": content.len() }))
        }
        other => Err(format!("no tool {other}")),
    }
}

/// Run `line` on `host` in its login shell, feeding `input`, within
/// `timeout` seconds: (exit code, stdout, stderr).
pub fn run_on(host: &str, line: &str, input: Option<&[u8]>, timeout: u64) -> Result<(Option<i32>, String, String), String> {
    ssh(host, line, input, timeout)
}

/// Where the shared connections' sockets live: short (a socket path has
/// ~104 bytes), private, and cleared on restart.
fn control_dir() -> PathBuf {
    #[cfg(unix)]
    // SAFETY: getuid reads this process's user id.
    let dir = PathBuf::from(format!("/tmp/deck-{}", unsafe { libc::getuid() }));
    #[cfg(not(unix))]
    let dir = std::env::temp_dir().join("deck-ssh");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = crate::os::private(&dir);
    }
    dir
}

fn ssh(host: &str, line: &str, input: Option<&[u8]>, timeout: u64) -> Result<(Option<i32>, String, String), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    // One connection per host, kept for ten minutes and shared by every
    // call: an agent's tool call then costs a round trip, not a handshake
    // (about 90 ms against 900 to an EC2 box). %C is ssh's hash of the
    // destination, so the path stays short.
    let control = format!("ControlPath={}/%C", control_dir().display());
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=20"])
        .args(["-o", "ControlMaster=auto", "-o", &control, "-o", "ControlPersist=10m", "-o", "LogLevel=ERROR"])
        .args(ssh_flags())
        .args([host, "--"])
        .arg(format!("exec \"$SHELL\" -lc {}", q(line)))
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run ssh: {e}"))?;
    // Drained as it comes: a full pipe would stall the command until the
    // timeout.
    let drain = |r: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|s| Box::new(s) as _));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as _));
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(data);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {timeout} s on {host}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Ok((status.code(), out.join().unwrap_or_default(), err.join().unwrap_or_default()))
}

/// Output for an agent: its last 20,000 characters when longer.
fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= 20_000 {
        s.to_string()
    } else {
        format!("[… {} characters cut]\n{}", n - 20_000, s.chars().skip(n - 20_000).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_hosts_read_with_their_details() {
        let dir = std::env::temp_dir().join(format!("deck-remote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(
            &file,
            "Host *\n  ServerAliveInterval 30\n\nHost box devbox\n  HostName 10.0.0.5\n  User nees\n\nHost gpu\n  HostName gpu.example.com\n",
        )
        .unwrap();
        let hosts = parse_config(&file, 0);
        let names: Vec<_> = hosts.iter().map(|h| (h.name.as_str(), h.detail.as_str())).collect();
        assert_eq!(names, vec![("box", "nees@10.0.0.5"), ("devbox", "nees@10.0.0.5"), ("gpu", "gpu.example.com")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn added_hosts_become_config_blocks() {
        assert_eq!(split_address("me@1.2.3.4:2222"), (Some("me"), "1.2.3.4", Some("2222")));
        assert_eq!(split_address("box"), (None, "box", None));
        let h = Host {
            name: "box".into(),
            detail: String::new(),
            dir: None,
            added: true,
            address: Some("ec2-user@1.2.3.4".into()),
            key: Some("/k/box.pem".into()),
        };
        let b = config_block(&h).unwrap();
        assert!(b.starts_with("Host box\n  HostName 1.2.3.4\n  User ec2-user\n"), "{b}");
        assert!(b.contains("IdentityFile \"/k/box.pem\"\n  IdentitiesOnly yes"), "{b}");
        let plain = Host { address: None, key: None, ..h };
        assert!(config_block(&plain).is_none());
    }

    #[test]
    fn terminal_commands_quote_and_expand_home() {
        let c = terminal_command("box", "~", None);
        assert!(c.contains("Connecting to"), "{c}");
        assert!(c.contains("ssh -t -o ConnectTimeout=10 -o ServerAliveInterval=30 "), "{c}");
        assert!(c.ends_with(" box 'cd \"$HOME\" 2>/dev/null || cd; exec \"$SHELL\" -l'"), "{c}");
        let c = terminal_command("me@host", "~/src/app", Some("claude"));
        assert!(c.contains(" me@host '"), "{c}");
        assert!(c.contains("claude; exec"), "{c}");
    }
}
