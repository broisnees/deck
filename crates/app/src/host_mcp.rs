//! `deck mcp --host HOST --dir DIR`: an MCP server bound to one machine, for
//! a Deck chat whose agent works on that host while it runs on this Mac.
//!
//! The agent's own shell and file tools are turned off for such a chat
//! (`SessionCtx::host_only`), so these are how it acts: run a
//! command, read, write and edit files, find files and search them, all in
//! `DIR` on `HOST`. Nothing is installed there and no login leaves the Mac;
//! every call is one `ssh` over the shared connection `remote::ssh` keeps.

use crate::remote::{q, run_on};
use serde_json::{json, Value};

const READ_LIMIT: u64 = 2000;
const LIST_LIMIT: usize = 500;

/// (name, what it does, input properties, required)
type Tool = (&'static str, &'static str, fn() -> Value, &'static [&'static str]);

const TOOLS: &[Tool] = &[
    (
        "run",
        "Run a shell command on the host, in the working folder, and get its exit code, stdout and stderr. Non-interactive (no TTY, no prompts). Default timeout 120 s, at most 1800; the host kills it too, so nothing is left running. For work that takes longer, start it detached (nohup … &) and read its log.",
        || json!({"command": {"type": "string"}, "timeout": {"type": "integer", "description": "seconds"}}),
        &["command"],
    ),
    (
        "read",
        "Read a text file on the host, with line numbers (cat -n style). Relative paths are from the working folder. Reads 2000 lines from `offset` (1-based) unless `limit` says otherwise.",
        || json!({"path": {"type": "string"}, "offset": {"type": "integer"}, "limit": {"type": "integer"}}),
        &["path"],
    ),
    (
        "write",
        "Create or replace a text file on the host with `content`; missing folders are made. Keeps the file's permissions when it exists.",
        || json!({"path": {"type": "string"}, "content": {"type": "string"}}),
        &["path", "content"],
    ),
    (
        "edit",
        "Replace `old_string` with `new_string` in a file on the host. `old_string` must occur exactly once unless `replace_all` is true. Read the file first so the text matches exactly, indentation included.",
        || {
            json!({
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            })
        },
        &["path", "old_string", "new_string"],
    ),
    (
        "glob",
        "Find files on the host by a glob pattern such as **/*.rs or src/*.ts, from the working folder (or `path`). At most 500 paths.",
        || json!({"pattern": {"type": "string"}, "path": {"type": "string"}}),
        &["pattern"],
    ),
    (
        "grep",
        "Search file contents on the host for a regular expression, from the working folder (or `path`), with file names and line numbers; .git is skipped. `glob` narrows the files (e.g. *.py). At most 500 lines.",
        || {
            json!({
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "ignore_case": {"type": "boolean"}
            })
        },
        &["pattern"],
    ),
];

pub fn run(host: &str, dir: &str) -> i32 {
    let (host, dir) = (host.to_string(), dir.to_string());
    let instructions = format!(
        "You are working on the machine `{host}`, in the folder `{dir}` there, over SSH. These tools are the only way to act on it: `run` for commands, `read`, `write` and `edit` for files, `glob` and `grep` to find them. Paths are relative to `{dir}` unless absolute."
    );
    crate::mcp::serve(|method, params| match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "deck-host", "version": crate::updates::VERSION},
            "instructions": instructions,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": TOOLS.iter().map(|(name, about, props, req)| json!({
            "name": name,
            "description": about,
            "inputSchema": {"type": "object", "properties": props(), "required": req},
        })).collect::<Vec<_>>() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match call(&host, &dir, name, &args) {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            })
        }
        _ => Err((-32601, format!("no method {method}"))),
    })
}

/// `cd` into the working folder: `~` and `~/x` left for the host to expand.
fn cd(dir: &str) -> String {
    let d = match dir.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) => format!("\"$HOME\"{}", q(rest)),
        None => q(dir),
    };
    format!("cd {d}")
}

fn call(host: &str, dir: &str, name: &str, a: &Value) -> Result<String, String> {
    let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
    let need = |k: &str| s(k).ok_or(format!("`{k}` is required"));
    let n = |k: &str| a.get(k).and_then(Value::as_u64);
    let b = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
    let cd = cd(dir);
    match name {
        "run" => {
            let command = need("command")?;
            let t = n("timeout").unwrap_or(120).clamp(1, 1800);
            // Killed on the host as well, so a timeout leaves nothing behind.
            let line = format!(
                "{cd} && if command -v timeout >/dev/null 2>&1; then exec timeout -k 5 {t} \"$SHELL\" -c {c}; else exec \"$SHELL\" -c {c}; fi",
                c = q(&command)
            );
            let (code, out, err) = run_on(host, &line, None, t + 15)?;
            let mut text = match code {
                Some(124) => format!("exit 124 (timed out after {t} s)\n"),
                Some(c) => format!("exit {c}\n"),
                None => "killed by a signal\n".to_string(),
            };
            if !out.is_empty() {
                text += &format!("--- stdout\n{}", clip(&out));
            }
            if !err.is_empty() {
                text += &format!("\n--- stderr\n{}", clip(&err));
            }
            Ok(text)
        }
        "read" => {
            let path = need("path")?;
            let from = n("offset").unwrap_or(1).max(1);
            let limit = n("limit").unwrap_or(READ_LIMIT).clamp(1, 20_000);
            let line = format!(
                "{cd} && test -f {p} || {{ echo \"no file {p}\" >&2; exit 2; }}; awk -v a={from} -v b={end} 'NR>=a && NR<b {{ printf \"%6d\\t%s\\n\", NR, $0 }} NR>=b {{ exit }}' {p}",
                p = q(&path),
                end = from + limit
            );
            let (code, out, err) = run_on(host, &line, None, 60)?;
            match code {
                Some(0) if out.is_empty() => Ok(format!("{path}: empty, or shorter than line {from}")),
                Some(0) => Ok(clip(&out)),
                _ => Err(err.trim().to_string()),
            }
        }
        "write" => {
            let path = need("path")?;
            let content = s("content").unwrap_or_default();
            write(host, &cd, &path, &content)?;
            Ok(format!("wrote {path} ({} bytes)", content.len()))
        }
        "edit" => {
            let path = need("path")?;
            let old = need("old_string")?;
            let new = s("new_string").unwrap_or_default();
            if old.is_empty() {
                return Err("`old_string` is empty; use write to create a file".into());
            }
            let (code, text, err) = run_on(host, &format!("{cd} && cat -- {}", q(&path)), None, 60)?;
            if code != Some(0) {
                return Err(err.trim().to_string());
            }
            let count = text.matches(&old).count();
            let changed = match count {
                0 => return Err(format!("`old_string` was not found in {path}")),
                1 => text.replacen(&old, &new, 1),
                _ if b("replace_all") => text.replace(&old, &new),
                k => return Err(format!("`old_string` occurs {k} times in {path}; give more context or set replace_all")),
            };
            write(host, &cd, &path, &changed)?;
            Ok(format!("edited {path} ({} replacement{})", if b("replace_all") { count } else { 1 }, if b("replace_all") && count != 1 { "s" } else { "" }))
        }
        "glob" => {
            let pattern = need("pattern")?;
            let base = s("path").map(|p| format!(" && cd {}", q(&p))).unwrap_or_default();
            // The pattern unquoted on purpose: bash expands it.
            let line = format!(
                "{cd}{base} && exec bash -c {}",
                q(&format!("shopt -s globstar nullglob dotglob; for f in {pattern}; do printf '%s\\n' \"$f\"; done | head -n {}", LIST_LIMIT + 1))
            );
            let (code, out, err) = run_on(host, &line, None, 60)?;
            if code != Some(0) {
                return Err(err.trim().to_string());
            }
            Ok(listing(&out, "no files match"))
        }
        "grep" => {
            let pattern = need("pattern")?;
            let path = s("path").unwrap_or_else(|| ".".into());
            let (rg_i, gr_i) = if b("ignore_case") { (" -i", "i") } else { ("", "") };
            let (rg_g, gr_g) = match s("glob") {
                Some(g) => (format!(" -g {}", q(&g)), format!(" --include={}", q(&g))),
                None => (String::new(), String::new()),
            };
            let line = format!(
                "{cd} && if command -v rg >/dev/null 2>&1; then rg -n --no-heading --color never{rg_i}{rg_g} -e {p} -- {d}; else grep -rnE{gr_i} --exclude-dir=.git{gr_g} -e {p} -- {d}; fi | head -n {}",
                LIST_LIMIT + 1,
                p = q(&pattern),
                d = q(&path)
            );
            let (_, out, err) = run_on(host, &line, None, 120)?;
            if out.is_empty() && !err.trim().is_empty() {
                return Err(err.trim().to_string());
            }
            Ok(listing(&out, "no matches"))
        }
        other => Err(format!("no tool {other}")),
    }
}

/// Write `content` to `path` there, making its folder; an existing file is
/// rewritten in place, so its permissions stay.
fn write(host: &str, cd: &str, path: &str, content: &str) -> Result<(), String> {
    let p = q(path);
    let line = format!("{cd} && mkdir -p -- \"$(dirname -- {p})\" && cat > {p}");
    let (code, _, err) = run_on(host, &line, Some(content.as_bytes()), 60)?;
    if code == Some(0) {
        Ok(())
    } else {
        Err(format!("could not write {path}: {}", err.trim()))
    }
}

fn listing(out: &str, none: &str) -> String {
    let lines: Vec<&str> = out.lines().collect();
    match lines.len() {
        0 => none.to_string(),
        n if n > LIST_LIMIT => format!("{}\n[more than {LIST_LIMIT}; narrow the search]", lines[..LIST_LIMIT].join("\n")),
        _ => lines.join("\n"),
    }
}

/// Output for an agent: its last 30,000 characters when longer.
fn clip(s: &str) -> String {
    let n = s.chars().count();
    if n <= 30_000 {
        s.to_string()
    } else {
        format!("[… {} characters cut]\n{}", n - 30_000, s.chars().skip(n - 30_000).collect::<String>())
    }
}

/// The MCP server a chat on `host` is given, with the chat's Deck env.
pub fn server_for(host: &str, dir: &str, env: &[(String, String)]) -> Option<agent_core::McpServerSpec> {
    let exe = std::env::current_exe().ok()?;
    Some(agent_core::McpServerSpec {
        name: "host".into(),
        transport: agent_core::McpTransport::Stdio {
            command: exe.to_string_lossy().into_owned(),
            args: vec!["mcp".into(), "--host".into(), host.into(), "--dir".into(), dir.into()],
            env: env.iter().filter(|(k, _)| k.starts_with("DECK_")).cloned().collect(),
        },
        enabled: true,
    })
}

/// The agents that can work on a host from this Mac: each can take the
/// `host` server and turn off its own tools that act here (Claude by name,
/// OpenCode by permission, Codex by feature flag and a read-only sandbox).
pub fn bridged(agent: &str) -> bool {
    matches!(agent, "claude" | "opencode" | "codex")
}

/// Claude's own tools that act on this Mac; off in a chat on a host.
pub const LOCAL_TOOLS: &[&str] = &[
    "Bash", "BashOutput", "KillShell", "Read", "Write", "Edit", "MultiEdit", "NotebookEdit", "Glob", "Grep", "LS", "LSP",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_expand_home_there() {
        assert_eq!(cd("~"), "cd \"$HOME\"");
        assert_eq!(cd("~/src/app"), "cd \"$HOME\"/src/app");
        assert_eq!(cd("/srv/my app"), "cd '/srv/my app'");
    }

    #[test]
    fn long_listings_say_so() {
        let many: String = (0..600).map(|i| format!("f{i}\n")).collect();
        let l = listing(&many, "none");
        assert_eq!(l.lines().count(), LIST_LIMIT + 1);
        assert!(l.ends_with("narrow the search]"));
        assert_eq!(listing("", "none"), "none");
    }
}
