//! `deck mcp`: deck's control socket as an MCP server on stdin/stdout, so
//! an agent gets deck's commands as tools of its own (open a terminal, read
//! one, open a page, hand out a task, see the window) instead of composing
//! `deck …` shell lines. Deck adds it to every chat's Claude session with
//! that chat's token; any other MCP client can run `deck mcp` from a deck
//! terminal.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// (tool, socket method, what it does, its input schema properties, required)
type Tool = (
    &'static str,
    &'static str,
    &'static str,
    fn() -> Value,
    &'static [&'static str],
);

const TOOLS: &[Tool] = &[
    (
        "deck_remote_hosts",
        "remote.hosts",
        "List the machines reachable over SSH (aliases from the user's SSH config and hosts added in Deck), with user@hostname.",
        || json!({}),
        &[],
    ),
    (
        "deck_remote_run",
        "remote.run",
        "Run a shell command on a remote host over SSH, in a folder there, and get its output and exit code. For builds, tests, git, inspecting a server. Non-interactive: no prompts, no TTY. Default timeout 120 s.",
        || {
            json!({
                "host": {"type": "string", "description": "an alias from deck_remote_hosts, or user@host"},
                "command": {"type": "string"},
                "dir": {"type": "string", "description": "folder on the host; ~ by default"},
                "timeout": {"type": "integer", "description": "seconds, at most 1800"}
            })
        },
        &["host", "command"],
    ),
    (
        "deck_remote_read",
        "remote.read",
        "Read a text file on a remote host (the first 256 KB unless max_bytes says otherwise).",
        || json!({"host": {"type": "string"}, "path": {"type": "string"}, "max_bytes": {"type": "integer"}}),
        &["host", "path"],
    ),
    (
        "deck_remote_write",
        "remote.write",
        "Write (replace) a text file on a remote host; its folder is created when missing.",
        || json!({"host": {"type": "string"}, "path": {"type": "string"}, "content": {"type": "string"}}),
        &["host", "path", "content"],
    ),
    (
        "deck_panes",
        "pane.list",
        "List deck's open panes (terminals, pages, chats) with their ids, titles, folders and what runs in them.",
        || json!({}),
        &[],
    ),
    (
        "deck_split",
        "pane.split",
        "Open a terminal or a page beside a pane (beside yours when no pane is given). With cmd, the terminal runs it (a dev server, tests).",
        || {
            json!({
                "dir": {"type": "string", "enum": ["right", "down"]},
                "kind": {"type": "string", "enum": ["terminal", "browser"]},
                "cmd": {"type": "string", "description": "command for the new terminal"},
                "url": {"type": "string", "description": "page for a new browser pane"},
                "pane": {"type": "integer", "description": "split beside this pane"}
            })
        },
        &[],
    ),
    (
        "deck_new_tab",
        "tab.new",
        "Open a new tab: a terminal (optionally running cmd), a page, a chat with an agent (chat: claude, codex…), or an agent's own TUI in a terminal (agent).",
        || {
            json!({
                "kind": {"type": "string", "enum": ["terminal", "browser", "chat"]},
                "chat": {"type": "string"}, "agent": {"type": "string"},
                "cmd": {"type": "string"}, "url": {"type": "string"}
            })
        },
        &[],
    ),
    (
        "deck_run",
        "pane.run",
        "Run a command in a terminal split beside you that the user can watch live: a dev server, a watcher, tests. Give it a name (dev, test) and running it again reuses that pane (Ctrl-C, then the command). Then deck_wait for it to be ready and deck_read with lines for its logs.",
        || {
            json!({
                "cmd": {"type": "string"},
                "name": {"type": "string", "description": "a short handle for the pane, e.g. dev"},
                "dir": {"type": "string", "enum": ["right", "down"]},
                "cwd": {"type": "string"}
            })
        },
        &["cmd"],
    ),
    (
        "deck_wait",
        "pane.wait",
        "Wait until a terminal pane prints some text (alternatives separated by |, e.g. \"ready|Local:|error\"). Returns the matching line, or an error with the pane's last lines on timeout.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "for": {"type": "string"},
                "timeout": {"type": "integer", "description": "seconds, default 120"}
            })
        },
        &["for"],
    ),
    (
        "deck_read",
        "pane.read",
        "Read what a pane shows: a terminal's screen and recent scrollback, a page's text, or a chat's messages. For a terminal, lines gives just its last N lines (logs).",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}, "lines": {"type": "integer"}}),
        &[],
    ),
    (
        "deck_send",
        "pane.send",
        "Type text into a terminal pane (Enter after it unless enter is false), or send it to a chat pane as a message.",
        || {
            json!({
                "pane": {"type": "integer"},
                "text": {"type": "string"},
                "enter": {"type": "boolean"}
            })
        },
        &["pane", "text"],
    ),
    (
        "deck_focus",
        "pane.focus",
        "Bring a pane to the front.",
        || json!({"pane": {"type": "integer"}}),
        &["pane"],
    ),
    (
        "deck_close",
        "pane.close",
        "Close a pane.",
        || json!({"pane": {"type": "integer"}}),
        &["pane"],
    ),
    (
        "deck_open",
        "browser.open",
        "Open a URL in deck's browser, beside you (or in the given browser pane). Give it a name (app, docs) and opening that name again navigates the same pane; the other browser tools then take the name. new_tab opens it in a tab of its own.",
        || {
            json!({
                "url": {"type": "string"},
                "pane": {"type": "integer"},
                "name": {"type": "string", "description": "a short handle for the page, e.g. app"},
                "new_tab": {"type": "boolean"}
            })
        },
        &["url"],
    ),
    (
        "deck_eval",
        "browser.eval",
        "Run JavaScript in a browser pane's page and get its result (document.title, the text of an element, whether a selector exists). Without pane or name it is the page in your tab.",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}, "js": {"type": "string"}}),
        &["js"],
    ),
    (
        "deck_browser_snapshot",
        "browse.snapshot",
        "Read a page the way you would use it: its title, headings, and its visible links, buttons and fields, each numbered [N] with its name, value and state. Use the numbers with deck_click and deck_type. Take a new one after the page changes.",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}}),
        &[],
    ),
    (
        "deck_click",
        "browse.click",
        "Click something on a page: a [N] from deck_browser_snapshot, text=Some text (the control that says it), or a CSS selector. Waits for a navigation it starts. Dialogs it raises are answered OK unless dismiss is set; they are in the result.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "target": {"type": "string", "description": "3, text=Sign in, or a CSS selector"},
                "dismiss": {"type": "boolean", "description": "cancel confirm/prompt dialogs instead of accepting them"}
            })
        },
        &["target"],
    ),
    (
        "deck_type",
        "browse.type",
        "Type into a field on a page (a [N] from deck_browser_snapshot, text=its label, or a CSS selector), replacing its text unless append. A <select> picks the option with that value or text. enter presses Enter after (submits a form).",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "target": {"type": "string"}, "text": {"type": "string"},
                "enter": {"type": "boolean"}, "append": {"type": "boolean"}
            })
        },
        &["target", "text"],
    ),
    (
        "deck_press_key",
        "browse.key",
        "Press a key in a page, sent to whatever has focus: Enter, Escape, Tab, Shift+Tab, ArrowDown, Backspace, PageDown, or one character.",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}, "key": {"type": "string"}}),
        &["key"],
    ),
    (
        "deck_scroll",
        "browse.scroll",
        "Scroll a page: up, down, top, bottom, or to a target ([N], text=…, or a CSS selector).",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}, "to": {"type": "string"}}),
        &[],
    ),
    (
        "deck_browser_nav",
        "browse.nav",
        "Go back, forward, or reload a page, and wait for it to load.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "action": {"type": "string", "enum": ["back", "forward", "reload"]}
            })
        },
        &["action"],
    ),
    (
        "deck_browser_wait",
        "browse.wait",
        "Wait until a page shows some text (alternatives separated by |, any case) or a CSS selector matches something visible; with gone, until it no longer does. Errors on timeout with the page's title and address.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "for": {"type": "string"}, "selector": {"type": "string"},
                "gone": {"type": "boolean"},
                "timeout": {"type": "integer", "description": "seconds, default 30"}
            })
        },
        &[],
    ),
    (
        "deck_console",
        "browse.console",
        "A page's console: its log lines, uncaught errors and rejections, dialogs and navigations, oldest first, numbered by seq. kind errors gives just errors (console.error, uncaught, failed or >= 400 requests). Pass the last_seq you got as since to see only what is new.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "kind": {"type": "string", "enum": ["console", "errors", "network", "all"]},
                "since": {"type": "integer"}, "limit": {"type": "integer"},
                "failed": {"type": "boolean"},
                "filter": {"type": "string", "description": "only entries whose url or text has this"},
                "clear": {"type": "boolean"}
            })
        },
        &[],
    ),
    (
        "deck_network",
        "browse.network",
        "A page's network requests: fetch and XHR calls with method, status, time, and request and response bodies (clipped to 2 KB), plus scripts, styles, images and the document. failed gives only failed or >= 400 requests. Pass last_seq back as since for what is new.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "since": {"type": "integer"}, "limit": {"type": "integer"},
                "failed": {"type": "boolean"}, "filter": {"type": "string"},
                "clear": {"type": "boolean"}
            })
        },
        &[],
    ),
    (
        "deck_browser_dialog",
        "browser.dialog",
        "The alert, confirm, prompt or login box a page has up (a page with one runs nothing until it is answered), or with action, its answer: accept (with text for a prompt, user and password for a login) or dismiss.",
        || {
            json!({
                "pane": {"type": "integer"}, "name": {"type": "string"},
                "action": {"type": "string", "enum": ["accept", "dismiss"]},
                "text": {"type": "string"}, "user": {"type": "string"}, "password": {"type": "string"}
            })
        },
        &[],
    ),
    (
        "deck_browser_shot",
        "browser.shot",
        "See one page as it is on screen now: an image of just that browser pane.",
        || json!({"pane": {"type": "integer"}, "name": {"type": "string"}}),
        &[],
    ),
    (
        "deck_task_new",
        "task.new",
        "Hand out a task: a sub-agent on its own branch and worktree of this project, working on the brief. You hear back when it finishes a turn.",
        || {
            json!({
                "text": {"type": "string", "description": "the brief"},
                "agent": {"type": "string"},
                "cwd": {"type": "string"}
            })
        },
        &["text"],
    ),
    (
        "deck_tasks",
        "task.list",
        "List open tasks: what each agent is doing and what it changed.",
        || json!({}),
        &[],
    ),
    (
        "deck_task_show",
        "task.show",
        "A task's status, changes and its agent's last reply.",
        || json!({"task": {"type": "string"}}),
        &["task"],
    ),
    (
        "deck_task_diff",
        "task.diff",
        "A task's changes as a diff against where it started.",
        || json!({"task": {"type": "string"}}),
        &["task"],
    ),
    (
        "deck_task_send",
        "task.send",
        "Send a task's agent a follow-up.",
        || json!({"task": {"type": "string"}, "text": {"type": "string"}}),
        &["task", "text"],
    ),
    (
        "deck_task_merge",
        "task.merge",
        "Squash a task's branch into its base branch.",
        || json!({"task": {"type": "string"}}),
        &["task"],
    ),
    (
        "deck_task_discard",
        "task.discard",
        "Throw a task's branch and worktree away.",
        || json!({"task": {"type": "string"}}),
        &["task"],
    ),
    (
        "deck_screenshot",
        "window.shot",
        "See deck's window as it is now: an image of everything on screen in it, pages included.",
        || json!({}),
        &[],
    ),
];

pub fn run() -> i32 {
    serve(handle)
}

/// Serve MCP on stdin/stdout with `handle` answering each request.
pub fn serve(handle: impl Fn(&str, &Value) -> Result<Value, (i64, String)>) -> i32 {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let _ = reply(&mut out, Value::Null, Err((-32700, "parse error".into())));
            continue;
        };
        // Notifications (no id) get no answer.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let answer = handle(method, &params);
        if reply(&mut out, id, answer).is_err() {
            break;
        }
    }
    0
}

fn reply(out: &mut impl Write, id: Value, r: Result<Value, (i64, String)>) -> std::io::Result<()> {
    let msg = match r {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    writeln!(out, "{msg}")?;
    out.flush()
}

fn handle(method: &str, params: &Value) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "deck", "version": crate::updates::VERSION},
            "instructions": "Tools that drive the deck window this agent runs in: terminals, pages, chats and sub-agent tasks. Pane ids come from deck_panes. To work in a web page: deck_open it (with a name), deck_browser_snapshot to read it, deck_click and deck_type with the snapshot's [N] numbers, deck_browser_wait for what should appear, and deck_console and deck_network for what it logged, threw and fetched."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(
            json!({ "tools": TOOLS.iter().map(|(name, _, about, props, req)| json!({
            "name": name,
            "description": about,
            "inputSchema": {"type": "object", "properties": props(), "required": req},
        })).collect::<Vec<_>>() }),
        ),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let Some((_, rpc, ..)) = TOOLS.iter().find(|t| t.0 == name) else {
                return Err((-32602, format!("no tool {name}")));
            };
            Ok(call(rpc, args))
        }
        _ => Err((-32601, format!("no method {method}"))),
    }
}

/// Run one tool through the control socket; a failure is the tool's
/// answer (`isError`), not a protocol error, so the agent can read it.
fn call(rpc: &str, args: Value) -> Value {
    if rpc.starts_with("remote.") {
        return match crate::remote::tool(rpc, &args) {
            Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
            Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
        };
    }
    if rpc == "window.shot" || rpc == "browser.shot" {
        return screenshot(rpc, args);
    }
    if let Some(verb) = rpc.strip_prefix("browse.") {
        let r = match verb {
            "console" | "network" => {
                let mut a = args;
                if verb == "network" || a.get("kind").is_none() {
                    a["kind"] = verb.into();
                }
                crate::control::call("browser.logs", a)
            }
            "nav" => match args.get("action").and_then(Value::as_str) {
                Some(a @ ("back" | "forward" | "reload")) => {
                    let a = a.to_string();
                    crate::browse::run(&a, args)
                }
                _ => Err("action is back, forward or reload".into()),
            },
            v => crate::browse::run(v, args),
        };
        return match r {
            Ok(v) => json!({"content": [{"type": "text", "text": pretty(&v)}]}),
            Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
        };
    }
    if rpc == "pane.wait" {
        let want = args.get("for").and_then(Value::as_str).unwrap_or_default().to_string();
        let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(120).min(1800);
        let mut target = json!({});
        for k in ["pane", "name"] {
            if let Some(v) = args.get(k) {
                target[k] = v.clone();
            }
        }
        return match crate::cli::wait_text(target, &want, timeout) {
            Ok(v) => json!({"content": [{"type": "text", "text": pretty(&v)}]}),
            Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
        };
    }
    match crate::control::call(rpc, args) {
        Ok(v) => json!({"content": [{"type": "text", "text": pretty(&v)}]}),
        Err(e) => json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// The window (`window.shot`) or one page (`browser.shot`) as an image,
/// scaled to at most 1600 px so it costs the agent a sensible number of
/// tokens.
fn screenshot(rpc: &str, mut args: Value) -> Value {
    let path = std::env::temp_dir().join(format!("deck-mcp-shot-{}.png", std::process::id()));
    if !args.is_object() {
        args = json!({});
    }
    args["out"] = json!(path);
    let shot = crate::control::call(rpc, args);
    if let Err(e) = shot {
        return json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true});
    }
    let _ = std::process::Command::new("/usr/bin/sips")
        .args(["-Z", "1600"])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let bytes = std::fs::read(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    json!({"content": [{"type": "image", "data": base64(&bytes), "mimeType": "image/png"}]})
}

fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What a chat's agent session is given: `deck mcp` with that chat's socket
/// and token, so its tools act for that chat.
pub fn server_for(env: &[(String, String)]) -> Option<agent_core::McpServerSpec> {
    let exe = std::env::current_exe().ok()?;
    let env: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| k.starts_with("DECK_"))
        .cloned()
        .collect();
    Some(agent_core::McpServerSpec {
        name: "deck".into(),
        transport: agent_core::McpTransport::Stdio {
            command: exe.to_string_lossy().into_owned(),
            args: vec!["mcp".into()],
            env,
        },
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_mcp() {
        let init = handle("initialize", &json!({"protocolVersion": "2025-03-26"})).unwrap();
        assert_eq!(init["protocolVersion"], "2025-03-26");
        assert_eq!(init["serverInfo"]["name"], "deck");
        let list = handle("tools/list", &Value::Null).unwrap();
        let tools = list["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOLS.len());
        let read = tools.iter().find(|t| t["name"] == "deck_read").unwrap();
        assert_eq!(read["inputSchema"]["required"], json!([]));
        let run = tools.iter().find(|t| t["name"] == "deck_run").unwrap();
        assert_eq!(run["inputSchema"]["required"], json!(["cmd"]));
        for name in [
            "deck_browser_snapshot",
            "deck_click",
            "deck_type",
            "deck_press_key",
            "deck_browser_wait",
            "deck_console",
            "deck_network",
            "deck_browser_shot",
        ] {
            let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name}"));
            // Every page tool can name its pane either way.
            assert_eq!(t["inputSchema"]["properties"]["pane"]["type"], "integer", "{name}");
            assert_eq!(t["inputSchema"]["properties"]["name"]["type"], "string", "{name}");
        }
        let click = tools.iter().find(|t| t["name"] == "deck_click").unwrap();
        assert_eq!(click["inputSchema"]["required"], json!(["target"]));
        let open = tools.iter().find(|t| t["name"] == "deck_open").unwrap();
        assert_eq!(open["inputSchema"]["properties"]["new_tab"]["type"], "boolean");
        assert_eq!(open["inputSchema"]["properties"]["name"]["type"], "string");
        // Every required field is one the tool describes.
        for t in tools {
            for r in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(t["inputSchema"]["properties"].get(r.as_str().unwrap()).is_some(), "{}", t["name"]);
            }
        }
        let names: std::collections::HashSet<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), tools.len());
        // A bad action is the tool's answer, not a protocol error.
        let nav = call("browse.nav", json!({"action": "sideways"}));
        assert_eq!(nav["isError"], true);
        assert!(handle("tools/call", &json!({"name": "nope"})).is_err());
        assert_eq!(handle("bogus", &Value::Null).unwrap_err().0, -32601);
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
