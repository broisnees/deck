//! Deck memory: short notes every coding agent shares (facts about a
//! project, how the user likes things done), kept in one file Deck owns.
//!
//! Agents save and look them up with `deck memory …`. Claude Code is handed
//! them at the start of every session by a SessionStart hook Deck installs
//! (`deck hook session-start`), with a word on working inside Deck; Codex,
//! Gemini CLI and OpenCode are told in their global instructions to run
//! `deck memory list` first. Both are put back at every Deck launch.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub text: String,
    /// The project root it is about; `None` holds everywhere.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Unix seconds.
    pub created: i64,
    /// What sort of thing it is: `preference`, `decision`, `gotcha` or
    /// `fact`. Older notes have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Unix seconds it was last written or confirmed (added again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<i64>,
}

/// The kinds a memory can be, in the order a session reads them: how to
/// work first, then what bites, then why things are as they are.
pub const KINDS: [&str; 4] = ["preference", "gotcha", "decision", "fact"];

impl Memory {
    fn rank(&self) -> usize {
        self.kind.as_deref().and_then(|k| KINDS.iter().position(|x| *x == k)).unwrap_or(KINDS.len())
    }

    fn touched(&self) -> i64 {
        self.updated.unwrap_or(self.created)
    }
}

/// Text as compared for sameness: lowercase, words only.
fn norm(s: &str) -> String {
    s.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

fn file() -> PathBuf {
    crate::control::socket_dir().join("memory.json")
}

pub fn load() -> Vec<Memory> {
    std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(all: &[Memory]) -> std::io::Result<()> {
    let path = file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(all)?)?;
    std::fs::rename(tmp, path)
}

/// The memories for work in `cwd`: its project's, then the global ones.
pub fn relevant(cwd: &Path) -> Vec<Memory> {
    let root = crate::inspect::project_root(cwd);
    let mut out: Vec<Memory> = load()
        .into_iter()
        .filter(|m| m.project.as_ref().is_none_or(|p| *p == root))
        .collect();
    // This project's before the global ones; within each, how to work
    // before what happened, newest first.
    out.sort_by_key(|m| (m.project.is_none(), m.rank(), std::cmp::Reverse(m.touched())));
    out
}

/// Keep `text` for `project` (or everywhere). Saying the same thing again
/// confirms the note already kept instead of adding a twin.
pub fn add(text: &str, project: Option<PathBuf>, kind: Option<&str>) -> std::io::Result<Memory> {
    let mut all = load();
    let created = crate::local::now();
    let key = norm(text);
    if let Some(m) = all.iter_mut().find(|m| m.project == project && norm(&m.text) == key) {
        m.updated = Some(created);
        if kind.is_some() {
            m.kind = kind.map(str::to_string);
        }
        let m = m.clone();
        save(&all)?;
        return Ok(m);
    }
    let m = Memory {
        id: format!("{:x}", (created as u64).wrapping_mul(2654435761) ^ all.len() as u64)
            .chars()
            .rev()
            .take(6)
            .collect(),
        text: text.trim().to_string(),
        project,
        created,
        kind: kind.map(str::to_string),
        updated: None,
    };
    all.push(m.clone());
    save(&all)?;
    Ok(m)
}

/// Rewrite memory `id` (a prefix works) with `text`.
pub fn edit(id: &str, text: &str) -> std::io::Result<Option<Memory>> {
    let mut all = load();
    let Some(m) = all.iter_mut().find(|m| !id.is_empty() && m.id.starts_with(id)) else {
        return Ok(None);
    };
    m.text = text.trim().to_string();
    m.updated = Some(crate::local::now());
    let m = m.clone();
    save(&all)?;
    Ok(Some(m))
}

pub fn forget(id: &str) -> std::io::Result<bool> {
    let mut all = load();
    let before = all.len();
    all.retain(|m| !m.id.starts_with(id) || id.is_empty());
    let gone = all.len() != before;
    if gone {
        save(&all)?;
    }
    Ok(gone)
}

const HELP: &str = "\
deck memory: notes every coding agent shares, per project or everywhere.

  deck memory add [--global] [--kind K] TEXT
                                     keep TEXT for this project (or everywhere); K is
                                     preference, gotcha, decision or fact
  deck memory edit ID TEXT           rewrite one
  deck memory list [--all]           this project's and the global ones (--all: every project)
  deck memory search QUERY           any memory containing every word of QUERY
  deck memory forget ID              remove one (an id prefix works)

Save what a later session should know and can't read from the code: the
user's preferences, decisions and why, where things are deployed, gotchas.
";

pub fn cli(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let print = |v: Value| {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        0
    };
    let fail = |e: String| {
        eprintln!("deck: {e}");
        1
    };
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("add") => {
            let global = rest.iter().any(|a| a == "--global");
            let kind = rest.windows(2).find(|w| w[0] == "--kind").map(|w| w[1].to_lowercase());
            if let Some(k) = kind.as_deref().filter(|k| !KINDS.contains(k)) {
                return fail(format!("no kind {k:?}: one of {}", KINDS.join(", ")));
            }
            let mut words = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--global" => {}
                    "--kind" => {
                        it.next();
                    }
                    w => words.push(w),
                }
            }
            let text = words.join(" ");
            if text.trim().is_empty() {
                return fail("memory add needs the text to keep".into());
            }
            let project = (!global).then(|| crate::inspect::project_root(&cwd));
            match add(&text, project, kind.as_deref()) {
                Ok(m) => print(json!({ "saved": m })),
                Err(e) => fail(e.to_string()),
            }
        }
        Some("list") => {
            let all = rest.iter().any(|a| a == "--all");
            let list = if all { load() } else { relevant(&cwd) };
            print(json!({ "memories": list }))
        }
        Some("search") => {
            let words: Vec<String> = rest.iter().map(|w| w.to_lowercase()).collect();
            let hits: Vec<Memory> = load()
                .into_iter()
                .filter(|m| {
                    let t = m.text.to_lowercase();
                    words.iter().all(|w| t.contains(w.as_str()))
                })
                .collect();
            print(json!({ "memories": hits }))
        }
        Some("edit") => match rest.split_first() {
            Some((id, words)) if !words.is_empty() => match edit(id, &words.join(" ")) {
                Ok(Some(m)) => print(json!({ "saved": m })),
                Ok(None) => fail(format!("no memory {id:?}")),
                Err(e) => fail(e.to_string()),
            },
            _ => fail("memory edit needs an id and the new text".into()),
        },
        Some("forget") => match rest.first() {
            Some(id) => match forget(id) {
                Ok(true) => print(json!({ "forgot": id })),
                Ok(false) => fail(format!("no memory {id:?}")),
                Err(e) => fail(e.to_string()),
            },
            None => fail("memory forget needs an id (see `deck memory list`)".into()),
        },
        _ => {
            print!("{HELP}");
            0
        }
    }
}

/// Deck memory as MCP tools (`deck mcp`), for the folder the server runs
/// in: the same as `deck memory list|add|edit|forget`.
pub fn tool(rpc: &str, args: &Value) -> Result<Value, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    match rpc {
        "memory.list" => {
            let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
            let mut list = if all { load() } else { relevant(&cwd) };
            if let Some(q) = s("query").filter(|q| !q.trim().is_empty()) {
                let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
                list.retain(|m| {
                    let t = m.text.to_lowercase();
                    words.iter().all(|w| t.contains(w.as_str()))
                });
            }
            Ok(json!({ "memories": list }))
        }
        "memory.add" => {
            let text = s("text").filter(|t| !t.trim().is_empty()).ok_or("text is the note to keep")?;
            let kind = s("kind");
            if let Some(k) = kind.as_deref().filter(|k| !KINDS.contains(k)) {
                return Err(format!("no kind {k:?}: one of {}", KINDS.join(", ")));
            }
            let global = args.get("global").and_then(Value::as_bool).unwrap_or(false);
            let project = (!global).then(|| crate::inspect::project_root(&cwd));
            add(&text, project, kind.as_deref()).map(|m| json!({ "saved": m })).map_err(|e| e.to_string())
        }
        "memory.edit" => {
            let id = s("id").ok_or("id is required")?;
            let text = s("text").ok_or("text is required")?;
            match edit(&id, &text).map_err(|e| e.to_string())? {
                Some(m) => Ok(json!({ "saved": m })),
                None => Err(format!("no memory {id:?}")),
            }
        }
        "memory.forget" => {
            let id = s("id").ok_or("id is required")?;
            match forget(&id).map_err(|e| e.to_string())? {
                true => Ok(json!({ "forgot": id })),
                false => Err(format!("no memory {id:?}")),
            }
        }
        _ => Err(format!("no memory tool {rpc}")),
    }
}

/// `deck hook session-start`: what Claude Code adds to a new session's
/// context. Its hook input (JSON on stdin) names the folder.
pub fn session_start_hook() -> i32 {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let cwd = serde_json::from_str::<Value>(&input)
        .ok()
        .and_then(|v| v["cwd"].as_str().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    print!("{}", brief(&cwd, std::env::var_os("DECK_SOCKET").is_some()));
    0
}

/// The text a session starts with: working in Deck (when it is), and the
/// memories for this folder.
pub fn brief(cwd: &Path, in_deck: bool) -> String {
    let mut out = String::new();
    if in_deck {
        if let Ok(pane) = std::env::var("DECK_PANE") {
            out.push_str(&format!("(Deck pane {pane}.) "));
        }
        out.push_str(
            "# Deck\nYou are running inside Deck. Use the `deck` CLI (see the deck skill): `deck job start --name dev CMD` for dev servers and long commands (they keep running and show in the status bar), `deck read URL` to read any web page as Markdown, `deck browser …` to drive a page, `deck usage` for plan limits.\n\n",
        );
    }
    let mems = relevant(cwd);
    out.push_str("# Deck memory\n");
    if mems.is_empty() {
        out.push_str("Nothing saved for this project yet.\n");
    } else {
        // Most useful first, within a budget: a long memory must not crowd
        // out the session's own work.
        let mut used = 0;
        let mut shown = 0;
        for m in &mems {
            let scope = if m.project.is_some() { "project" } else { "global" };
            let kind = m.kind.as_deref().map(|k| format!("{k}, ")).unwrap_or_default();
            let line = format!("- [{}] ({kind}{scope}) {}\n", m.id, m.text.replace('\n', " "));
            if used + line.len() > BRIEF_BUDGET && shown > 0 {
                break;
            }
            used += line.len();
            shown += 1;
            out.push_str(&line);
        }
        if shown < mems.len() {
            out.push_str(&format!("({} more: `deck memory list`, `deck memory search WORDS`)\n", mems.len() - shown));
        }
    }
    let recent = recent_work(cwd);
    if !recent.is_empty() {
        out.push_str("\n# Recent work here (Deck chats)\n");
        for line in recent {
            out.push_str(&format!("- {line}\n"));
        }
        out.push_str("Reopen one with `deck history` and `deck resume ID`.\n");
    }
    out.push_str("\nSave what a later session should know and can't read from the code with `deck memory add --kind preference|gotcha|decision|fact TEXT` (`--global` for every project). Fix one with `deck memory edit ID TEXT`, remove a stale one with `deck memory forget ID`.\n");
    out
}

/// How much of a session's opening context the memories may take.
const BRIEF_BUDGET: usize = 3500;

/// The last few Deck chats in `cwd`'s project: title and how long ago.
/// Only the newest thread files are read, so a session starts as fast with
/// a thousand old threads as with none.
fn recent_work(cwd: &Path) -> Vec<String> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        title: String,
        #[serde(default)]
        cwd: PathBuf,
        #[serde(default)]
        updated: i64,
        #[serde(default)]
        control: bool,
    }
    let root = crate::inspect::project_root(cwd);
    let Ok(dir) = std::fs::read_dir(crate::threads::dir()) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = dir
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let now = crate::local::now() * 1000;
    let mut out = Vec::new();
    for (_, path) in files.into_iter().take(25) {
        let Some(h) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Head>(&b).ok()) else { continue };
        if h.control || h.title.trim().is_empty() || crate::inspect::project_root(&h.cwd) != root {
            continue;
        }
        let mins = ((now - h.updated).max(0) / 60_000) as u64;
        let ago = match mins {
            0..=59 => format!("{mins}m ago"),
            60..=1439 => format!("{}h ago", mins / 60),
            _ => format!("{}d ago", mins / 1440),
        };
        out.push(format!("{} ({ago})", h.title.trim().chars().take(100).collect::<String>()));
        if out.len() == 5 {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Installing it into the agents

const MARK_START: &str = "<!-- deck:memory start (managed by Deck; delete these markers to keep your own copy) -->";
const MARK_END: &str = "<!-- deck:memory end -->";
const HOOK_CMD: &str = "deck hook session-start";

const BLOCK: &str = "\
## Deck memory

At the start of a session, run `deck memory list` and take the notes it
prints into account. Save what a later session should know and can't read
from the code (the user's preferences, decisions and why, gotchas) with
`deck memory add TEXT` (`--global` for every project). When inside Deck
(DECK_SOCKET is set), use the `deck` CLI for dev servers (`deck job start`),
web pages (`deck read URL`) and the browser.";

/// Put the memory hook and instructions in place (`on`), or take Deck's
/// copies out. Run at launch, off the main thread.
pub fn install(on: bool) {
    if on {
        install_mcp();
    }
    let Some(home) = agent_core::dirs::home_dir() else { return };
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
    if claude.is_dir() {
        if let Err(e) = claude_hook(&claude.join("settings.json"), on) {
            tracing::warn!("deck memory hook: {e}");
        }
    }
    for (dir, file) in [(".codex", "AGENTS.md"), (".gemini", "GEMINI.md"), (".config/opencode", "AGENTS.md")] {
        let d = home.join(dir);
        if d.is_dir() {
            let _ = instructions(&d.join(file), on);
        }
    }
}

/// The `deck` linked into `~/.local/bin` (`deck.exe` on Windows), when
/// there is one.
fn installed_deck() -> Option<PathBuf> {
    agent_core::dirs::home_dir()
        .map(|h| h.join(".local").join("bin").join(format!("deck{}", std::env::consts::EXE_SUFFIX)))
        .filter(|p| p.exists())
}

/// Register `deck mcp` (Deck's commands as MCP tools) with each agent
/// CLI, through its own `mcp add`, once: a marker under deck's data folder
/// remembers which have it, so no agent is asked twice.
pub fn install_mcp() {
    let bin = installed_deck().or_else(|| std::env::current_exe().ok());
    let Some(bin) = bin else { return };
    let bin = bin.to_string_lossy().into_owned();
    let dir = crate::control::socket_dir().join("mcp-registered");
    let _ = std::fs::create_dir_all(&dir);
    let plans: [(&str, Vec<&str>); 3] = [
        ("claude", vec!["mcp", "add", "--scope", "user", "deck", "--", &bin, "mcp"]),
        ("codex", vec!["mcp", "add", "deck", "--", &bin, "mcp"]),
        ("gemini", vec!["mcp", "add", "--scope", "user", "deck", &bin, "mcp"]),
    ];
    for (agent, args) in plans {
        let mark = dir.join(agent);
        if mark.exists() {
            continue;
        }
        if term::which(agent).is_none() {
            continue;
        }
        let (program, first) = agent_core::exec::launcher(agent);
        let ok = crate::os::no_window(&mut std::process::Command::new(program))
            .args(first)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .map(|o| {
                let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                // Already there counts too.
                o.status.success() || text.contains("already exists")
            })
            .unwrap_or(false);
        // Tried either way: a failure is not retried at every launch.
        let _ = std::fs::write(&mark, if ok { "ok" } else { "failed" });
    }
}

/// Add (or remove) Deck's SessionStart hook in Claude Code's settings,
/// leaving every other setting and hook as it is.
fn claude_hook(path: &Path, on: bool) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".into());
    let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
    let Some(obj) = v.as_object_mut() else { return Ok(()) };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else { return Ok(()) };
    let list = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    let Some(list) = list.as_array_mut() else { return Ok(()) };
    let ours = |g: &Value| {
        g["hooks"].as_array().is_some_and(|h| {
            h.iter().any(|x| x["command"].as_str().is_some_and(|c| c.contains("deck") && c.ends_with(&HOOK_CMD[4..])))
        })
    };
    let has = list.iter().any(ours);
    if on == has {
        return Ok(());
    }
    if on {
        // The full path: Claude Code's hooks run without a login shell's PATH.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let bin = installed_deck().unwrap_or(exe);
        let cmd = format!("{} hook session-start", shell_word(&bin.to_string_lossy()));
        list.push(json!({ "hooks": [{ "type": "command", "command": cmd, "timeout": 5 }] }));
    } else {
        list.retain(|g| !ours(g));
    }
    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.deck-tmp");
    std::fs::write(&tmp, out + "\n").map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn shell_word(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || "/._-".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Deck's block in an agent's global instructions file, between markers;
/// the rest of the file is the user's and stays as it is.
fn instructions(path: &Path, on: bool) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let stripped = match (text.find(MARK_START), text.find(MARK_END)) {
        (Some(a), Some(b)) if b > a => format!("{}{}", &text[..a], &text[b + MARK_END.len()..]).trim_end().to_string(),
        (None, None) => text.trim_end().to_string(),
        // Markers changed by hand: the file is the user's now.
        _ => return Ok(()),
    };
    let next = if on {
        let sep = if stripped.is_empty() { "" } else { "\n\n" };
        format!("{stripped}{sep}{MARK_START}\n{BLOCK}\n{MARK_END}\n")
    } else if stripped.is_empty() {
        String::new()
    } else {
        format!("{stripped}\n")
    };
    if next == text {
        return Ok(());
    }
    if next.is_empty() {
        return std::fs::remove_file(path);
    }
    std::fs::write(path, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(text: &str, kind: Option<&str>, project: bool, at: i64) -> Memory {
        Memory {
            id: text.into(),
            text: text.into(),
            project: project.then(|| PathBuf::from("/p")),
            created: at,
            kind: kind.map(str::to_string),
            updated: None,
        }
    }

    #[test]
    fn the_same_note_twice_is_one_note() {
        assert_eq!(norm("Use pnpm, not npm."), norm("use  PNPM not npm"));
        assert_ne!(norm("use pnpm"), norm("use npm"));
    }

    #[test]
    fn a_session_reads_how_to_work_first_and_its_project_before_the_rest() {
        let mut v = vec![
            mem("global fact", Some("fact"), false, 5),
            mem("old fact", Some("fact"), true, 1),
            mem("new fact", Some("fact"), true, 9),
            mem("pref", Some("preference"), true, 2),
            mem("untyped", None, true, 3),
        ];
        v.sort_by_key(|m| (m.project.is_none(), m.rank(), std::cmp::Reverse(m.touched())));
        let order: Vec<&str> = v.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(order, ["pref", "new fact", "old fact", "untyped", "global fact"]);
    }

    #[test]
    fn hook_goes_in_once_and_comes_out_leaving_the_rest() {
        let dir = std::env::temp_dir().join(format!("deck-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other"}]}]}}"#).unwrap();
        claude_hook(&path, true).unwrap();
        claude_hook(&path, true).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        claude_hook(&path, false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["SessionStart"][0]["hooks"][0]["command"], "other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn instructions_block_is_replaced_not_repeated() {
        let dir = std::env::temp_dir().join(format!("deck-inst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("AGENTS.md");
        std::fs::write(&path, "# Mine\nkeep this\n").unwrap();
        instructions(&path, true).unwrap();
        instructions(&path, true).unwrap();
        let t = std::fs::read_to_string(&path).unwrap();
        assert_eq!(t.matches(MARK_START).count(), 1);
        assert!(t.starts_with("# Mine\nkeep this"));
        instructions(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Mine\nkeep this\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
