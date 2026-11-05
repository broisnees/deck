//! `deckd`: background jobs and terminal agents that keep running when Deck
//! quits, and come back after the Mac restarts.
//!
//! The same binary run as `deck daemon`, kept alive by a LaunchAgent. Each
//! job is a command in a PTY of the daemon's own, with its output in a ring
//! in memory and a log on disk. Deck shows a job by running `deck job attach
//! NAME` in an ordinary terminal pane: the client replays what the job
//! printed, then passes keys, output and window size through, the way
//! tmux's client does. Quitting Deck ends the client, never the job.
//!
//! The daemon puts an icon in the menu bar: every job with its state, and
//! Show in Deck, Restart and Stop for each.
//!
//! Requests are one JSON line on `deckd.sock` (0600 in deck's 0700 data
//! folder), answered with one JSON line; `attach` then turns the connection
//! into the job's stream. Frames from an attached client: `0, len (u32 BE),
//! bytes` for input, `1, rows (u16 BE), cols (u16 BE)` for a resize.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
const LABEL: &str = "com.broisnischal.deckd";
/// What a newly attached client is sent of the job's past output.
const RING: usize = 512 * 1024;
/// A log this big is cut to its last half.
const LOG_MAX: u64 = 8 * 1024 * 1024;

pub fn socket() -> PathBuf {
    crate::control::socket_dir().join("deckd.sock")
}

fn state_file() -> PathBuf {
    crate::control::socket_dir().join("deckd-jobs.json")
}

fn log_dir() -> PathBuf {
    crate::control::socket_dir().join("jobs")
}

fn log_file(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    log_dir().join(format!("{safe}.log"))
}

/// A job as the daemon reports it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobInfo {
    pub name: String,
    pub cmd: String,
    pub cwd: PathBuf,
    pub running: bool,
    pub exit: Option<i32>,
    pub pid: i32,
    /// Unix seconds.
    pub started: i64,
    /// The agent CLI it runs (`claude`, `codex`), when it is one.
    pub agent: Option<String>,
    /// A terminal tab's session (Deck shows it as a tab, not as a job).
    #[serde(default)]
    pub tab: bool,
}

/// What the daemon keeps to start a job again after a restart.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    name: String,
    cmd: String,
    cwd: PathBuf,
    env: HashMap<String, String>,
    running: bool,
    #[serde(default)]
    tab: bool,
}

// ---------------------------------------------------------------------------
// Client side

/// One request to the daemon, starting it first when it is not running.
pub fn call(req: Value) -> Result<Value, String> {
    match ask(&req) {
        Ok(v) => Ok(v),
        Err(_) => {
            ensure_running()?;
            let start = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(150));
                match ask(&req) {
                    Ok(v) => return Ok(v),
                    Err(e) if start.elapsed() > Duration::from_secs(6) => {
                        return Err(format!("the background service did not start: {e}"))
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

/// One request, without starting the daemon.
pub fn ask(req: &Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(socket()).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut answer = String::new();
    BufReader::new(s).read_line(&mut answer).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&answer).map_err(|e| format!("bad answer from deckd: {e}"))?;
    match v.get("error").and_then(Value::as_str) {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

/// The jobs, if the daemon is up; kept two seconds, for drawing.
pub fn cached_list() -> Vec<JobInfo> {
    static CACHE: Mutex<Option<(Instant, Vec<JobInfo>)>> = Mutex::new(None);
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, jobs)) = c.as_ref() {
        if at.elapsed() < Duration::from_secs(2) {
            return jobs.clone();
        }
    }
    let jobs = list().unwrap_or_default();
    *c = Some((Instant::now(), jobs.clone()));
    jobs
}

/// The jobs, without starting the daemon.
pub fn list() -> Result<Vec<JobInfo>, String> {
    let v = ask(&json!({ "op": "list" }))?;
    serde_json::from_value(v["jobs"].clone()).map_err(|e| e.to_string())
}

/// Start (or restart) a job. `env` is the environment it runs with,
/// usually the caller's own, so it finds the same programs.
pub fn start(name: &str, cmd: &str, cwd: &Path, env: HashMap<String, String>) -> Result<JobInfo, String> {
    start_kind(name, cmd, cwd, env, false)
}

/// A terminal tab's shell, run in the service so it outlives Deck.
pub fn start_tab(name: &str, cmd: &str, cwd: &Path, env: HashMap<String, String>) -> Result<JobInfo, String> {
    start_kind(name, cmd, cwd, env, true)
}

fn start_kind(name: &str, cmd: &str, cwd: &Path, env: HashMap<String, String>, tab: bool) -> Result<JobInfo, String> {
    let v = call(json!({ "op": "start", "name": name, "cmd": cmd, "cwd": cwd, "env": env, "tab": tab }))?;
    serde_json::from_value(v["job"].clone()).map_err(|e| e.to_string())
}

pub fn stop(name: &str) -> Result<(), String> {
    ask(&json!({ "op": "stop", "name": name })).map(|_| ())
}

pub fn restart(name: &str) -> Result<(), String> {
    ask(&json!({ "op": "restart", "name": name })).map(|_| ())
}

/// The environment worth giving a job: ours, without what ties a process
/// to one Deck pane.
pub fn job_env() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| !matches!(k.as_str(), "DECK_PANE" | "DECK_TOKEN" | "TERM_SESSION_ID" | "OLDPWD" | "PWD" | "SHLVL" | "_"))
        .filter(|(k, _)| !inherited_agent_marker(k))
        .collect()
}

/// What an agent CLI sets in the processes it starts so it knows a nested
/// session (Claude Code's `CLAUDECODE`, `CLAUDE_CODE_*` session markers).
/// A shell or agent started for you is not nested in anything: inherited,
/// these turn Claude Code's transcript saving off. Your own settings come
/// back from your shell's startup files.
pub fn inherited_agent_marker(k: &str) -> bool {
    k == "CLAUDECODE" || k.starts_with("CLAUDE_CODE_") || k == "CLAUDE_AGENT_SDK_VERSION" || k == "CODEX_SANDBOX" || k == "CODEX_MANAGED_BY_NPM"
}

/// Make sure the LaunchAgent is installed and the daemon running: through
/// launchd when it can be, else started directly, detached. Off macOS it
/// is always started directly; a systemd user unit is still to come.
pub fn ensure_running() -> Result<(), String> {
    let exe = service_exe()?;
    #[cfg(target_os = "macos")]
    let rewrote = install_agent(&exe).unwrap_or(false);
    if ask(&json!({ "op": "ping" })).is_ok() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if start_with_launchd(&exe, rewrote) {
        return Ok(());
    }
    // No launchd (Linux, a test, a sandbox), or it would not run it: run it
    // ourselves, in its own session.
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(&exe);
    c.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    c.spawn().map_err(|e| e.to_string())?;
    if until(Duration::from_secs(5), || ask(&json!({ "op": "ping" })).is_ok()) {
        Ok(())
    } else {
        Err(format!("the background service did not start; see {}", crate::control::socket_dir().join("deckd.log").display()))
    }
}

/// Start the service through launchd; whether it then answers.
#[cfg(target_os = "macos")]
fn start_with_launchd(exe: &Path, rewrote: bool) -> bool {
    // Not running, so reloading it costs nothing: no job or tab is lost.
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{LABEL}");
    let launchctl = |args: &[&str]| {
        std::process::Command::new("/bin/launchctl")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let printed = || {
        std::process::Command::new("/bin/launchctl")
            .args(["print", &target])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    // launchd keeps the definition it loaded, not the plist on disk: a job
    // loaded before the service moved out of the bundle (or before the
    // plist changed) would kickstart the old binary, which exits 0 and,
    // with KeepAlive only on failure, stays stopped. Reload it then.
    let loaded = printed();
    let stale = rewrote
        || loaded.as_deref().is_some_and(|p| {
            !p.lines().any(|l| l.trim().strip_prefix("program = ") == Some(&*exe.to_string_lossy()))
        });
    if loaded.is_some() && stale {
        launchctl(&["bootout", &target]);
        // bootout returns before launchd lets go of the label; bootstrapping
        // in that window fails with an I/O error.
        until(Duration::from_secs(5), || printed().is_none());
    }
    let started = if loaded.is_some() && !stale {
        launchctl(&["kickstart", &target])
    } else {
        launchctl(&["bootstrap", &domain, &agent_plist().to_string_lossy()]) || launchctl(&["kickstart", &target])
    };
    // launchctl succeeding means launchd started something, not that it
    // serves: wait for the socket to answer.
    started && until(Duration::from_secs(5), || ask(&json!({ "op": "ping" })).is_ok())
}

/// Poll `done` every 100 ms for up to `limit`; whether it came true.
fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The binary the service runs: a copy of this one outside the app bundle.
/// Run from inside Deck.app, macOS takes it for a running copy of Deck (it
/// shares the bundle id): `open Deck.app` then only woke the service
/// instead of opening a window, and replacing the app on an update could
/// quit it, and every job and terminal session with it. Copied again when
/// this binary changes, by rename, so a running service is never touched.
fn service_exe() -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if !me.to_string_lossy().contains(".app/Contents/MacOS/") {
        return Ok(me);
    }
    let dir = crate::control::socket_dir().join("bin");
    let dest = dir.join("deckd");
    let stamp = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    let src = stamp(&me);
    let same = std::fs::read_to_string(dir.join("deckd.from")).ok().as_deref()
        == Some(format!("{:?}", src).as_str())
        && dest.exists();
    if !same {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let tmp = dir.join("deckd.new");
        std::fs::copy(&me, &tmp).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        let _ = std::fs::write(dir.join("deckd.from"), format!("{:?}", src));
    }
    Ok(dest)
}

#[cfg(target_os = "macos")]
fn agent_plist() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

/// Write the LaunchAgent: started at login, started again if it crashes,
/// left stopped after a clean exit (Quit in its menu). Rewritten only when
/// it changes; launchd reads it at the next login or bootstrap.
#[cfg(target_os = "macos")]
fn install_agent(exe: &Path) -> std::io::Result<bool> {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let path = std::env::var("PATH").unwrap_or_default();
    let log = crate::control::socket_dir().join("deckd.log");
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
</dict>
</plist>
"#,
        esc(&exe.to_string_lossy()),
        esc(&log.to_string_lossy()),
        esc(&path),
    );
    let file = agent_plist();
    if std::fs::read_to_string(&file).ok().as_deref() == Some(body.as_str()) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, body)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The CLI: `deck job …`

const JOB_HELP: &str = "\
deck job: background jobs and agents that keep running when Deck quits.

  deck job start [--name NAME] [--cwd DIR] CMD...   start (or restart) CMD in the background
  deck job list                                     every job: name, command, running, exit code
  deck job logs  --name NAME [--lines N]            its output, plain text
  deck job wait  --name NAME --for TEXT[|TEXT] [--timeout SECS]
  deck job wait  --name NAME --exit [--timeout SECS]   until it ends; exits with its code
  deck job show  --name NAME                        its terminal in Deck (opens Deck)
  deck job attach NAME                              its terminal here (what Deck's pane runs)
  deck job restart --name NAME
  deck job stop  --name NAME

They run in deck's background service, shown in the menu bar, and start
again after the Mac restarts. Run any agent this way too:
  deck job start --name fixer claude
";

pub fn cli(args: &[String]) -> i32 {
    let out = |v: Value| {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        0
    };
    let fail = |e: String| {
        eprintln!("deck: {e}");
        1
    };
    let verb = args.first().map(String::as_str).unwrap_or("help");
    let rest = args.get(1..).unwrap_or(&[]);
    let flag = |k: &str| rest.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let name = || flag("--name").or_else(|| rest.iter().find(|a| !a.starts_with("--")).cloned());
    match verb {
        "start" => {
            let (name, cwd, cmd) = match parse_start(rest) {
                Ok(x) => x,
                Err(e) => return fail(e),
            };
            match start(&name, &cmd, &cwd, job_env()) {
                Ok(job) => out(json!({ "job": job })),
                Err(e) => fail(e),
            }
        }
        "list" => match call(json!({ "op": "list" })) {
            Ok(v) => out(v),
            Err(e) => fail(e),
        },
        "logs" => {
            let Some(name) = name() else { return fail("logs needs --name NAME".into()) };
            let lines: usize = flag("--lines").and_then(|l| l.parse().ok()).unwrap_or(200);
            match ask(&json!({ "op": "logs", "name": name, "lines": lines })) {
                Ok(v) => {
                    print!("{}", v["text"].as_str().unwrap_or(""));
                    0
                }
                Err(e) => fail(e),
            }
        }
        "wait" => {
            let Some(name) = name() else { return fail("wait needs --name NAME".into()) };
            let timeout: u64 = flag("--timeout").and_then(|t| t.parse().ok()).unwrap_or(120);
            wait(&name, flag("--for"), rest.iter().any(|a| a == "--exit"), Duration::from_secs(timeout))
        }
        "stop" | "restart" => {
            let Some(name) = name() else { return fail(format!("{verb} needs --name NAME")) };
            let r = if verb == "stop" { stop(&name) } else { restart(&name) };
            match r {
                Ok(()) => out(json!({ verb: name })),
                Err(e) => fail(e),
            }
        }
        "show" => {
            let Some(name) = name() else { return fail("show needs --name NAME".into()) };
            let url = format!("deck://job/{}", crate::browser::encode_query(&name));
            match std::process::Command::new("/usr/bin/open").arg(url).status() {
                Ok(s) if s.success() => out(json!({ "shown": name })),
                _ => fail("could not open Deck".into()),
            }
        }
        "attach" => match name() {
            Some(n) => attach(&n),
            None => fail("attach needs a job name".into()),
        },
        _ => {
            print!("{JOB_HELP}");
            if verb == "help" || verb == "--help" { 0 } else { 2 }
        }
    }
}

/// `[--name N] [--cwd DIR] CMD...`: everything after the flags is the
/// command. The name defaults to one guessed from the command.
pub fn parse_start(rest: &[String]) -> Result<(String, PathBuf, String), String> {
    let mut name = None;
    let mut cwd = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--name" if i + 1 < rest.len() => name = Some(rest[i + 1].clone()),
            "--cwd" if i + 1 < rest.len() => cwd = Some(PathBuf::from(&rest[i + 1])),
            "--bg" => {
                i += 1;
                continue;
            }
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 2;
    }
    // One word is a command line as typed (`deck job start "npm run dev"`);
    // several are argv, quoted back so `sh -c 'a; b'` stays one argument.
    let cmd = match &rest[i..] {
        [one] => one.clone(),
        words => words.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" "),
    };
    if cmd.trim().is_empty() {
        return Err("job start needs a command, e.g. `deck job start --name dev npm run dev`".into());
    }
    let cwd = cwd
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    Ok((name.unwrap_or_else(|| job_name(&cmd)), cwd, cmd))
}

fn shell_quote(w: &str) -> String {
    if !w.is_empty() && w.chars().all(|c| c.is_alphanumeric() || "-_./:=@%+,".contains(c)) {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// A job's name from its command: `npm run dev` → dev, `cargo watch -x
/// test` → cargo, `claude` → claude.
pub fn job_name(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().filter(|w| !w.contains('=') && !w.starts_with('-')).collect();
    let pick = match words.as_slice() {
        [pm, "run", script, ..] if matches!(*pm, "npm" | "pnpm" | "yarn" | "bun") => *script,
        [pm, script, ..] if matches!(*pm, "pnpm" | "yarn" | "bun") && !matches!(*script, "x" | "dlx") => *script,
        [first, ..] => first.rsplit('/').next().unwrap_or(first),
        [] => "job",
    };
    pick.to_string()
}

fn wait(name: &str, want: Option<String>, exit: bool, timeout: Duration) -> i32 {
    let start = Instant::now();
    let alts: Vec<String> = want.iter().flat_map(|w| w.split('|')).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    loop {
        let jobs = match list() {
            Ok(j) => j,
            Err(e) => {
                eprintln!("deck: {e}");
                return 1;
            }
        };
        let Some(job) = jobs.into_iter().find(|j| j.name == name) else {
            eprintln!("deck: no job {name:?}");
            return 1;
        };
        if exit && !job.running {
            println!("{}", json!({ "name": name, "exit": job.exit }));
            return job.exit.unwrap_or(0);
        }
        if !alts.is_empty() {
            if let Ok(v) = ask(&json!({ "op": "logs", "name": name, "lines": 400 })) {
                // Deck's own banners name the command; they don't count.
                let text: String = v["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .lines()
                    .filter(|l| !l.starts_with("[deck: "))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Some(hit) = alts.iter().find(|a| text.contains(a.as_str())) {
                    let line = text.lines().rev().find(|l| l.contains(hit.as_str())).unwrap_or("");
                    println!("{}", json!({ "name": name, "matched": hit, "line": line.trim() }));
                    return 0;
                }
            }
            if !job.running {
                eprintln!("deck: {name} exited ({:?}) before printing {:?}", job.exit, want.unwrap_or_default());
                return 1;
            }
        }
        if start.elapsed() > timeout {
            if let Ok(v) = ask(&json!({ "op": "logs", "name": name, "lines": 20 })) {
                eprintln!("{}", v["text"].as_str().unwrap_or(""));
            }
            eprintln!("deck: timed out waiting on {name}");
            return 1;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// `deck job attach NAME`: this terminal becomes the job's, until the
/// connection closes. Keys go to the job raw; its output comes back.
fn attach(name: &str) -> i32 {
    let (rows, cols) = winsize(0).unwrap_or((40, 120));
    let mut s = match UnixStream::connect(socket()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("deck: the background service is not running ({e})");
            return 1;
        }
    };
    let req = json!({ "op": "attach", "name": name, "rows": rows, "cols": cols });
    if s.write_all(format!("{req}\n").as_bytes()).is_err() {
        return 1;
    }
    // The answer line, read a byte at a time so no output is swallowed.
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while s.read(&mut b).ok() == Some(1) && b[0] != b'\n' {
        line.push(b[0]);
    }
    if let Ok(v) = serde_json::from_slice::<Value>(&line) {
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            eprintln!("deck: {e}");
            return 1;
        }
    }
    let saved = raw_mode(0);
    let mut input = match s.try_clone() {
        Ok(c) => c,
        Err(_) => return 1,
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut last = winsize(0);
        let mut stdin = std::io::stdin();
        loop {
            // Poll for keys a little at a time so a resize is noticed too.
            let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pfd, 1, 250) };
            let now = winsize(0);
            if now != last {
                last = now;
                if let Some((r, c)) = now {
                    let mut f = vec![1u8];
                    f.extend_from_slice(&r.to_be_bytes());
                    f.extend_from_slice(&c.to_be_bytes());
                    if input.write_all(&f).is_err() {
                        break;
                    }
                }
            }
            if ready <= 0 {
                continue;
            }
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut f = vec![0u8];
                    f.extend_from_slice(&(n as u32).to_be_bytes());
                    f.extend_from_slice(&buf[..n]);
                    if input.write_all(&f).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut out = std::io::stdout();
    let mut buf = [0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = out.flush();
            }
        }
    }
    if let Some(t) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    }
    0
}

fn winsize(fd: i32) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0;
    (ok && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

fn raw_mode(fd: i32) -> Option<libc::termios> {
    unsafe {
        if libc::isatty(fd) == 0 {
            return None;
        }
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return None;
        }
        let saved = t;
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(fd, libc::TCSANOW, &t);
        Some(saved)
    }
}

// ---------------------------------------------------------------------------
// The daemon

struct Job {
    info: JobInfo,
    env: HashMap<String, String>,
    master: i32,
    ring: Vec<u8>,
    clients: Vec<UnixStream>,
    size: (u16, u16),
}

type Jobs = Arc<Mutex<HashMap<String, Job>>>;

static JOBS: LazyLock<Jobs> = LazyLock::new(Default::default);
/// Jobs being stopped on purpose, so their exit is not told as a failure.
static STOPPING: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(Default::default);

/// `deck daemon`: serve jobs on the socket and show them in the menu bar.
/// Returns only when told to quit.
pub fn serve() -> i32 {
    // Started from inside Deck.app (`deck daemon` typed in a shell, whose
    // PATH finds the bundle's binary), macOS registers the service as a
    // second running Deck: an icon in ⌘-Tab, and the `deck://` links its
    // menu opens go to it instead of the window. Run as the copy outside the
    // bundle instead, as launchd does.
    if std::env::current_exe().is_ok_and(|me| me.to_string_lossy().contains(".app/Contents/MacOS/")) {
        if let Ok(exe) = service_exe() {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&exe).arg("daemon").exec();
            eprintln!("deckd: could not run {}: {err}", exe.display());
        }
    }
    let path = socket();
    if ask(&json!({ "op": "ping" })).is_ok() {
        eprintln!("deckd: already running");
        return 0;
    }
    let _ = std::fs::create_dir_all(crate::control::socket_dir());
    let _ = std::fs::create_dir_all(log_dir());
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("deckd: {e}");
            return 1;
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    // Jobs that were running when the daemon last stopped (a restart of the
    // Mac, a crash) start again.
    for s in load_saved().into_iter().filter(|s| s.running) {
        if let Err(e) = spawn_job(&s.name, &s.cmd, &s.cwd, s.env.clone(), s.tab) {
            eprintln!("deckd: could not restart {}: {e}", s.name);
        }
    }
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || handle(conn));
        }
    });
    crate::daemon_menu::run();
    0
}

fn load_saved() -> Vec<Saved> {
    std::fs::read(state_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_state() {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<Saved> = jobs
        .values()
        .map(|j| Saved {
            name: j.info.name.clone(),
            cmd: j.info.cmd.clone(),
            cwd: j.info.cwd.clone(),
            env: j.env.clone(),
            running: j.info.running,
            tab: j.info.tab,
        })
        .collect();
    drop(jobs);
    if let Ok(b) = serde_json::to_vec_pretty(&saved) {
        let tmp = state_file().with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(&tmp, state_file());
        }
    }
}

/// Every job, newest first, for the menu and `list`.
pub fn snapshot() -> Vec<JobInfo> {
    let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<JobInfo> = jobs.values().map(|j| j.info.clone()).collect();
    out.sort_by_key(|j| std::cmp::Reverse(j.started));
    out
}

fn handle(mut conn: UnixStream) {
    let mut reader = match conn.try_clone() {
        Ok(c) => BufReader::new(c),
        Err(_) => return,
    };
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let op = req["op"].as_str().unwrap_or("");
    let name = req["name"].as_str().unwrap_or("").to_string();
    let answer = match op {
        "ping" => Ok(json!({ "pid": std::process::id() })),
        // What macOS lets the service itself do: it, not Deck, runs the
        // terminals and agents it keeps.
        "privacy" => Ok(json!({ "full_disk": crate::privacy::full_disk() == crate::privacy::State::On })),
        "list" => Ok(json!({ "jobs": snapshot() })),
        "start" => {
            let cmd = req["cmd"].as_str().unwrap_or("").to_string();
            let cwd = req["cwd"].as_str().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
            let env: HashMap<String, String> = serde_json::from_value(req["env"].clone()).unwrap_or_default();
            let tab = req["tab"].as_bool().unwrap_or(false);
            let name = if name.is_empty() { job_name(&cmd) } else { name.clone() };
            if cmd.trim().is_empty() {
                Err("nothing to run".to_string())
            } else {
                kill_job(&name, false);
                spawn_job(&name, &cmd, &cwd, env, tab).map(|info| json!({ "job": info }))
            }
        }
        "stop" => {
            if kill_job(&name, true) {
                Ok(json!({ "stopped": name }))
            } else {
                Err(format!("no job {name:?}"))
            }
        }
        "restart" => restart_job(&name).map(|info| json!({ "job": info })),
        "logs" => {
            let lines = req["lines"].as_u64().unwrap_or(200) as usize;
            Ok(json!({ "text": tail_log(&name, lines) }))
        }
        "quit" => {
            let _ = conn.write_all(b"{\"ok\":true}\n");
            quit_all();
        }
        "attach" => {
            let rows = req["rows"].as_u64().unwrap_or(40) as u16;
            let cols = req["cols"].as_u64().unwrap_or(120) as u16;
            return attach_client(conn, reader, &name, rows, cols);
        }
        other => Err(format!("unknown request {other:?}")),
    };
    let v = match answer {
        Ok(v) => v,
        Err(e) => json!({ "error": e }),
    };
    let _ = conn.write_all(format!("{v}\n").as_bytes());
}

fn agent_of(cmd: &str) -> Option<String> {
    let first = cmd.split_whitespace().find(|w| !w.contains('='))?;
    let prog = first.rsplit('/').next()?;
    crate::workspace::AGENTS.iter().find(|a| a.cmd == prog).map(|a| a.cmd.to_string())
}

/// Start `cmd` in a PTY under the user's shell, in `cwd`, with `env`.
fn spawn_job(name: &str, cmd: &str, cwd: &Path, env: HashMap<String, String>, tab: bool) -> Result<JobInfo, String> {
    let shell = env
        .get("SHELL")
        .cloned()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/zsh".into());
    let (rows, cols) = (40u16, 120u16);
    let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    let mut master: libc::c_int = -1;
    // Everything the child needs, made before the fork.
    let c = |s: &str| std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default();
    let prog = c(&shell);
    let args = [c(&shell), c("-c"), c(cmd)];
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut vars: HashMap<String, String> = env;
    vars.insert("TERM".into(), "xterm-256color".into());
    vars.insert("COLORTERM".into(), "truecolor".into());
    vars.insert("DECK_JOB".into(), name.into());
    let envs: Vec<std::ffi::CString> = vars.iter().map(|(k, v)| c(&format!("{k}={v}"))).collect();
    let mut envp: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    let dir = c(&cwd.to_string_lossy());
    // SAFETY: forkpty, then in the child only async-signal-safe calls
    // (chdir, execve, _exit) on memory prepared above.
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
    if pid < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if pid == 0 {
        unsafe {
            libc::chdir(dir.as_ptr());
            libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    let info = JobInfo {
        name: name.to_string(),
        cmd: cmd.to_string(),
        cwd: cwd.to_path_buf(),
        running: true,
        exit: None,
        pid,
        started: crate::local::now(),
        agent: agent_of(cmd),
        tab,
    };
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(log_file(name)).ok();
    if let Some(l) = log.as_mut() {
        let _ = write!(l, "\r\n\x1b[2m[deck: started {cmd}]\x1b[0m\r\n");
    }
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).insert(
        name.to_string(),
        Job {
            info: info.clone(),
            env: vars,
            master,
            ring: Vec::new(),
            clients: Vec::new(),
            size: (rows, cols),
        },
    );
    save_state();
    // Output: to the ring, the log and every attached client.
    let key = name.to_string();
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let chunk = &buf[..n as usize];
            if let Some(l) = log.as_mut() {
                let _ = l.write_all(chunk);
            }
            let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(job) = jobs.get_mut(&key).filter(|j| j.info.pid == pid) else { break };
            job.ring.extend_from_slice(chunk);
            if job.ring.len() > RING {
                let cut = job.ring.len() - RING;
                job.ring.drain(..cut);
            }
            job.clients.retain_mut(|c| c.write_all(chunk).is_ok());
        }
        trim_log(&log_file(&key));
    });
    // Its end: the exit code, said to the clients, kept for `list`.
    let key = name.to_string();
    std::thread::spawn(move || {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { 128 + libc::WTERMSIG(status) };
        let note = format!("\r\n\x1b[2m[deck: exited {code}]\x1b[0m\r\n");
        {
            let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = jobs.get_mut(&key).filter(|j| j.info.pid == pid) {
                job.info.running = false;
                job.info.exit = Some(code);
                job.ring.extend_from_slice(note.as_bytes());
                job.clients.retain_mut(|c| c.write_all(note.as_bytes()).is_ok());
            }
        }
        if let Ok(mut l) = std::fs::OpenOptions::new().append(true).open(log_file(&key)) {
            let _ = l.write_all(note.as_bytes());
        }
        // A tab's shell that ended (`exit`): the tab closes, so the session
        // goes, and the attached pane's connection with it.
        let tab = JOBS.lock().unwrap_or_else(|e| e.into_inner()).get(&key).is_some_and(|j| j.info.tab && j.info.pid == pid);
        if tab {
            if let Some(mut job) = JOBS.lock().unwrap_or_else(|e| e.into_inner()).remove(&key) {
                for c in &mut job.clients {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
                unsafe { libc::close(job.master) };
            }
            let _ = std::fs::remove_file(log_file(&key));
        }
        save_state();
        // Stopped or restarted on purpose: not a failure.
        let meant = STOPPING.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        crate::daemon_menu::job_ended(&key, code, tab || meant);
    });
    Ok(info)
}

/// Stop a job's process group: TERM, then KILL if it lingers. With
/// `forget`, it leaves the list and its clients are let go.
fn kill_job(name: &str, forget: bool) -> bool {
    let (pid, running, master) = {
        let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        match jobs.get(name) {
            Some(j) => (j.info.pid, j.info.running, j.master),
            None => return false,
        }
    };
    if running {
        STOPPING.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string());
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
            libc::kill(pid, libc::SIGTERM);
        }
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until && unsafe { libc::kill(pid, 0) } == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe {
            if libc::kill(pid, 0) == 0 {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    if forget {
        let job = JOBS.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
        if let Some(mut job) = job {
            for c in &mut job.clients {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
            unsafe { libc::close(master) };
        }
        save_state();
    }
    true
}

fn restart_job(name: &str) -> Result<JobInfo, String> {
    let (cmd, cwd, env, clients, tab) = {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        let j = jobs.get_mut(name).ok_or_else(|| format!("no job {name:?}"))?;
        (j.info.cmd.clone(), j.info.cwd.clone(), j.env.clone(), std::mem::take(&mut j.clients), j.info.tab)
    };
    kill_job(name, false);
    let info = spawn_job(name, &cmd, &cwd, env, tab)?;
    // Attached terminals stay attached to the new run.
    if let Some(j) = JOBS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) {
        j.clients = clients;
        let size = j.size;
        resize(j.master, size.0, size.1);
    }
    Ok(info)
}

fn quit_all() -> ! {
    let names: Vec<String> = JOBS.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
    for n in &names {
        kill_job(n, false);
    }
    // Remembered as running? No: quitting the service stops them for good.
    {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        for j in jobs.values_mut() {
            j.info.running = false;
        }
    }
    save_state();
    let _ = std::fs::remove_file(socket());
    std::process::exit(0);
}

/// The menu bar icon's actions, which only macOS has yet.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn quit() -> ! {
    quit_all()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn stop_named(name: &str) {
    kill_job(name, true);
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn restart_named(name: &str) {
    let _ = restart_job(name);
}

fn resize(master: i32, rows: u16, cols: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) };
}

/// A client takes over the connection: what the job printed so far, then
/// everything it prints; its keys and size go to the job.
fn attach_client(mut conn: UnixStream, mut reader: BufReader<UnixStream>, name: &str, rows: u16, cols: u16) {
    let master = {
        let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(job) = jobs.get_mut(name) else {
            let _ = conn.write_all(format!("{}\n", json!({ "error": format!("no job {name:?}") })).as_bytes());
            return;
        };
        let _ = conn.write_all(b"{\"ok\":true}\n");
        // A clean screen, then the past.
        let _ = conn.write_all(b"\x1bc");
        let _ = conn.write_all(&job.ring);
        let _ = conn.set_write_timeout(Some(Duration::from_millis(500)));
        if let Ok(c) = conn.try_clone() {
            job.clients.push(c);
        }
        job.size = (rows, cols);
        job.master
    };
    // One size off, then the right one: a full-screen program redraws even
    // when the size did not change.
    resize(master, rows, cols.saturating_sub(1).max(1));
    std::thread::sleep(Duration::from_millis(30));
    resize(master, rows, cols);
    let mut kind = [0u8; 1];
    loop {
        if reader.read_exact(&mut kind).is_err() {
            break;
        }
        let master = match JOBS.lock().unwrap_or_else(|e| e.into_inner()).get(name) {
            Some(j) => j.master,
            None => break,
        };
        match kind[0] {
            0 => {
                let mut len = [0u8; 4];
                if reader.read_exact(&mut len).is_err() {
                    break;
                }
                let mut data = vec![0u8; u32::from_be_bytes(len).min(1 << 20) as usize];
                if reader.read_exact(&mut data).is_err() {
                    break;
                }
                unsafe { libc::write(master, data.as_ptr().cast(), data.len()) };
            }
            1 => {
                let mut sz = [0u8; 4];
                if reader.read_exact(&mut sz).is_err() {
                    break;
                }
                let (r, c) = (u16::from_be_bytes([sz[0], sz[1]]), u16::from_be_bytes([sz[2], sz[3]]));
                if let Some(j) = JOBS.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) {
                    j.size = (r, c);
                }
                resize(master, r, c);
            }
            _ => break,
        }
    }
}

/// The last `lines` lines of a job's output, as plain text.
pub fn tail_text(name: &str, lines: usize) -> String {
    tail_log(name, lines)
}

/// The last `lines` lines of a job's log, escapes removed.
fn tail_log(name: &str, lines: usize) -> String {
    let path = log_file(name);
    let Ok(mut f) = std::fs::File::open(&path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let take = len.min(512 * 1024);
    use std::io::Seek;
    let _ = f.seek(std::io::SeekFrom::Start(len - take));
    let mut raw = Vec::new();
    let _ = f.read_to_end(&mut raw);
    let text = plain(&String::from_utf8_lossy(&raw));
    let all: Vec<&str> = text.lines().collect();
    let from = all.len().saturating_sub(lines);
    let mut out = all[from..].join("\n");
    out.push('\n');
    out
}

/// Terminal output as plain text: escape sequences out, a carriage return
/// keeps what came after it on that line.
pub fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut line = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for d in it.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') => {
                    // OSC / DCS / APC: to BEL or ST.
                    while let Some(d) = it.next() {
                        if d == '\x07' || (d == '\x1b' && it.peek() == Some(&'\\')) {
                            if d == '\x1b' {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if it.peek() != Some(&'\n') {
                    line.clear();
                }
            }
            '\n' => {
                out.push_str(line.trim_end());
                out.push('\n');
                line.clear();
            }
            '\x08' => {
                line.pop();
            }
            c if c.is_control() && c != '\t' => {}
            c => line.push(c),
        }
    }
    out.push_str(&line);
    out
}

fn trim_log(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() <= LOG_MAX {
        return;
    }
    if let Ok(b) = std::fs::read(path) {
        let half = b.len() / 2;
        let _ = std::fs::write(path, &b[half..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_reads_as_plain_text() {
        assert_eq!(plain("\x1b[32mready\x1b[0m in 3ms\r\n"), "ready in 3ms\n");
        assert_eq!(plain("50%\r100%\n"), "100%\n");
        assert_eq!(plain("\x1b]0;title\x07hi\n"), "hi\n");
        assert_eq!(plain("ab\x08c\n"), "ac\n");
    }

    #[test]
    fn start_arguments_parse() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let (n, cwd, cmd) = parse_start(&a("--name web --cwd /tmp npm run dev")).unwrap();
        assert_eq!((n.as_str(), cwd, cmd.as_str()), ("web", PathBuf::from("/tmp"), "npm run dev"));
        let words: Vec<String> = vec!["sh".into(), "-c".into(), "echo hi; exit 4".into()];
        assert_eq!(parse_start(&words).unwrap().2, "sh -c 'echo hi; exit 4'");
        let (n, _, cmd) = parse_start(&a("pnpm dev --port 3001")).unwrap();
        assert_eq!((n.as_str(), cmd.as_str()), ("dev", "pnpm dev --port 3001"));
        assert!(parse_start(&a("--name x")).is_err());
        assert_eq!(job_name("PORT=1 cargo watch -x test"), "cargo");
    }

    /// A real job in a real daemon, on a socket of its own.
    #[test]
    fn jobs_run_log_and_end() {
        let dir = std::env::temp_dir().join(format!("deckd-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("jobs")).unwrap();
        std::env::set_var("DECK_DATA_DIR", &dir);
        let info = spawn_job("t-echo", "printf 'hello\\n'; exit 3", Path::new("/tmp"), job_env(), false).unwrap();
        assert!(info.pid > 0);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let done = snapshot().into_iter().find(|j| j.name == "t-echo").is_some_and(|j| !j.running);
            if done || Instant::now() > until {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let j = snapshot().into_iter().find(|j| j.name == "t-echo").unwrap();
        assert_eq!(j.exit, Some(3));
        assert!(tail_log("t-echo", 10).contains("hello"));
        kill_job("t-echo", true);
        let _ = std::fs::remove_file(log_file("t-echo"));
    }
}
