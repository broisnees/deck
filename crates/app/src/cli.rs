//! `deck <group> <verb> [--key value]... [text]`: a thin client over the
//! control socket. Output is JSON so agents can parse it.

use crate::control;
use serde_json::{Map, Value};

const HELP: &str = "\
deck: drive the deck window you are running in.

  deck doctor                       show the PATH and which agent CLIs deck can find
  deck mcp                          serve these commands as MCP tools on stdin/stdout
  deck open [--name NAME] [--tab] URL
      open URL in a browser beside this terminal; --tab opens a new tab instead,
      --name keeps the page under NAME so opening NAME again navigates that pane
  deck pane list
  deck pane split [--dir right|down] [--kind terminal|browser] [--cmd CMD] [--url URL] [--pane N]
  deck pane send  --pane N [--no-enter] TEXT     (to a chat pane: sends TEXT as a message)
  deck run [--name NAME] [--dir right|down] [--cwd DIR] CMD
      runs CMD in a terminal split beside you; with --name, running it again
      reuses that pane (Ctrl-C, then CMD). Use it for dev servers and watchers.
  deck run --bg [--name NAME] [--cwd DIR] CMD     same as `deck job start`
  deck job start [--name NAME] [--cwd DIR] CMD
      runs CMD in deck's background service: it keeps running when Deck quits,
      starts again after the Mac restarts, shows in the menu bar and as a tab in
      Deck's status bar. Starting NAME again restarts it. Dev servers, watchers,
      agents (`deck job start --name fixer claude`). Works with Deck closed.
  deck job list                     every job: name, command, running, exit code
  deck job logs  --name NAME [--lines N]      its output, plain text
  deck job wait  --name NAME --for TEXT[|TEXT] | --exit [--timeout SECS]
  deck job show  --name NAME        its terminal in Deck
  deck job restart --name NAME
  deck job stop  --name NAME        stop it and forget it
  deck pane prompt --pane N|--name NAME TEXT [--timeout SECS]
      send an agent a message, wait until it has worked and stopped, print its answer
  deck report working|blocked|done|error|clear [MESSAGE] [--progress N]
      set this pane's state and message in Deck's sidebar (blocked also notifies you)
  deck events [--for working,waiting,idle,opened,closed] [--once] [--json]
      a line for every change in this window as it happens
  deck pane diff --pane N | --name NAME [--turn]
      what the terminal agent there changed since its current (or last) turn began
  deck pane key --pane N KEY        a key press: enter, escape, tab, up, ctrl+c, alt+b, …
  deck history [QUERY]              saved chat threads, newest first (open, closed, older)
  deck resume ID                    reopen a thread (an id prefix from `deck history` works)
  deck page open usage|packages     open Deck's Usage (cost by day and model) or Packages page
  deck ssh HOST [--dir D] [--agent claude|codex|…]
      this terminal becomes a shell on HOST in D (or the agent there); works without Deck
  deck remote list | add HOST [--dir D] | remove HOST
  deck remote open HOST [--dir D] [--agent claude|codex|…] [--chat]
      a terminal on HOST over SSH, or an agent there: its own interface, or with
      --chat a Deck chat whose agent runs on HOST
  deck memory add [--global] TEXT | list [--all] | search QUERY | forget ID
      notes every coding agent shares; Claude Code gets them at every session start
  deck context                      what a new agent session is told: Deck, and this project's memory
  deck do ACTION | --list             run any ⌘K action by its name, e.g. `deck do Split Right`
  deck usage                        plan limits, this conversation, today's tokens and cost
  deck pane read  --pane N [--lines N]     --lines: just the last N lines (logs)
  deck pane wait  --pane N --for TEXT[|TEXT] [--timeout SECS]
      until the pane prints TEXT, e.g. --for \"ready|Local:\"; exit 1 on timeout
      (--name NAME works in place of --pane N for read, wait, send, focus, close)
  deck pane focus --pane N
  deck pane close --pane N
  deck tab new [--kind terminal|browser|chat] [--chat claude|codex|…] [--agent claude|…] [--cmd CMD] [--url URL] [--name NAME]
      --chat starts a structured chat with that agent; --agent runs its TUI in a terminal
  deck browser open [--pane N | --name NAME] [--tab] URL
  deck browser eval [--pane N] JS
  deck read URL [--name NAME] [--scroll N] [--max CHARS] [--json]
      the page as Markdown, read in a background page: real WebKit with your
      sign-ins, out of sight (a tab in the status bar). Works where plain
      fetches are turned away (Reddit, X, sites behind sign-in). --scroll loads
      N more screens of an endless feed. The page stays open under NAME.
  deck browser open --bg [--name NAME] URL   a background page: no split, a tab in the status bar
  deck browser read [--scroll N]    the page you are on, as Markdown
  deck browser snapshot             the page's headings and controls, numbered [N] for the verbs below
  deck browser click TARGET         TARGET: a [N] from snapshot, text=Some text, or a CSS selector
  deck browser type TARGET TEXT [--enter] [--append]
      replaces the field's text (--append adds to it); a <select> picks the option
  deck browser key KEY              Enter, Escape, Tab, Shift+Tab, ArrowDown, Backspace, …
  deck browser scroll [up|down|top|bottom|TARGET]
  deck browser back | forward | reload
  deck browser wait --for TEXT[|TEXT] | --selector SEL [--gone] [--timeout SECS]
      until the page shows TEXT (any case) or SEL (default 30 s); exit 1 on timeout
  deck browser console | network | errors | logs [--since SEQ] [--limit N] [--failed] [--filter TEXT] [--clear]
      what the page logged, threw and loaded, oldest first; pass last_seq back as --since
  deck browser shot [PATH]          a PNG of just the page
  deck browser dialog [accept|dismiss [TEXT]] [--user U --password P]
      the alert, confirm, prompt or login the page has up, or its answer
      click, type and key answer dialogs they raise themselves (OK; --dismiss to cancel)
      every browser verb takes --pane N or --name NAME; without, the page in your tab
  deck settings open [--page general|appearance|terminal|browser|agents|models|permissions|keyboard|snippets]
  deck window shot [PATH]            a PNG of this deck window (default: shot.png in deck's data folder)
  deck task new [--agent claude|codex|…] [--cwd DIR] BRIEF
      a sub-agent on its own branch and worktree of this project
  deck task list
  deck task show    --task ID       status, changes and its last reply
  deck task wait    --task ID [--timeout SECS]   until its agent stops working
  deck task diff    --task ID
  deck task send    --task ID TEXT  a follow-up for its agent
  deck task merge   --task ID       squash into the base branch (or: pr, discard)

Run it from a terminal inside deck; it reads DECK_SOCKET and DECK_TOKEN.
Output is JSON on stdout; errors go to stderr with exit code 1.
";

/// Writing into a closed pipe (`deck pane list | head`) ends the command
/// quietly, as it does for any Unix tool, instead of panicking on EPIPE.
fn quiet_broken_pipe() {
    extern "C" {
        fn signal(sig: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: restores the default disposition before any output is written.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

pub fn run(args: &[String]) -> i32 {
    quiet_broken_pipe();
    if args.first().map(String::as_str) == Some("doctor") {
        return doctor();
    }
    if args.first().map(String::as_str) == Some("mcp") {
        // `deck mcp --host H --dir D`: tools bound to one machine, for a
        // chat whose agent works there.
        let flag = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
        if let Some(host) = flag("--host") {
            return crate::host_mcp::run(&host, &flag("--dir").unwrap_or_else(|| "~".into()));
        }
        return crate::mcp::run();
    }
    match args.first().map(String::as_str) {
        Some("report") => return report(&args[1..]),
        Some("events") => return events(&args[1..]),
        _ => {}
    }
    if args.len() >= 2 && args[0] == "pane" && args[1] == "prompt" {
        return prompt(&args[2..]);
    }
    // Background jobs live in deck's background service, so these work
    // from any terminal, with Deck open or not.
    match args.first().map(String::as_str) {
        Some("daemon") => return crate::daemon::serve(),
        Some("memory") => return crate::memory::cli(&args[1..]),
        Some("remote") => return remote(&args[1..]),
        // `deck ssh HOST [--dir D] [--agent A]`: this terminal becomes a
        // shell (or an agent) on HOST, in that folder. No Deck needed.
        Some("ssh") => return ssh_here(&args[1..]),
        Some("hook") if args.get(1).map(String::as_str) == Some("session-start") => return crate::memory::session_start_hook(),
        Some("context") => {
            let cwd = std::env::current_dir().unwrap_or_default();
            print!("{}", crate::memory::brief(&cwd, std::env::var_os("DECK_SOCKET").is_some()));
            return 0;
        }
        Some("job") => return crate::daemon::cli(&args[1..]),
        Some("run") if args.iter().any(|a| a == "--bg") => {
            let mut rest: Vec<String> = vec!["start".into()];
            rest.extend(args[1..].iter().filter(|a| *a != "--bg").cloned());
            return crate::daemon::cli(&rest);
        }
        _ => {}
    }
    if args.len() >= 2 && args[0] == "task" && args[1] == "wait" {
        return wait(&args[2..]);
    }
    if args.len() >= 2 && args[0] == "pane" && args[1] == "wait" {
        return pane_wait(&args[2..]);
    }
    match parse(args) {
        Ok(None) => {
            print!("{HELP}");
            0
        }
        Ok(Some((method, params))) => match call(&method, params.clone()) {
            Ok(v) if method == "pane.diff" => {
                print!("{}", v["diff"].as_str().unwrap_or(""));
                0
            }
            Ok(v) if method == "browse.read_url" && params.get("json") != Some(&Value::Bool(true)) => {
                println!("# {}\n<{}>\n", v["title"].as_str().unwrap_or(""), v["url"].as_str().unwrap_or(""));
                println!("{}", v["markdown"].as_str().unwrap_or(""));
                0
            }
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                0
            }
            Err(e) => {
                eprintln!("deck: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("deck: {e}\n\n{HELP}");
            2
        }
    }
}

/// `deck remote list | add HOST [--dir D] | remove HOST | open HOST [--dir D]
/// [--agent A] [--chat]`.
fn remote(args: &[String]) -> i32 {
    let flag = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let host = args.get(1).filter(|h| !h.starts_with("--")).cloned();
    let out = |v: Value| {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        0
    };
    match args.first().map(String::as_str) {
        Some("list") | None => out(serde_json::json!({ "hosts": crate::remote::hosts() })),
        Some("add") => match host {
            Some(h) => match crate::remote::add(&h, flag("--name").as_deref(), flag("--key").as_deref(), flag("--dir")) {
                Ok(h) => out(serde_json::json!({ "added": h })),
                Err(e) => {
                    eprintln!("deck: {e}");
                    1
                }
            },
            None => {
                eprintln!("deck: remote add needs a host, e.g. `deck remote add me@10.0.0.5`");
                2
            }
        },
        Some("remove") => match host.map(|h| crate::remote::remove(&h)) {
            Some(Ok(true)) => out(serde_json::json!({ "removed": true })),
            Some(Ok(false)) => {
                eprintln!("deck: only hosts added in Deck can be removed here; SSH's own are in its config");
                1
            }
            Some(Err(e)) => {
                eprintln!("deck: {e}");
                1
            }
            None => 2,
        },
        Some("open") => {
            let Some(h) = host else {
                eprintln!("deck: remote open needs a host (see `deck remote list`)");
                return 2;
            };
            let mut p = Map::new();
            p.insert("host".into(), h.into());
            if let Some(d) = flag("--dir") {
                p.insert("dir".into(), d.into());
            }
            if let Some(a) = flag("--agent") {
                p.insert("agent".into(), a.into());
            }
            if args.iter().any(|a| a == "--chat") {
                p.insert("chat".into(), true.into());
            }
            match call("remote.open", Value::Object(p)) {
                Ok(v) => out(v),
                Err(e) => {
                    eprintln!("deck: {e}");
                    1
                }
            }
        }
        Some(_) => {
            eprintln!("usage: deck remote list | add HOST [--name N] [--key PEM] [--dir D] | remove HOST | open HOST [--dir D] [--agent claude] [--chat]");
            2
        }
    }
}

fn ssh_here(args: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;
    let flag = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let Some(host) = args.first().filter(|h| !h.starts_with("--")).cloned() else {
        eprintln!("usage: deck ssh HOST [--dir DIR] [--agent claude|codex|…]   (hosts: `deck remote list`)");
        return 2;
    };
    let dir = flag("--dir").unwrap_or_else(|| "~".into());
    let agent = flag("--agent");
    let line = crate::remote::remote_line(&dir, agent.as_deref());
    eprintln!("Connecting to {host}…");
    let err = std::process::Command::new("ssh")
        .args(["-t", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=30"])
        .args(crate::remote::ssh_flags())
        .arg(&host)
        .arg(line)
        .exec();
    eprintln!("deck: could not run ssh: {err}");
    1
}

/// One parsed command: a socket call, or a page action that makes its own.
fn call(method: &str, mut params: Value) -> Result<Value, String> {
    // A relative PATH is this shell's; the app runs elsewhere.
    if let Some(out) = params.get("out").and_then(Value::as_str) {
        let out = std::path::Path::new(out);
        if out.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                params["out"] = cwd.join(out).to_string_lossy().into_owned().into();
            }
        }
    }
    match method.strip_prefix("browse.") {
        Some("read_url") => crate::browse::read_url(&params),
        Some(verb) => crate::browse::run(verb, params),
        None => control::call(method, params),
    }
}

/// `deck report working|blocked|done|error|clear [MESSAGE] [--progress N]`:
/// tell Deck how the program in this pane is doing. An escape written to
/// the terminal, so it works from any program, in a pane or a job.
fn report(args: &[String]) -> i32 {
    let Some(state) = args.first().map(|s| s.to_lowercase()) else {
        eprintln!("deck: report needs a state: working, blocked, done, error or clear");
        return 2;
    };
    if !matches!(state.as_str(), "working" | "blocked" | "done" | "error" | "clear") {
        eprintln!("deck: report state is working, blocked, done, error or clear, not {state:?}");
        return 2;
    }
    let mut progress = None;
    let mut words = Vec::new();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        if a == "--progress" {
            progress = it.next().and_then(|p| p.parse::<u8>().ok()).map(|p| p.min(100));
        } else {
            words.push(a.as_str());
        }
    }
    let msg = words.join(" ").replace(['\x07', '\x1b'], "");
    let mut out = format!("\x1b]6973;report;{state};{msg}\x07");
    match (state.as_str(), progress) {
        ("working", Some(p)) => out.push_str(&format!("\x1b]9;4;1;{p}\x07")),
        ("working", None) => out.push_str("\x1b]9;4;3\x07"),
        ("error", _) => out.push_str("\x1b]9;4;2;100\x07"),
        _ => out.push_str("\x1b]9;4;0\x07"),
    }
    // The terminal itself, so a redirected stdout still reaches it.
    use std::io::Write;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .and_then(|mut t| t.write_all(out.as_bytes()));
    if written.is_err() {
        print!("{out}");
    }
    0
}

/// `deck events [--for working,waiting,idle,closed,opened] [--once] [--json]`:
/// what changes in this window, a line each, as it happens: panes opened and
/// closed, agents starting and stopping work or starting to wait on you.
fn events(args: &[String]) -> i32 {
    let mut want: Vec<String> = Vec::new();
    let mut once = false;
    let mut json_out = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--for" => want = it.next().map(|w| w.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default(),
            "--once" => once = true,
            "--json" => json_out = true,
            _ => {}
        }
    }
    let snapshot = || -> Result<std::collections::HashMap<u64, (String, String, String)>, String> {
        let v = control::call("pane.list", Value::Object(Map::new()))?;
        Ok(v["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some((
                    p["id"].as_u64()?,
                    (
                        p["status"].as_str().unwrap_or("none").to_string(),
                        p["title"].as_str().unwrap_or("").to_string(),
                        p["kind"].as_str().unwrap_or("").to_string(),
                    ),
                ))
            })
            .collect())
    };
    let mut last = match snapshot() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("deck: {e}");
            return 1;
        }
    };
    loop {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let now = match snapshot() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("deck: {e}");
                return 1;
            }
        };
        let mut said = Vec::new();
        for (id, (status, title, kind)) in &now {
            match last.get(id) {
                None => said.push((*id, "opened".to_string(), title.clone(), kind.clone())),
                Some((was, ..)) if was != status && status != "none" => said.push((*id, status.clone(), title.clone(), kind.clone())),
                _ => {}
            }
        }
        for (id, (_, title, kind)) in &last {
            if !now.contains_key(id) {
                said.push((*id, "closed".to_string(), title.clone(), kind.clone()));
            }
        }
        for (id, event, title, kind) in said {
            if !want.is_empty() && !want.iter().any(|w| *w == event) {
                continue;
            }
            if json_out {
                println!("{}", serde_json::json!({ "pane": id, "event": event, "title": title, "kind": kind }));
            } else {
                println!("{event}\tpane {id}\t{kind}\t{title}");
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if once {
                return 0;
            }
        }
        last = now;
    }
}

/// `deck pane prompt --pane N | --name NAME TEXT [--timeout SECS]`: send an
/// agent a message, wait until it has worked and stopped, print its answer.
fn prompt(args: &[String]) -> i32 {
    let mut target = Map::new();
    let mut timeout = 600u64;
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pane" => {
                if let Some(n) = it.next().and_then(|n| n.parse::<u64>().ok()) {
                    target.insert("pane".into(), n.into());
                }
            }
            "--name" => {
                if let Some(n) = it.next() {
                    target.insert("name".into(), n.clone().into());
                }
            }
            "--timeout" => timeout = it.next().and_then(|t| t.parse().ok()).unwrap_or(timeout),
            w => words.push(w.to_string()),
        }
    }
    if target.is_empty() || words.is_empty() {
        eprintln!("deck: pane prompt needs --pane N (or --name NAME) and the message");
        return 2;
    }
    let status_of = |t: &Map<String, Value>| -> Option<String> {
        let v = control::call("pane.list", Value::Object(Map::new())).ok()?;
        let id = match t.get("pane").and_then(Value::as_u64) {
            Some(id) => id,
            None => return Some("unknown".into()),
        };
        v["panes"].as_array()?.iter().find(|p| p["id"].as_u64() == Some(id)).and_then(|p| p["status"].as_str().map(str::to_string))
    };
    let mut send = target.clone();
    send.insert("text".into(), words.join(" ").into());
    if let Err(e) = control::call("pane.send", Value::Object(send)) {
        eprintln!("deck: {e}");
        return 1;
    }
    let start = std::time::Instant::now();
    let mut seen_work = false;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let st = status_of(&target).unwrap_or_default();
        if st == "working" {
            seen_work = true;
        }
        // Done once it worked and stopped (or, for a pane that never says,
        // after its output has been quiet a while).
        if (seen_work && st != "working") || (st == "unknown" && start.elapsed().as_secs() > 20) {
            break;
        }
        if st == "waiting" && seen_work {
            break;
        }
        if start.elapsed().as_secs() > timeout {
            eprintln!("deck: timed out waiting for the answer");
            return 1;
        }
    }
    let mut read = target.clone();
    read.insert("lines".into(), 80u64.into());
    match control::call("pane.read", Value::Object(read)) {
        Ok(v) => {
            println!("{}", v["text"].as_str().unwrap_or(""));
            0
        }
        Err(e) => {
            eprintln!("deck: {e}");
            1
        }
    }
}

/// `deck doctor`: what the app would see at launch. The PATH is the one a
/// Finder or Dock launch ends up with, and each agent CLI says whether it
/// is reachable on it.
fn doctor() -> i32 {
    crate::adopt_shell_path();
    let path = std::env::var("PATH").unwrap_or_default();
    println!("PATH {path}");
    let mut found = 0;
    for agent in crate::workspace::AGENTS {
        match term::which(agent.cmd) {
            Some(p) => {
                found += 1;
                println!("  {:<13} {}", agent.cmd, p.display());
            }
            None => println!("  {:<13} not found", agent.cmd),
        }
    }
    if found == 0 {
        eprintln!("deck: no agent CLI is reachable; chats will not start");
        return 1;
    }
    0
}

/// `deck task wait --task ID`: ask every two seconds until the task's agent
/// has worked and stopped (finished, or waiting on you), then print it.
fn wait(args: &[String]) -> i32 {
    let mut task = None;
    let mut timeout = 30 * 60u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--task" => task = it.next().cloned(),
            "--timeout" => timeout = it.next().and_then(|t| t.parse().ok()).unwrap_or(timeout),
            other => task = task.or(Some(other.to_string())),
        }
    }
    let Some(task) = task else {
        eprintln!("deck: task wait needs --task ID");
        return 2;
    };
    let start = std::time::Instant::now();
    let mut seen_working = false;
    loop {
        let v = match control::call("task.show", serde_json::json!({ "task": task })) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("deck: {e}");
                return 1;
            }
        };
        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        seen_working |= status == "working";
        // An agent that has not picked the brief up yet is not done; give
        // it twenty seconds to start before taking idle at its word.
        let settled = status != "working"
            && (seen_working || start.elapsed().as_secs() > 20 || status == "closed");
        if settled || start.elapsed().as_secs() >= timeout {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            return if settled { 0 } else { 1 };
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// `deck pane wait --pane N --for TEXT`: poll the end of a pane until a line
/// matches one of the `|`-separated texts, e.g. `--for "ready|Local:"`.
fn pane_wait(args: &[String]) -> i32 {
    let mut params = Map::new();
    let mut want = String::new();
    let mut timeout = 120u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pane" => {
                if let Some(n) = it.next().and_then(|n| n.parse::<u64>().ok()) {
                    params.insert("pane".into(), n.into());
                }
            }
            "--name" => {
                if let Some(n) = it.next() {
                    params.insert("name".into(), n.clone().into());
                }
            }
            "--for" => want = it.next().cloned().unwrap_or_default(),
            "--timeout" => timeout = it.next().and_then(|t| t.parse().ok()).unwrap_or(timeout),
            other => {
                if want.is_empty() {
                    want = other.to_string();
                }
            }
        }
    }
    if want.trim().is_empty() || params.is_empty() {
        eprintln!("deck: pane wait needs --pane N (or --name NAME) and --for TEXT");
        return 2;
    }
    match wait_text(Value::Object(params), &want, timeout) {
        Ok(v) => {
            println!("{v}");
            0
        }
        Err(e) => {
            eprintln!("deck: {e}");
            1
        }
    }
}

/// Poll the last 200 lines of a pane until one holds any of the
/// `|`-separated texts in `want`. `deck pane wait` and `deck_wait` share it.
pub fn wait_text(mut params: Value, want: &str, timeout: u64) -> Result<Value, String> {
    let needles: Vec<&str> = want.split('|').map(str::trim).filter(|n| !n.is_empty()).collect();
    if needles.is_empty() {
        return Err("nothing to wait for".into());
    }
    params["lines"] = 200.into();
    let start = std::time::Instant::now();
    loop {
        let v = control::call("pane.read", params.clone()).map_err(|e| e.to_string())?;
        let text = v.get("text").and_then(Value::as_str).unwrap_or("");
        if let Some(line) = text.lines().rev().find(|l| needles.iter().any(|n| l.contains(n))) {
            return Ok(serde_json::json!({ "matched": line.trim(), "seconds": start.elapsed().as_secs() }));
        }
        if start.elapsed().as_secs() >= timeout {
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(20)..].join("\n");
            return Err(format!("no {want:?} after {timeout}s; the pane ends with:\n{tail}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// `deck browser VERB …`: the verbs that drive a page. Those that run in
/// the page come back as `browse.VERB` (see `browse.rs`); the rest are
/// plain calls.
fn parse_browser(verb: &str, rest: &[String]) -> Result<(String, Value), String> {
    const FLAGS: &[&str] = &["enter", "append", "failed", "clear", "tab", "gone", "dismiss", "bg"];
    const NUMBERS: &[&str] = &["pane", "since", "limit", "timeout", "scroll", "max"];
    let mut params = Map::new();
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            positional.extend(it.by_ref().cloned());
            break;
        }
        let Some(key) = a.strip_prefix("--").filter(|k| !k.is_empty()) else {
            positional.push(a.clone());
            continue;
        };
        if FLAGS.contains(&key) {
            params.insert(key.into(), Value::Bool(true));
            continue;
        }
        let v = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        let v = if NUMBERS.contains(&key) {
            Value::from(
                v.parse::<u64>()
                    .map_err(|_| format!("--{key} takes a number, got {v:?}"))?,
            )
        } else {
            Value::String(v.clone())
        };
        params.insert(key.into(), v);
    }
    let mut put = |key: &str, v: String| {
        params.entry(key).or_insert(Value::String(v));
    };
    let method = match verb {
        "open" => {
            if !positional.is_empty() {
                put("url", positional.join(" "));
            }
            if !params.contains_key("url") {
                return Err("browser open needs a URL".into());
            }
            "browser.open".to_string()
        }
        "read" => {
            if !positional.is_empty() {
                return Err("browser read takes no URL; use `deck read URL` to open one".into());
            }
            "browse.read".into()
        }
        "console" | "network" | "errors" | "logs" => {
            if !positional.is_empty() {
                return Err(format!("browser {verb} takes no positional arguments"));
            }
            let kind = if verb == "logs" { "all" } else { verb };
            params.entry("kind").or_insert(kind.into());
            "browser.logs".into()
        }
        "shot" => {
            if !positional.is_empty() {
                put("out", positional.join(" "));
            }
            "browser.shot".into()
        }
        "click" | "scroll" => {
            let key = if verb == "click" { "target" } else { "to" };
            if !positional.is_empty() {
                put(key, positional.join(" "));
            }
            if verb == "click" && !params.contains_key("target") {
                return Err("click needs a TARGET: a [N] from snapshot, text=…, or a CSS selector".into());
            }
            format!("browse.{verb}")
        }
        "type" => {
            let mut words = positional.into_iter();
            if let Some(t) = words.next() {
                put("target", t);
            }
            let text: Vec<String> = words.collect();
            if !text.is_empty() {
                put("text", text.join(" "));
            }
            if !params.contains_key("target") || !params.contains_key("text") {
                return Err("type needs a TARGET and the TEXT, e.g. `deck browser type 3 hello`".into());
            }
            "browse.type".into()
        }
        "key" | "press" => {
            if !positional.is_empty() {
                put("key", positional.join(" "));
            }
            if !params.contains_key("key") {
                return Err("key needs a KEY, e.g. Enter, Escape, Tab".into());
            }
            "browse.key".into()
        }
        "wait" => {
            if !positional.is_empty() {
                put("for", positional.join(" "));
            }
            if !params.contains_key("for") && !params.contains_key("selector") {
                return Err("wait needs --for TEXT or --selector SEL".into());
            }
            "browse.wait".into()
        }
        "dialog" => {
            let mut words = positional.into_iter();
            if let Some(a) = words.next() {
                put("action", a);
            }
            let text: Vec<String> = words.collect();
            if !text.is_empty() {
                put("text", text.join(" "));
            }
            "browser.dialog".into()
        }
        "snapshot" | "back" | "forward" | "reload" => {
            if !positional.is_empty() {
                return Err(format!("browser {verb} takes no positional arguments"));
            }
            format!("browse.{verb}")
        }
        v => return Err(format!("no browser verb {v:?}; run `deck help`")),
    };
    Ok((method, Value::Object(params)))
}

fn parse(args: &[String]) -> Result<Option<(String, Value)>, String> {
    // `deck URL` is what $BROWSER runs: cargo, among others, runs $BROWSER
    // as one program, so it can't be `deck open`.
    if let [url] = args {
        if url.starts_with("http://") || url.starts_with("https://") {
            let mut params = Map::new();
            params.insert("url".into(), url.clone().into());
            return Ok(Some(("browser.open".into(), Value::Object(params))));
        }
    }
    // `deck open URL` takes the URL bare too.
    if let [open, rest @ ..] = args {
        if open == "open" {
            let mut params = Map::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--tab" => params.insert("tab".into(), Value::Bool(true)),
                    "--name" if i + 1 < rest.len() => {
                        i += 1;
                        params.insert("name".into(), rest[i].clone().into())
                    }
                    _ => break,
                };
                i += 1;
            }
            if rest[i..].is_empty() {
                return Err("open needs a URL".into());
            }
            params.insert("url".into(), rest[i..].join(" ").into());
            return Ok(Some(("browser.open".into(), Value::Object(params))));
        }
    }
    // Everything in a page, from the browser group, except `eval`.
    if let [group, verb, rest @ ..] = args {
        if group == "browser" && verb != "eval" {
            return parse_browser(verb, rest).map(Some);
        }
    }
    // `deck run [--name N] [--dir right|down] [--cwd DIR] CMD...`: everything
    // after the flags is the command, flags and all.
    // `deck read URL [flags]`: flags anywhere, the rest is the URL.
    if let [read, rest @ ..] = args {
        if read == "read" {
            let mut params = Map::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--json" => {
                        params.insert("json".into(), Value::Bool(true));
                    }
                    "--name" | "--scroll" | "--max" | "--timeout" => {
                        let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                        params.insert(a[2..].into(), Value::String(v.clone()));
                    }
                    url => {
                        params.insert("url".into(), Value::String(url.to_string()));
                    }
                }
            }
            if !params.contains_key("url") {
                return Err("read needs a URL, e.g. `deck read https://www.reddit.com/r/rust`".into());
            }
            return Ok(Some(("browse.read_url".into(), Value::Object(params))));
        }
    }
    // `deck do ACTION NAME…` runs a ⌘K action by its name; `--list` lists them.
    if let [verb, rest @ ..] = args {
        if verb == "do" {
            if rest.is_empty() || rest[0] == "--list" {
                return Ok(Some(("action.list".into(), Value::Object(Map::new()))));
            }
            let mut params = Map::new();
            params.insert("name".into(), rest.join(" ").into());
            return Ok(Some(("action.run".into(), Value::Object(params))));
        }
    }
    if let [usage] = args {
        if usage == "usage" {
            return Ok(Some(("usage.get".into(), Value::Object(Map::new()))));
        }
    }
    if let [verb, rest @ ..] = args {
        if verb == "history" || verb == "resume" {
            let mut params = Map::new();
            let mut words = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--limit" {
                    params.insert("limit".into(), it.next().cloned().unwrap_or_default().into());
                } else {
                    words.push(a.clone());
                }
            }
            let (method, key) = if verb == "history" { ("thread.history", "query") } else { ("thread.resume", "id") };
            if !words.is_empty() {
                params.insert(key.into(), words.join(" ").into());
            }
            return Ok(Some((method.into(), Value::Object(params))));
        }
    }
    if let [run, rest @ ..] = args {
        if run == "run" {
            let mut params = Map::new();
            let mut i = 0;
            while i + 1 < rest.len() && matches!(rest[i].as_str(), "--name" | "--dir" | "--cwd") {
                params.insert(rest[i][2..].into(), Value::String(rest[i + 1].clone()));
                i += 2;
            }
            let cmd = rest[i..].strip_prefix(&["--".to_string()][..]).unwrap_or(&rest[i..]);
            if cmd.is_empty() {
                return Err("run needs a command, e.g. `deck run --name dev npm run dev`".into());
            }
            params.insert("cmd".into(), Value::String(cmd.join(" ")));
            return Ok(Some(("pane.run".into(), Value::Object(params))));
        }
    }
    let [group, verb, rest @ ..] = args else {
        return match args.first().map(String::as_str) {
            None | Some("help" | "-h" | "--help") => Ok(None),
            Some(_) => Err("expected `deck <group> <verb>`".into()),
        };
    };
    let method = format!("{group}.{verb}");
    let mut params = Map::new();
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            positional.extend(it.by_ref().cloned());
            break;
        }
        let Some(key) = a.strip_prefix("--") else {
            positional.push(a.clone());
            continue;
        };
        if let Some(flag) = key.strip_prefix("no-") {
            params.insert(flag.into(), Value::Bool(false));
            continue;
        }
        let v = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
        let v = match key {
            "pane" | "lines" => Value::from(
                v.parse::<u64>()
                    .map_err(|_| format!("--{key} takes a number, got {v:?}"))?,
            ),
            _ => Value::String(v.clone()),
        };
        params.insert(key.into(), v);
    }
    if !positional.is_empty() {
        let key = match method.as_str() {
            "pane.send" | "task.new" | "task.send" => "text",
            "pane.key" => "key",
            "page.open" => "page",
            "browser.open" => "url",
            "browser.eval" => "js",
            "window.shot" => "out",
            m => return Err(format!("{m} takes no positional arguments")),
        };
        params.insert(key.into(), Value::String(positional.join(" ")));
    }
    Ok(Some((method, Value::Object(params))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> Result<Option<(String, Value)>, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn jobs_history_and_pages_parse() {
        assert_eq!(
            p("run --name dev npm start").unwrap(),
            Some(("pane.run".into(), json!({"name": "dev", "cmd": "npm start"})))
        );
        assert_eq!(
            p("pane key --name dev ctrl+c").unwrap(),
            Some(("pane.key".into(), json!({"name": "dev", "key": "ctrl+c"})))
        );
        assert_eq!(p("usage").unwrap(), Some(("usage.get".into(), json!({}))));
        assert_eq!(
            p("history auth bug --limit 5").unwrap(),
            Some(("thread.history".into(), json!({"query": "auth bug", "limit": "5"})))
        );
        assert_eq!(p("resume 9e66").unwrap(), Some(("thread.resume".into(), json!({"id": "9e66"}))));
        assert_eq!(p("page open packages").unwrap(), Some(("page.open".into(), json!({"page": "packages"}))));
        assert_eq!(
            p("read --scroll 2 https://www.reddit.com/r/rust").unwrap(),
            Some(("browse.read_url".into(), json!({"scroll": "2", "url": "https://www.reddit.com/r/rust"})))
        );
        assert!(p("read --json").is_err());
        assert_eq!(
            p("browser open --bg --name r https://x.com").unwrap(),
            Some(("browser.open".into(), json!({"bg": true, "name": "r", "url": "https://x.com"})))
        );
        assert_eq!(
            p("browser read --name r --scroll 1").unwrap(),
            Some(("browse.read".into(), json!({"name": "r", "scroll": 1})))
        );
    }

    #[test]
    fn flags_and_positionals_become_params() {
        assert_eq!(p("").unwrap(), None);
        assert_eq!(
            p("pane list").unwrap(),
            Some(("pane.list".into(), json!({})))
        );
        assert_eq!(
            p("pane send --pane 3 --no-enter ls -la").unwrap(),
            Some((
                "pane.send".into(),
                json!({"pane": 3, "enter": false, "text": "ls -la"})
            ))
        );
        assert_eq!(
            p("pane split --dir down --kind browser --url example.com").unwrap(),
            Some((
                "pane.split".into(),
                json!({"dir": "down", "kind": "browser", "url": "example.com"})
            ))
        );
        assert!(p("pane read --pane x").is_err());
        assert!(p("pane list extra").is_err());
        assert!(p("pane").is_err());
        assert_eq!(
            p("open http://localhost:3000").unwrap(),
            Some((
                "browser.open".into(),
                json!({"url": "http://localhost:3000"})
            ))
        );
        assert_eq!(
            p("https://github.com/login/device").unwrap(),
            Some((
                "browser.open".into(),
                json!({"url": "https://github.com/login/device"})
            ))
        );
        assert_eq!(
            p("tab new --chat opencode").unwrap(),
            Some(("tab.new".into(), json!({"chat": "opencode"})))
        );
        assert_eq!(
            p("tab new --agent claude").unwrap(),
            Some(("tab.new".into(), json!({"agent": "claude"})))
        );
        assert_eq!(
            p("task new --agent codex fix the flaky test").unwrap(),
            Some((
                "task.new".into(),
                json!({"agent": "codex", "text": "fix the flaky test"})
            ))
        );
        assert_eq!(
            p("run --name dev --cwd web npm run dev -- --port 3001").unwrap(),
            Some((
                "pane.run".into(),
                json!({"name": "dev", "cwd": "web", "cmd": "npm run dev -- --port 3001"})
            ))
        );
        assert!(p("run --name dev").is_err());
        assert_eq!(
            p("pane read --pane 2 --lines 50").unwrap(),
            Some(("pane.read".into(), json!({"pane": 2, "lines": 50})))
        );
        assert_eq!(
            p("task merge --task add-x-1f").unwrap(),
            Some(("task.merge".into(), json!({"task": "add-x-1f"})))
        );
    }

    #[test]
    fn browser_verbs() {
        assert_eq!(
            p("open --name app --tab localhost:3000").unwrap(),
            Some((
                "browser.open".into(),
                json!({"name": "app", "tab": true, "url": "localhost:3000"})
            ))
        );
        assert_eq!(
            p("browser open --name app --tab localhost:3000").unwrap(),
            Some((
                "browser.open".into(),
                json!({"name": "app", "tab": true, "url": "localhost:3000"})
            ))
        );
        assert_eq!(
            p("tab new --kind browser --name app --url localhost:3000").unwrap(),
            Some((
                "tab.new".into(),
                json!({"kind": "browser", "name": "app", "url": "localhost:3000"})
            ))
        );
        assert_eq!(
            p("browser snapshot --name app").unwrap(),
            Some(("browse.snapshot".into(), json!({"name": "app"})))
        );
        assert_eq!(
            p("browser click --pane 4 text=Sign in").unwrap(),
            Some(("browse.click".into(), json!({"pane": 4, "target": "text=Sign in"})))
        );
        assert_eq!(
            p("browser click 12 --dismiss").unwrap(),
            Some(("browse.click".into(), json!({"target": "12", "dismiss": true})))
        );
        assert_eq!(
            p("browser type input[name=q] hello world --enter").unwrap(),
            Some((
                "browse.type".into(),
                json!({"target": "input[name=q]", "text": "hello world", "enter": true})
            ))
        );
        assert_eq!(
            p("browser type 3 more --append").unwrap(),
            Some(("browse.type".into(), json!({"target": "3", "text": "more", "append": true})))
        );
        assert!(p("browser type 3").is_err());
        assert_eq!(
            p("browser key Shift+Tab").unwrap(),
            Some(("browse.key".into(), json!({"key": "Shift+Tab"})))
        );
        assert_eq!(
            p("browser scroll bottom").unwrap(),
            Some(("browse.scroll".into(), json!({"to": "bottom"})))
        );
        assert_eq!(
            p("browser scroll").unwrap(),
            Some(("browse.scroll".into(), json!({})))
        );
        assert_eq!(p("browser back").unwrap(), Some(("browse.back".into(), json!({}))));
        assert!(p("browser reload now").is_err());
        assert_eq!(
            p("browser wait --for Welcome|Dashboard --timeout 10").unwrap(),
            Some(("browse.wait".into(), json!({"for": "Welcome|Dashboard", "timeout": 10})))
        );
        assert_eq!(
            p("browser wait --selector .spinner --gone").unwrap(),
            Some(("browse.wait".into(), json!({"selector": ".spinner", "gone": true})))
        );
        assert!(p("browser wait").is_err());
        assert!(p("browser wait --for x --timeout soon").is_err());
        assert_eq!(
            p("browser console --since 40 --limit 20 --filter api").unwrap(),
            Some((
                "browser.logs".into(),
                json!({"kind": "console", "since": 40, "limit": 20, "filter": "api"})
            ))
        );
        assert_eq!(
            p("browser network --failed --clear").unwrap(),
            Some(("browser.logs".into(), json!({"kind": "network", "failed": true, "clear": true})))
        );
        assert_eq!(
            p("browser errors").unwrap(),
            Some(("browser.logs".into(), json!({"kind": "errors"})))
        );
        assert_eq!(
            p("browser logs --pane 2").unwrap(),
            Some(("browser.logs".into(), json!({"kind": "all", "pane": 2})))
        );
        assert_eq!(
            p("browser shot --pane 2 page.png").unwrap(),
            Some(("browser.shot".into(), json!({"pane": 2, "out": "page.png"})))
        );
        assert_eq!(
            p("browser eval --pane 2 document.title").unwrap(),
            Some(("browser.eval".into(), json!({"pane": 2, "js": "document.title"})))
        );
        assert_eq!(
            p("browser dialog").unwrap(),
            Some(("browser.dialog".into(), json!({})))
        );
        assert_eq!(
            p("browser dialog accept my answer").unwrap(),
            Some(("browser.dialog".into(), json!({"action": "accept", "text": "my answer"})))
        );
        assert_eq!(
            p("browser dialog accept --user me --password pw").unwrap(),
            Some((
                "browser.dialog".into(),
                json!({"action": "accept", "user": "me", "password": "pw"})
            ))
        );
        assert!(p("browser hover 3").is_err());
    }
}
