//! The window's layout, kept across launches: which tabs were open and how
//! each was split, what every pane was (a shell and its folder, an agent and
//! its conversation, a page, a chat thread, Settings), which one had the
//! keyboard, and whether the sidebar and inspector were showing. Written
//! when it changes, read once at launch.
//!
//! Shells come back in their folder; the command that was running is not run
//! again, since nobody wants `rm` replayed. Agents are the exception: an
//! agent CLI comes back with the flags it had and resumes its conversation
//! (Claude Code by session id, Codex and OpenCode by "the last one").

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<TabState>,
    pub active: usize,
    pub sidebar: bool,
    pub inspector: bool,
    /// The side panels' widths, as last dragged.
    pub sidebar_width: f32,
    pub inspector_width: f32,
    /// The sidebar lists by time (Now, Today, …) rather than by project.
    pub recent: bool,
    /// Background jobs, started again at launch (Settings › Terminal).
    pub jobs: Vec<JobState>,
}

/// A background job: what `deck job start` ran, under its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobState {
    pub name: String,
    pub cmd: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            sidebar: true,
            inspector: false,
            sidebar_width: crate::theme::SIDEBAR_PX,
            inspector_width: crate::theme::INSPECTOR_PX,
            recent: false,
            jobs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabState {
    #[serde(default)]
    pub pinned: bool,
    /// Which pane had the keyboard: an index into the tab's panes in order.
    #[serde(default)]
    pub focused: usize,
    pub root: NodeState,
    /// Pinned by you to the top of the sidebar.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_top: bool,
    /// When it last did something, in seconds since the epoch, so the
    /// sidebar's Recent view keeps its age across a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeState {
    Pane(PaneState),
    Split {
        row: bool,
        children: Vec<NodeState>,
        /// Each child's share, by weight; missing in older files.
        #[serde(default)]
        sizes: Vec<f32>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PaneState {
    Terminal {
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        agent: Option<AgentRun>,
        /// The key its scrollback is kept under at quit, to show again under
        /// the new shell (see `scrollback_file`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scrollback: Option<String>,
        /// Its session in the background service, to attach to again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// Its control token, kept so the shell's `deck` still answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Browser {
        url: String,
    },
    Chat {
        thread: String,
    },
    Settings,
}

/// An agent CLI that was running in a terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun {
    /// `claude`, `codex`, …
    pub cmd: String,
    /// The options it was started with, without a prompt or an earlier
    /// resume (see [`keep_flags`]).
    #[serde(default)]
    pub flags: Vec<String>,
    /// Claude Code's conversation, when deck could tell which one it was.
    #[serde(default)]
    pub session: Option<String>,
    /// Started by deck as an agent tab, rather than typed into a shell.
    #[serde(default)]
    pub tab: bool,
}

pub fn path() -> PathBuf {
    crate::control::socket_dir().join("session.json")
}

/// Where a terminal's scrollback waits between a quit and the next launch.
pub fn scrollback_file(key: &str) -> PathBuf {
    let key: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    crate::control::socket_dir().join("scrollback").join(format!("{key}.ansi"))
}

pub fn load() -> Option<Session> {
    let bytes = std::fs::read(path()).ok()?;
    serde_json::from_slice(&bytes)
        .map_err(|e| tracing::warn!("session.json unreadable, starting fresh: {e}"))
        .ok()
}

/// Written to a temporary file and renamed over the old one, so a crash
/// mid-write never leaves half a layout. Writes are serialised: two at once
/// would race on the temporary file.
pub fn save(session: &Session) {
    static WRITING: Mutex<()> = Mutex::new(());
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let path = path();
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(session)
        .map_err(std::io::Error::other)
        .and_then(|json| std::fs::write(&tmp, json))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        tracing::warn!("could not save the session: {e}");
    }
}

/// Options that take a value, per agent; anything else that does not start
/// with `-` is a prompt or a subcommand and is left out.
fn valued(cmd: &str) -> &'static [&'static str] {
    match cmd {
        "claude" => &[
            "--model",
            "--permission-mode",
            "--add-dir",
            "--settings",
            "--setting-sources",
            "--mcp-config",
            "--agent",
            "--agents",
            "--fallback-model",
            "--append-system-prompt",
            "--system-prompt",
            "--allowedTools",
            "--allowed-tools",
            "--disallowedTools",
            "--disallowed-tools",
            "--plugin-dir",
        ],
        "codex" => &[
            "-m",
            "--model",
            "-c",
            "--config",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-p",
            "--profile",
            "-C",
            "--cd",
        ],
        "opencode" => &["-m", "--model", "--agent"],
        _ => &[],
    }
}

/// Options that pick a conversation or make the run one-shot: dropped,
/// since the restore picks the conversation itself. The value-taking ones
/// take their value with them.
fn dropped(cmd: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match cmd {
        "claude" => (
            &["--continue", "-c", "--fork-session", "--print", "-p"],
            &[
                "--resume",
                "-r",
                "--session-id",
                "--output-format",
                "--input-format",
            ],
        ),
        "codex" => (&["--last"], &[]),
        "opencode" => (&["--continue", "-c"], &["--session", "-s", "--prompt"]),
        _ => (&[], &[]),
    }
}

/// The options worth starting `cmd` with again, from the `argv` it ran with:
/// its options and their values, never its prompt.
pub fn keep_flags(cmd: &str, argv: &[String]) -> Vec<String> {
    let valued = valued(cmd);
    let (drop, drop_valued) = dropped(cmd);
    let mut out = Vec::new();
    let mut args = argv.iter().skip(1);
    while let Some(a) = args.next() {
        let name = a.split('=').next().unwrap_or(a);
        let inline = a.contains('=');
        if drop.contains(&a.as_str()) {
            continue;
        }
        if drop_valued.contains(&name) {
            if !inline {
                args.next();
            }
            continue;
        }
        if !a.starts_with('-') {
            continue;
        }
        out.push(a.clone());
        if valued.contains(&name) && !inline {
            if let Some(v) = args.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

/// The command line that brings an agent back where it was.
pub fn resume_line(run: &AgentRun) -> String {
    let mut words: Vec<String> = vec![run.cmd.clone()];
    match run.cmd.as_str() {
        "claude" => {
            words.extend(run.flags.iter().cloned());
            match &run.session {
                Some(id) => {
                    words.extend(["--resume".into(), id.clone()]);
                    // Claude Code keeps a session alive in the background
                    // after its window goes, and then refuses to resume it:
                    // open the live one if it is there (by the short id
                    // `claude agents` shows), else resume it.
                    let short: String = id.chars().take(8).collect();
                    let resume = words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
                    return format!("claude attach {} 2>/dev/null || {resume}", quote(&short));
                }
                None => words.push("--continue".into()),
            }
        }
        "codex" => {
            words.extend(["resume".into(), "--last".into()]);
            words.extend(run.flags.iter().cloned());
        }
        "opencode" => {
            words.extend(run.flags.iter().cloned());
            words.push("--continue".into());
        }
        _ => words.extend(run.flags.iter().cloned()),
    }
    words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ")
}

/// Single-quoted for the shell when it needs to be.
fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Where Claude Code keeps the conversations started in `cwd`: the path
/// with every character that is not a letter or digit turned into `-`.
pub(crate) fn claude_dir(cwd: &Path) -> PathBuf {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    agent_core::dirs::home_dir()
        .unwrap_or_default()
        .join(".claude/projects")
        .join(slug)
}

/// Claude Code's conversations in `cwd`, the most recently written first.
pub fn claude_sessions(cwd: &Path) -> Vec<String> {
    let mut found: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(claude_dir(cwd))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let id = path
                .file_name()?
                .to_str()?
                .strip_suffix(".jsonl")?
                .to_string();
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, id))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, id)| id).collect()
}

/// The conversation an agent's command line picks: `--resume ID`,
/// `-r ID` or `--session-id ID`.
pub fn session_arg(argv: &[String]) -> Option<String> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--resume=").or_else(|| a.strip_prefix("--session-id=")) {
            return Some(v.to_string());
        }
        if matches!(a.as_str(), "--resume" | "-r" | "--session-id") {
            return it.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

/// The conversation the Claude Code process `pid` holds, from the file
/// Claude Code keeps for each running one (`~/.claude/sessions/<pid>.json`).
/// A `claude attach <id>` client has none of its own: it holds the
/// background session whose id starts with the one it was given.
pub fn claude_live_session(pid: u32, argv: &[String]) -> Option<String> {
    let dir = agent_core::dirs::home_dir()?.join(".claude/sessions");
    let id_in = |path: &Path| -> Option<String> {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        v.get("sessionId")?.as_str().map(str::to_string)
    };
    if let Some(id) = id_in(&dir.join(format!("{pid}.json"))) {
        return Some(id);
    }
    let short = attach_target(argv)?;
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| id_in(&e.path()))
        .find(|id| id.starts_with(short))
}

/// `claude attach 9e66de16` → `9e66de16`.
fn attach_target(argv: &[String]) -> Option<&str> {
    let at = argv.iter().position(|a| a == "attach")?;
    argv.get(at + 1).map(String::as_str).filter(|s| s.len() >= 6 && !s.starts_with('-'))
}

/// What a Claude Code conversation has been called: the titles Claude Code
/// gave it, oldest first, and the first real thing you asked in it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConvTitles {
    pub titles: Vec<String>,
    pub ask: Option<String>,
}

impl ConvTitles {
    /// Its newest title other than `not` (one Claude Code gave every open
    /// session alike), else the first ask.
    pub fn pick(&self, not: &str) -> Option<String> {
        self.titles.iter().rev().find(|t| t.as_str() != not).cloned().or_else(|| self.ask.clone())
    }
}

pub fn claude_titles(cwd: &Path, id: &str) -> ConvTitles {
    titles_in(&claude_dir(cwd).join(format!("{id}.jsonl")))
}

fn titles_in(path: &Path) -> ConvTitles {
    use std::io::BufRead;
    let mut out = ConvTitles::default();
    let Ok(file) = std::fs::File::open(path) else { return out };
    let mut first_ask: Option<String> = None;
    for (n, line) in std::io::BufReader::new(file).lines().map_while(Result::ok).enumerate() {
        if line.contains("\"ai-title\"") || line.contains("\"custom-title\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let t = v.get("customTitle").or_else(|| v.get("aiTitle")).and_then(|t| t.as_str());
                if let Some(t) = t.map(str::trim).filter(|t| !t.is_empty()) {
                    if out.titles.last().map(String::as_str) != Some(t) {
                        out.titles.push(t.to_string());
                    }
                }
            }
            continue;
        }
        if out.ask.is_none() && n < 400 && line.contains("\"user\"") {
            if let Some(ask) = ask_of(&line) {
                first_ask.get_or_insert_with(|| ask.clone());
                // "hi" says nothing; the first ask of a few words does.
                if ask.split_whitespace().count() >= 3 {
                    out.ask = Some(ask);
                }
            }
        }
    }
    out.ask = out.ask.or(first_ask);
    out
}

/// A user message's first line, when it is something you asked rather
/// than a slash command or a caveat Claude Code wrote.
fn ask_of(line: &str) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("user") || v.get("isMeta").is_some_and(|m| m == true) {
        return None;
    }
    let text = match &v["message"]["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?.to_string();
    if first.starts_with('<') || first.starts_with('/') || first.starts_with("Caveat:") {
        return None;
    }
    let short: String = first.chars().take(60).collect();
    Some(if first.chars().count() > 60 { format!("{short}…") } else { short })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &str) -> Vec<String> {
        words.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn an_attach_client_names_its_session_by_prefix() {
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(attach_target(&argv("claude attach 9e66de16")), Some("9e66de16"));
        assert_eq!(attach_target(&argv("claude attach")), None);
        assert_eq!(attach_target(&argv("claude --resume 9e66de16-72a7")), None);
    }

    #[test]
    fn a_conversation_is_named_by_its_own_titles_or_first_ask() {
        assert_eq!(session_arg(&argv("claude --model opus --resume ab-1 fix")).as_deref(), Some("ab-1"));
        assert_eq!(session_arg(&argv("claude --session-id=x9")).as_deref(), Some("x9"));
        assert_eq!(session_arg(&argv("claude --continue")), None);
        let dir = std::env::temp_dir().join(format!("deck-first-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("s1.jsonl"),
            concat!(
                r#"{"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>x"}}"#, "\n",
                r#"{"type":"user","message":{"content":"/clear"}}"#, "\n",
                r#"{"type":"user","message":{"content":"hi"}}"#, "\n",
                r#"{"type":"user","message":{"content":[{"type":"text","text":"make the app icon\nround"}]}}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"macOS app icon"}"#, "\n",
                r#"{"type":"ai-title","aiTitle":"Modern minimal browser"}"#, "\n",
            ),
        )
        .unwrap();
        let t = titles_in(&dir.join("s1.jsonl"));
        assert_eq!(t.ask.as_deref(), Some("make the app icon"));
        assert_eq!(t.pick("Modern minimal browser").as_deref(), Some("macOS app icon"));
        assert_eq!(ConvTitles { titles: vec![], ask: Some("hi".into()) }.pick("x").as_deref(), Some("hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flags_keep_options_and_drop_prompts_and_resumes() {
        assert_eq!(
            keep_flags(
                "claude",
                &argv("claude --allow-dangerously-skip-permissions --model opus --resume abc fix the bug")
            ),
            argv("--allow-dangerously-skip-permissions --model opus")
        );
        assert_eq!(
            keep_flags("claude", &argv("claude -c --permission-mode=plan -p hi")),
            argv("--permission-mode=plan")
        );
        assert_eq!(
            keep_flags("codex", &argv("codex -m gpt-5 --yolo resume --last")),
            argv("-m gpt-5 --yolo")
        );
        assert!(keep_flags("gemini", &argv("gemini")).is_empty());
    }

    #[test]
    fn agents_come_back_into_their_conversation() {
        let run = |cmd: &str, flags: &str, session: Option<&str>| AgentRun {
            cmd: cmd.into(),
            flags: if flags.is_empty() {
                vec![]
            } else {
                argv(flags)
            },
            session: session.map(str::to_string),
            tab: false,
        };
        assert_eq!(
            resume_line(&run("claude", "--model opus", Some("1f2e-33"))),
            "claude attach 1f2e-33 2>/dev/null || claude --model opus --resume 1f2e-33"
        );
        assert_eq!(resume_line(&run("claude", "", None)), "claude --continue");
        assert_eq!(
            resume_line(&run("codex", "--yolo", None)),
            "codex resume --last --yolo"
        );
        assert_eq!(
            resume_line(&run("opencode", "", None)),
            "opencode --continue"
        );
        let mut spaced = run("claude", "", None);
        spaced.flags = vec!["--append-system-prompt".into(), "be brief".into()];
        assert_eq!(
            resume_line(&spaced),
            "claude --append-system-prompt 'be brief' --continue"
        );
    }

    #[test]
    fn claude_project_folders_match_its_naming() {
        let d = claude_dir(Path::new("/Users/me/my.app/src_x"));
        assert!(
            d.ends_with(".claude/projects/-Users-me-my-app-src-x"),
            "{d:?}"
        );
    }

    #[test]
    fn sessions_round_trip_and_old_files_load() {
        let s = Session {
            tabs: vec![TabState {
                pinned: false,
                focused: 1,
                root: NodeState::Split {
                    row: true,
                    sizes: vec![2., 1.],
                    children: vec![
                        NodeState::Pane(PaneState::Terminal {
                            cwd: Some("/tmp".into()),
                            agent: Some(AgentRun {
                                cmd: "claude".into(),
                                flags: vec![],
                                session: Some("abc".into()),
                                tab: true,
                            }),
                            scrollback: None,
                            session: Some("tab-abc".into()),
                            token: None,
                        }),
                        NodeState::Pane(PaneState::Browser {
                            url: "https://example.com".into(),
                        }),
                    ],
                },
                on_top: true,
                touched: Some(1_761_600_000),
            }],
            active: 0,
            sidebar: false,
            inspector: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
        let old: Session =
            serde_json::from_str(r#"{"tabs":[{"root":{"pane":{"kind":"settings"}}}]}"#).unwrap();
        assert!(old.sidebar && old.tabs.len() == 1);
        assert_eq!(old.tabs[0].touched, None);
        assert!(!old.tabs[0].on_top);
    }
}
