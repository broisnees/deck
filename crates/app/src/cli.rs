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
        return crate::mcp::run();
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
        Ok(Some((method, params))) => match call(&method, params) {
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
        Some(verb) => crate::browse::run(verb, params),
        None => control::call(method, params),
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
    const FLAGS: &[&str] = &["enter", "append", "failed", "clear", "tab", "gone", "dismiss"];
    const NUMBERS: &[&str] = &["pane", "since", "limit", "timeout"];
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
