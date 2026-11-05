//! The control plane's chat: talk to an agent (Claude Code, OpenCode, Codex,
//! Gemini…) in a structured thread (streamed text, tool calls, interrupt)
//! while the agent drives deck itself through the `deck` CLI: it can open
//! terminals beside the chat, run servers there, and open pages in deck's
//! browser.
//!
//! The session machinery (adapters, the tokio↔gpui driver, the canonical
//! transcript) comes from yourcode's agent crates; this module is only the
//! view and the wiring.

use crate::field::{Field, FieldEvent};
use crate::icon::{icon, spinner, Icon};
use crate::markdown::{self, MdBlock, Spans};
use crate::menu::{self, Entry, Item};
use crate::pasteboard::{self, Pasted};
use crate::select;
use crate::settings::Access;
use crate::theme::{self, u, OneLine};
use crate::threads;
use agent_core::{
    Action, AgentEvent, AgentId, AppliedKind, Attachment, AttachmentKind, Block, Decision,
    PermissionMode, RequestId, Role, SessionId, StopReason, ToolInvocation, ToolOutput, Transcript,
    UserTurn,
};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    canvas, div, img, list, px, relative, rgb, rgba, Animation, AnimationExt, AnyElement, Context,
    ElementId, Entity, EventEmitter, ExternalPaths, FocusHandle, FontStyle, FontWeight,
    InteractiveText, KeyDownEvent, ListAlignment, ListState, SharedString, StrikethroughStyle,
    StyledText, Subscription, Task, TextRun, UnderlineStyle, Window,
};
use session::{Conn, DriverEvent};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What every agent is told about running inside deck. Appended to the
/// vendor's own prompt, never replacing it.
const HARNESS: &str = "\
You are running inside deck, a macOS workspace of terminals, a browser and \
coding agents. Drive it with the `deck` CLI, already on PATH and authorised \
for this session. Output is JSON.
- `deck pane list`: the terminals, browser pages and agents open right now.
- `deck pane split --cmd 'npm run dev'`: start a long-running command (dev \
servers, watchers) in a new terminal beside this chat. Prefer this to running \
servers in your own shell, which would block you.
- `deck pane read --pane N`: read a terminal's screen (check a server came up).
- `deck pane send --pane N 'text'`: type into a terminal.
- `deck open http://localhost:3000`: open a page in deck's browser beside this chat.
- `deck browser eval --pane N 'document.title'`: inspect a page.
- `deck task new 'a self-contained brief'`: hand a piece of work to a sub-agent \
on its own branch and git worktree of this project. It works in parallel; you \
get a message here when it finishes. Use it to split big jobs or try things \
side by side.
- `deck task list`, `deck task show --task ID`, `deck task diff --task ID`: \
follow sub-agents and read what they changed; `deck task send --task ID 'text'` \
to steer one.
- `deck task merge --task ID` (or `pr`, `discard`): finish one once it is good.
Use these whenever the user asks to run, serve, preview or check something. \
Keep answers short; say what you ran and what you saw.";

/// Probed agents and the runtime they run on, shared by every chat.
pub struct ChatEnv {
    pub backend: session::Backend,
    pub registry: session::Registry,
    pub substrate: session::Substrate,
}

impl ChatEnv {
    /// Blocking (it probes every agent CLI); run it off the UI thread.
    pub fn start() -> anyhow::Result<Self> {
        let root = crate::control::socket_dir().join("agents");
        let backend = session::Backend::new(&root)?;
        let mut adapters: Vec<Arc<dyn agent_core::AgentAdapter>> =
            vec![Arc::new(agent_claude::ClaudeAdapter::new())];
        adapters.extend(agent_cli::CliAdapter::all());
        let registry = backend
            .handle()
            .block_on(session::Registry::probe_all(adapters));
        Ok(Self {
            backend,
            registry,
            substrate: session::Substrate {
                system_prompt: Arc::from(HARNESS),
                ..Default::default()
            },
        })
    }

    /// The agent a new chat starts with: Claude if it is ready, else the
    /// first one that is.
    pub fn default_agent(&self) -> Option<AgentId> {
        let claude = AgentId::new("claude");
        if self.registry.get(&claude).is_some_and(|e| e.is_ready()) {
            return Some(claude);
        }
        self.registry
            .default_agent()
            .filter(|e| e.is_ready())
            .map(|e| e.id())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStatus {
    Idle,
    Working,
    /// Working, but stopped on an approval or a question for the user.
    Waiting,
    Failed,
}

pub enum ChatEvent {
    Focused,
    /// A link in a reply was clicked.
    OpenUrl(String),
    /// "Open in editor" on a changed file: its path, with its changes shown.
    OpenFile(PathBuf),
    /// A new thread was pointed at another folder.
    Moved,
    /// The first message of a thread set to work on its own branch: start
    /// a task (its own branch and worktree) with this brief and agent.
    StartTask { brief: String, agent: String, cwd: PathBuf },
    /// "Make … the default" in the agent menu: new chats start with it.
    DefaultAgent(String),
}

/// An attached image's square thumbnail: being made, made, or not
/// possible (then the image shows as a named chip, like any file).
enum Thumb {
    Making,
    Ready(PathBuf),
    Failed,
}

/// A message on its way out: typed text plus what was attached.
#[derive(Clone)]
struct Outgoing {
    text: String,
    attachments: Vec<PathBuf>,
    /// ⌘⏎: read it now rather than after the current step.
    now: bool,
}

impl Outgoing {
    fn blocks(&self) -> Vec<Block> {
        self.attachments
            .iter()
            .map(|p| Block::Attachment {
                path: p.display().to_string(),
                kind: if is_image(p) {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
            })
            .collect()
    }

    fn turn(&self, id: String) -> UserTurn {
        UserTurn {
            text: self.text.clone(),
            attachments: self
                .attachments
                .iter()
                .map(|p| Attachment {
                    path: p.clone(),
                    mime: mime_for(p).into(),
                })
                .collect(),
            id: Some(id),
            now: self.now,
        }
    }
}

/// Something the agent is waiting on the user for, shown as a card above the
/// composer.
enum Ask {
    Permission {
        id: RequestId,
        action: Action,
        input: serde_json::Value,
        reason: Option<String>,
        tool: String,
        tool_use: Option<agent_core::ToolId>,
    },
    Question {
        id: RequestId,
        tool_use: Option<agent_core::ToolId>,
        questions: Vec<agent_core::Question>,
        /// Answers so far, one per question, in order.
        picked: Vec<String>,
        /// Options ticked in a multi-select question.
        toggled: HashSet<usize>,
    },
}

impl Ask {
    fn id(&self) -> RequestId {
        match self {
            Ask::Permission { id, .. } | Ask::Question { id, .. } => *id,
        }
    }

    fn tool_use(&self) -> Option<&agent_core::ToolId> {
        match self {
            Ask::Permission { tool_use, .. } | Ask::Question { tool_use, .. } => tool_use.as_ref(),
        }
    }
}

/// The answers to an approval, as in Claude Code's own prompt.
const PERMISSION_CHOICES: [(Decision, &str, &str); 3] = [
    (Decision::Allow, "Yes", "⏎"),
    (
        Decision::AllowAlways,
        "Yes, and don't ask again for this in this chat",
        "",
    ),
    (Decision::Deny, "No", "esc"),
];

/// The picker above the composer: `@` files, `/` commands, `$` skills.
struct Mention {
    /// Byte offset of the sigil in the composer.
    start: usize,
    /// '@', '/' or '$'.
    kind: char,
    matches: Vec<String>,
    /// For commands and skills: what each does, and what goes in the box.
    notes: Vec<String>,
    inserts: Vec<String>,
    selected: usize,
}

/// The agent's slash commands and the user's skills, as (shown name, what
/// it does, what to type). Built-ins first, then the user's own from
/// `~/.claude/commands`, the project's `.claude/commands` and the skills
/// folders, each described by its file's `description:`.
fn commands_for(agent: &str, cwd: &std::path::Path, sigil: char) -> Vec<(String, String, String)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let describe = |path: &std::path::Path| -> String {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| t.lines().take(20).find_map(|l| l.strip_prefix("description:").map(|d| d.trim().trim_matches('"').to_string())))
            .unwrap_or_default()
    };
    let mut out: Vec<(String, String, String)> = Vec::new();
    if sigil == '/' {
        let builtins: &[(&str, &str)] = if agent == "codex" {
            &[("compact", "Summarise the conversation to free context"), ("model", "Choose the model"), ("init", "Write an AGENTS.md for this project"), ("review", "Review the current changes"), ("diff", "Show the changes"), ("status", "Session and limits"), ("new", "Start a new conversation")]
        } else {
            &[("compact", "Summarise the conversation to free context"), ("clear", "Start over with an empty context"), ("context", "What fills the context"), ("cost", "What this session has cost"), ("init", "Write a CLAUDE.md for this project"), ("memory", "Edit Claude's memory files"), ("model", "Choose the model"), ("review", "Review a pull request"), ("agents", "Manage subagents"), ("mcp", "MCP servers")]
        };
        out.extend(builtins.iter().map(|(n, d)| (format!("/{n}"), d.to_string(), format!("/{n}"))));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
        for dir in [claude.join("commands"), cwd.join(".claude/commands")] {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "md") {
                    if let Some(stem) = p.file_stem() {
                        let n = stem.to_string_lossy().into_owned();
                        out.push((format!("/{n}"), describe(&p), format!("/{n}")));
                    }
                }
            }
        }
    }
    // Skills: Claude Code runs them as `/name`; other agents read `$name`.
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude"));
    for dir in [claude.join("skills"), cwd.join(".claude/skills"), home.join(".agents/skills")] {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let skill = e.path().join("SKILL.md");
            if skill.exists() {
                let n = e.file_name().to_string_lossy().into_owned();
                let insert = if agent == "claude" { format!("/{n}") } else { format!("${n}") };
                let shown = if sigil == '$' { format!("${n}") } else { format!("/{n}") };
                out.push((shown, describe(&skill), insert));
            }
        }
    }
    let mut seen = HashSet::new();
    out.retain(|(n, ..)| seen.insert(n.clone()));
    out
}

pub struct ChatPane {
    /// The console under the thread: every command the agent ran in it,
    /// with what each printed, live while it runs.
    console: bool,
    console_scroll: gpui::ScrollHandle,
    /// Moved off its agent at a usage limit: that agent, and when its limit
    /// resets. The first message after that goes back to it.
    handback: Option<(AgentId, Instant)>,
    /// The thread's agent is close to a plan limit: what to say about it.
    pub limit_note: Option<SharedString>,
    /// The outline on the thread's right edge: your messages, to jump to.
    outline_open: bool,
    env: Arc<ChatEnv>,
    pub agent: AgentId,
    session_id: SessionId,
    transcript: Transcript,
    /// Parsed markdown per (message, block), re-parsed only while streaming.
    docs: HashMap<(usize, usize), markdown::Cache>,
    conn: Option<Conn>,
    pub status: ChatStatus,
    error: Option<SharedString>,
    pub composer: Entity<Field>,
    list: ListState,
    /// Scrolled up off the end: new content no longer pulls the view down.
    paused: bool,
    /// Messages that arrived while paused.
    unread: usize,
    expanded: HashSet<String>,
    /// Work logs showing every step while the agent works (else the newest
    /// few), by (message, group).
    log_all: HashSet<(usize, usize)>,
    /// How much of each streaming text block is on screen, and when that
    /// last moved: text comes out at a steady pace instead of in bursts.
    reveal: HashMap<(usize, usize), (f32, Instant)>,
    /// Some text was still catching up when last drawn.
    revealing: Cell<bool>,
    /// Steps that open by themselves (the latest plan) but were closed.
    collapsed: HashSet<String>,
    /// When each tool started and, once done, how long it took.
    tool_times: HashMap<String, (Instant, Option<Duration>)>,
    /// The agent and model menu is open, on this agent's models.
    model_menu: Option<AgentId>,
    /// The model asked for, an id from the agent's own list; `None` runs
    /// the CLI's default.
    model: Option<smol_str::SmolStr>,
    /// The session was started early, when the composer took focus, so
    /// the first message does not wait for the agent to boot.
    warmed: bool,
    mode: PermissionMode,
    cwd: PathBuf,
    /// The SSH host the agent runs on; `cwd` is a folder there. `None`:
    /// this Mac.
    pub remote: Option<String>,
    /// "Work on its own branch": the first message starts a task instead.
    own_branch: bool,
    /// `DECK_*` credentials so the agent can drive this window.
    agent_env: Vec<(String, String)>,
    pub title: SharedString,
    pub last_activity: Instant,
    /// When it last did something, in milliseconds since the epoch: what
    /// the sidebar shows and sorts by, kept across restarts.
    pub updated: i64,
    /// Messages sent while a turn runs, for agents that cannot take input
    /// mid-turn: deck holds them and sends each when the turn ends.
    queued: std::collections::VecDeque<Outgoing>,
    /// Messages sent while a turn runs to an agent that queues them itself
    /// (Claude Code): already with the agent, shown as queued until it reads
    /// them, then placed in the thread where it did.
    inflight: Vec<(String, Outgoing)>,
    /// Esc was pressed: the turn that ends next was stopped, not failed.
    interrupting: bool,
    /// Approvals and questions the agent is waiting on, oldest first.
    asks: Vec<Ask>,
    /// The highlighted choice on the top card.
    ask_choice: usize,
    turn_started: Option<Instant>,
    /// How long each finished turn took, by assistant message.
    worked: HashMap<usize, Duration>,
    /// Work logs the user opened or closed by hand, by (message, group).
    log_open: HashMap<(usize, usize), bool>,
    copied: Option<String>,
    /// Text selected with the mouse in the transcript, for ⌘C.
    selection: Option<select::Selection>,
    /// The button went down over text and is still held.
    selecting: bool,
    /// The message being drawn and the number of its next piece of text, so
    /// every piece gets its key in reading order.
    sel_msg: Cell<usize>,
    sel_unit: Cell<usize>,
    /// Text as painted last frame, to find what is under the mouse.
    painted: Rc<RefCell<Vec<select::Painted>>>,
    /// Every piece of text drawn, by key, so a selection reaching out of
    /// view still copies whole.
    units: RefCell<BTreeMap<select::Key, select::Unit>>,
    /// The right-click menu on a message, while open.
    menu: Option<menu::Open>,
    /// A prompt rewrite in flight: its number, and whether it sends after.
    refining: Option<(u64, bool)>,
    refine_seq: u64,
    _refine: Option<Task<()>>,
    /// Your own words, kept while a rewrite stands in for them (⌃Z).
    mine: Option<String>,
    /// The next ⏎ sends the text as it is (a rewrite failed, or ⌃Z put your
    /// words back).
    as_typed: bool,
    /// A line about the last rewrite, shown under the composer.
    refine_note: Option<SharedString>,
    /// Auto-retry after a failed turn: the try it is on, when, and why.
    retry: Option<(u32, Instant, String)>,
    /// Tries in a row since the last turn that went through.
    retries: u32,
    /// The HTTP status and message of the API call that failed this turn,
    /// when the agent reported one.
    failed: Option<(Option<u16>, String)>,
    /// The project's files as they were when each of your messages was
    /// sent, by message: a commit to go back to.
    checkpoints: HashMap<usize, String>,
    /// The files as they were just before going back, to put them back.
    undo_rewind: Option<String>,
    _retry: Option<Task<()>>,
    /// Files and images going out with the next message.
    attachments: Vec<PathBuf>,
    /// Square thumbnails of attached images, by the image's path.
    thumbs: HashMap<PathBuf, Thumb>,
    /// An attachment open over the thread.
    preview: Option<crate::preview::Preview>,
    preview_focus: FocusHandle,
    /// The access menu is open above its chip, with this row highlighted.
    access_menu: Option<usize>,
    /// Files under `cwd` for `@`, listed on first use and again when stale.
    files: Option<Arc<Vec<String>>>,
    files_at: Option<Instant>,
    listing: bool,
    mention: Option<Mention>,
    /// The `@` whose picker was dismissed with Escape, so it stays shut.
    mention_dismissed: Option<usize>,
    /// Where this thread is saved, and whether it is the pinned control chat.
    thread_id: String,
    pub control: bool,
    archived: bool,
    /// The agent's own session id, so a later launch can resume it.
    native_id: Option<String>,
    /// The model the agent said it runs, from its latest start (`model` is
    /// the one this thread asked for, `None` for the agent's default).
    ran_model: Option<String>,
    /// A turn sent into a resumed session; if the resume fails it is sent
    /// again into a fresh one that gets the thread replayed.
    resuming: Option<UserTurn>,
    _save: Option<Task<()>>,
    _pump: Option<Task<()>>,
    _composer: Subscription,
    _lifecycle: [Subscription; 2],
}

impl EventEmitter<ChatEvent> for ChatPane {}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

pub fn is_image(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn mime_for(p: &std::path::Path) -> &'static str {
    match p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ if p.is_dir() => "inode/directory",
        _ => "application/octet-stream",
    }
}

/// Each access mode's mark, on its chip and in its menu.
fn access_icon(a: Access) -> Icon {
    match a {
        Access::Ask => Icon::Shield,
        Access::Edits => Icon::Pencil,
        Access::Full => Icon::Bolt,
    }
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

impl ChatPane {
    pub fn new(
        env: Arc<ChatEnv>,
        agent: AgentId,
        cwd: PathBuf,
        agent_env: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            let mut f = Field::multiline(
                "Ask anything, @ to mention a file, paste or drop images…",
                cx,
            );
            f.bare = true;
            f.attachments = true;
            f
        });
        let sub = cx.subscribe(&composer, |chat, _, ev: &FieldEvent, cx| match ev {
            FieldEvent::Submit(_) => chat.submit(false, cx),
            FieldEvent::SubmitNow => chat.submit(true, cx),
            FieldEvent::Cancel => chat.interrupt(cx),
            FieldEvent::Attach(p) => chat.paste(p, cx),
            FieldEvent::Changed => {
                if chat.composer.read(cx).text().trim().is_empty() {
                    chat.mine = None;
                    chat.as_typed = false;
                    chat.refine_note = None;
                }
                chat.expand_keyword(cx);
                chat.update_mention(cx);
                chat.sync_popup(cx);
                chat.schedule_save(cx);
                cx.notify();
            }
            FieldEvent::PopupKey(k) => chat.popup_key(k, cx),
        });
        // Whatever is unsaved when deck quits or the window closes is written
        // before it goes.
        let lifecycle = [
            cx.on_app_quit(|c, cx| {
                c.flush(cx);
                async {}
            }),
            cx.on_release(|c, cx| c.flush(cx)),
        ];
        Self {
            env,
            agent,
            session_id: SessionId::new(),
            transcript: Transcript::default(),
            docs: HashMap::new(),
            conn: None,
            status: ChatStatus::Idle,
            error: None,
            composer,
            list: {
                // Following: the list sticks to its end until you scroll
                // up; it tells us so, for the "Jump to latest" pill.
                let list = ListState::new(0, ListAlignment::Bottom, px(800.));
                let me = cx.entity().downgrade();
                list.set_scroll_handler(move |ev, _, cx| {
                    let paused = ev.is_scrolled;
                    let _ = me.update(cx, |c, cx| {
                        if c.paused != paused {
                            c.paused = paused;
                            if !paused {
                                c.unread = 0;
                            }
                            cx.notify();
                        }
                    });
                });
                list
            },
            paused: false,
            unread: 0,
            expanded: HashSet::new(),
            log_all: HashSet::new(),
            reveal: HashMap::new(),
            revealing: Cell::new(false),
            collapsed: HashSet::new(),
            tool_times: HashMap::new(),
            model_menu: None,
            console: false,
            handback: None,
            limit_note: None,
            outline_open: false,
            console_scroll: gpui::ScrollHandle::new(),
            model: None,
            warmed: false,
            // Edits flow; shell commands stay gated, except `deck …`, which is
            // pre-approved so the agent can always drive this window.
            mode: PermissionMode::Edit,
            cwd,
            remote: None,
            own_branch: false,
            agent_env,
            title: SharedString::default(),
            last_activity: Instant::now(),
            updated: now(),
            queued: Default::default(),
            inflight: Vec::new(),
            interrupting: false,
            asks: Vec::new(),
            ask_choice: 0,
            turn_started: None,
            worked: HashMap::new(),
            log_open: HashMap::new(),
            copied: None,
            selection: None,
            selecting: false,
            sel_msg: Cell::new(0),
            sel_unit: Cell::new(0),
            painted: Rc::default(),
            units: RefCell::default(),
            menu: None,
            refining: None,
            refine_seq: 0,
            _refine: None,
            mine: None,
            as_typed: false,
            refine_note: None,
            retry: None,
            failed: None,
            checkpoints: HashMap::new(),
            undo_rewind: None,
            retries: 0,
            _retry: None,
            attachments: Vec::new(),
            thumbs: HashMap::new(),
            preview: None,
            preview_focus: cx.focus_handle(),
            access_menu: None,
            files: None,
            files_at: None,
            listing: false,
            mention: None,
            mention_dismissed: None,
            thread_id: uuid_like(),
            control: false,
            archived: false,
            native_id: None,
            ran_model: None,
            resuming: None,
            _save: None,
            _pump: None,
            _composer: sub,
            _lifecycle: lifecycle,
        }
    }

    /// A thread saved by an earlier launch, with its draft, attachments and
    /// history. The agent is not started until the next message, which
    /// resumes its own session when it can.
    pub fn restore(
        env: Arc<ChatEnv>,
        saved: threads::Saved,
        agent_env: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let agent = AgentId::new(saved.agent.as_str());
        let mut c = Self::new(env, agent, saved.cwd.clone(), agent_env, cx);
        let mut transcript = saved.transcript;
        settle(&mut transcript);
        c.list.reset(transcript.messages.len());
        c.transcript = transcript;
        c.thread_id = saved.id;
        c.remote = saved.remote;
        c.control = saved.control;
        c.title = saved.title.into();
        c.updated = saved.updated;
        c.mode = if saved.full_access {
            PermissionMode::Yolo
        } else if saved.ask_first {
            PermissionMode::Ask
        } else {
            PermissionMode::Edit
        };
        c.native_id = saved.native_id;
        c.model = saved.model.map(Into::into);
        c.checkpoints = saved.checkpoints.into_iter().collect();
        c.attachments = saved
            .attachments
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        c.worked = saved
            .worked
            .into_iter()
            .map(|(ix, ms)| (ix, Duration::from_millis(ms)))
            .collect();
        let (draft, history) = (saved.draft, saved.history);
        tracing::debug!(
            draft = draft.len(),
            attachments = c.attachments.len(),
            "restored thread"
        );
        c.composer.update(cx, |f, cx| {
            f.set_history(history);
            f.set_text(&draft, cx);
            f.forget_undo();
        });
        c
    }

    /// The thread as saved to disk.
    /// The thread as it stands, to reopen it with a fresh agent process.
    pub fn saved(&self, cx: &gpui::App) -> threads::Saved {
        self.snapshot(cx)
    }

    fn snapshot(&self, cx: &gpui::App) -> threads::Saved {
        let f = self.composer.read(cx);
        threads::Saved {
            id: self.thread_id.clone(),
            control: self.control,
            agent: self.agent.as_str().to_string(),
            cwd: self.cwd.clone(),
            remote: self.remote.clone(),
            full_access: self.mode == PermissionMode::Yolo,
            model: self.model.as_ref().map(|m| m.to_string()),
            ask_first: self.mode == PermissionMode::Ask,
            title: self.title.to_string(),
            transcript: self.transcript.clone(),
            draft: f.text().to_string(),
            attachments: self.attachments.clone(),
            history: f.history().to_vec(),
            native_id: self.native_id.clone(),
            worked: self
                .worked
                .iter()
                .map(|(ix, d)| (*ix, d.as_millis() as u64))
                .collect(),
            checkpoints: self
                .checkpoints
                .iter()
                .map(|(ix, c)| (*ix, c.clone()))
                .collect(),
            // When it last did something, not when it was written: a
            // restored thread keeps its age.
            updated: self.updated,
        }
    }

    /// Save shortly after the last change, off the UI thread. Every edit
    /// pushes the save back, so typing costs one write when it pauses.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        if self.archived {
            return;
        }
        self._save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let Ok(Some(saved)) =
                this.update(cx, |c, cx| c.worth_saving(cx).then(|| c.snapshot(cx)))
            else {
                return;
            };
            let wrote = cx
                .background_executor()
                .spawn(async move { threads::write(&saved) })
                .await;
            if let Err(e) = wrote {
                tracing::warn!("could not save the thread: {e}");
            }
        }));
    }

    /// An untouched new chat is not worth a file; a closed one is filed away.
    fn worth_saving(&self, cx: &gpui::App) -> bool {
        let blank = self.transcript.messages.is_empty()
            && self.attachments.is_empty()
            && self.composer.read(cx).text().is_empty()
            && !self.control;
        !blank && !self.archived
    }

    /// Save now, on this thread: the app is quitting or the pane is going.
    fn flush(&mut self, cx: &gpui::App) {
        self._save = None;
        if self.worth_saving(cx) {
            if let Err(e) = threads::write(&self.snapshot(cx)) {
                tracing::warn!("could not save the thread: {e}");
            }
        }
    }

    /// Closing a thread files it away; it is no longer saved.
    pub fn archive(&mut self) {
        self.archived = true;
        self._save = None;
        threads::archive(&self.thread_id);
        // Its checkpoints go with it; git drops the commits at its next gc.
        if !self.checkpoints.is_empty() {
            let (dir, id) = (self.cwd.clone(), self.thread_id.clone());
            std::thread::spawn(move || crate::checkpoint::forget(&dir, &id));
        }
    }

    /// A turn is running, whether or not it is waiting on the user.
    pub fn busy(&self) -> bool {
        matches!(self.status, ChatStatus::Working | ChatStatus::Waiting)
    }

    /// Tokens, cost and how full the context is, as the agent reported them.
    pub fn usage(&self) -> &agent_core::Usage {
        &self.transcript.usage
    }

    /// The model the agent runs: as it said at its latest start, else the
    /// one this thread asked for.
    pub fn ran_model(&self) -> Option<&str> {
        self.ran_model.as_deref().or(self.model.as_deref())
    }

    /// How long the current turn has been running.
    pub fn working_for(&self) -> Option<Duration> {
        self.turn_started.map(|t| t.elapsed())
    }

    /// Rewrite what is in the composer into a clear prompt, from your own
    /// words if a rewrite already stands in for them; then send it, or leave
    /// it in the box to look at.
    pub fn rewrite(&mut self, send: bool, cx: &mut Context<Self>) {
        // Off in Settings is off for ⌃O as well as ⏎.
        if crate::refine::mode() == crate::refine::Mode::Off {
            return;
        }
        let from = self.composer.read(cx).text().to_string();
        if from.trim().is_empty() || self.refining.is_some() {
            return;
        }
        let src = self.mine.clone().unwrap_or_else(|| from.clone());
        self.refine_seq += 1;
        let seq = self.refine_seq;
        self.refining = Some((seq, send));
        self.refine_note = None;
        let ctx = crate::refine::Context {
            agent: self.agent_name().to_string(),
            folder: self.cwd.display().to_string(),
            task: self.title.to_string(),
            last_reply: self.last_reply_text(),
            history: self.history_brief(),
        };
        let model = crate::refine::model();
        let cwd = self.cwd.clone();
        let text = src.clone();
        self._refine = Some(cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move { crate::refine::rewrite(&text, &ctx, &model, &cwd) })
                .await;
            let _ = this.update(cx, |c, cx| c.rewritten(seq, from, src, out, cx));
        }));
        cx.notify();
    }

    fn rewritten(
        &mut self,
        seq: u64,
        from: String,
        src: String,
        out: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((at, send)) = self.refining else {
            return;
        };
        if at != seq {
            return;
        }
        self.refining = None;
        // Switched off while it ran: the words in the box stay yours, and
        // nothing is sent until you press ⏎ again.
        if crate::refine::mode() == crate::refine::Mode::Off {
            cx.notify();
            return;
        }
        match out {
            Err(e) => {
                self.as_typed = true;
                self.refine_note = Some(format!("{e} · ⏎ sends it as typed").into());
            }
            Ok(_) if self.composer.read(cx).text() != from => {
                self.refine_note =
                    Some("the message changed during the rewrite · ⌃O rewrites it again".into());
            }
            Ok(text) => {
                if self.mine.is_none() {
                    self.mine = Some(src);
                }
                self.as_typed = false;
                self.composer.update(cx, |f, cx| f.set_text(&text, cx));
                if send {
                    self.submit(false, cx);
                }
            }
        }
        cx.notify();
    }

    /// ⌃Z after a rewrite: your own words back, to send as they are.
    pub fn my_words(&mut self, cx: &mut Context<Self>) {
        if let Some(mine) = self.mine.take() {
            self.composer.update(cx, |f, cx| f.set_text(&mine, cx));
            self.as_typed = true;
            self.refine_note = None;
            cx.notify();
        }
    }

    /// The whole of the last reply's text.
    fn last_reply_text(&self) -> String {
        self.transcript
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// A turn just failed: if a retry can fix it, line one up after the
    /// backoff (or the usage limit's reset); otherwise say why not.
    fn maybe_retry(&mut self, cx: &mut Context<Self>) {
        if !crate::retry::enabled() {
            return;
        }
        let (status, reported) = self.failed.take().unwrap_or_default();
        let said = Some(reported)
            .filter(|e| !e.is_empty())
            .or_else(|| self.error.as_ref().map(|e| e.to_string()))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| self.last_reply_text());
        let (epoch, clock) = crate::retry::now();
        match crate::retry::classify(status, &said, epoch, clock) {
            crate::retry::Verdict::Blocked(why) => {
                self.retries = 0;
                self.retry = None;
                self.error = Some(format!("{why}. A retry would not fix this.").into());
            }
            crate::retry::Verdict::Retry { wait, reason } => {
                // Out of plan for this agent: carry on with the one Settings
                // names, if it is ready, instead of waiting hours.
                if reason == "usage limit" {
                    // The next ready agent in the chain after this one (so a
                    // second limit moves on down it), never back to the one
                    // that is out.
                    let chain = crate::settings::load().limit_chain;
                    let here = chain.iter().position(|a| a == self.agent.as_str());
                    let origin = self.handback.as_ref().map(|(a, _)| a.clone());
                    let order = chain.iter().skip(here.map_or(0, |i| i + 1)).chain(chain.iter().take(here.unwrap_or(0)));
                    let all = self.env.registry.all();
                    let to = order
                        .filter_map(|f| all.iter().find(|e| e.id().as_str() == f.as_str() && e.is_ready()))
                        .map(|e| (e.id(), e.display_name().to_string()))
                        .find(|(id, _)| *id != self.agent && Some(id) != origin.as_ref());
                    if let Some((to, name)) = to {
                        let from = self.agent_name().to_string();
                        // Back to the first agent once its limit resets.
                        let reset = Instant::now() + wait.unwrap_or(Duration::from_secs(5 * 3600));
                        if self.handback.is_none() {
                            self.handback = Some((self.agent.clone(), reset));
                        }
                        self.retries = 0;
                        self.retry = None;
                        self.switch_agent(to, None, cx);
                        self.refine_note = Some(format!("{from} reached its usage limit; continuing with {name}").into());
                        self.send_turn(
                            Outgoing {
                                text: "continue".into(),
                                attachments: Vec::new(),
                                now: false,
                            },
                            cx,
                        );
                        cx.notify();
                        return;
                    }
                }
                if self.retries >= crate::retry::MAX_TRIES {
                    self.error = Some(
                        format!(
                            "{reason}: gave up after {} tries. Send anything to try again.",
                            crate::retry::MAX_TRIES
                        )
                        .into(),
                    );
                    self.retries = 0;
                    return;
                }
                self.retries += 1;
                let wait = wait.unwrap_or_else(|| crate::retry::backoff(self.retries));
                self.retry = Some((self.retries, Instant::now() + wait, reason));
                // Tick each second, so the countdown under the composer moves.
                self._retry = Some(cx.spawn(async move |this, cx| loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let due = this.update(cx, |c, cx| {
                        cx.notify();
                        c.retry
                            .as_ref()
                            .is_none_or(|(_, at, _)| Instant::now() >= *at)
                    });
                    match due {
                        Ok(true) => {
                            let _ = this.update(cx, |c, cx| c.retry_now(cx));
                            return;
                        }
                        Ok(false) => {}
                        Err(_) => return,
                    }
                }));
            }
        }
        cx.notify();
    }

    /// Say "continue", as you would after the error, unless something else
    /// already got the agent going again.
    fn retry_now(&mut self, cx: &mut Context<Self>) {
        if self.retry.take().is_none() || self.busy() {
            return;
        }
        self._retry = None;
        self.error = None;
        self.send_turn(
            Outgoing {
                text: "continue".into(),
                attachments: Vec::new(),
                now: false,
            },
            cx,
        );
    }

    fn cancel_retry(&mut self, cx: &mut Context<Self>) {
        self.retry = None;
        self._retry = None;
        self.retries = 0;
        cx.notify();
    }

    /// What the agent is doing this moment, in a line ("Running cargo
    /// test", "Editing src/main.rs", "Thinking"), while a turn runs.
    pub fn activity(&self) -> Option<String> {
        if self.status != ChatStatus::Working {
            return None;
        }
        let last = self
            .transcript
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant);
        let line = last
            .and_then(|m| {
                m.blocks.iter().rev().find_map(|b| match b {
                    Block::Tool(t) if t.result.is_none() => {
                        let (verb, what) = step_phrase(t, true, &self.cwd);
                        let what = what.lines().next().unwrap_or_default().to_string();
                        Some(if what.is_empty() {
                            verb.to_string()
                        } else {
                            format!("{verb} {what}")
                        })
                    }
                    Block::Text { open: true, .. } => Some("Writing a reply".into()),
                    Block::Reasoning { open: true, .. } => Some("Thinking".into()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "Thinking".into());
        Some(line.chars().take(120).collect())
    }

    /// The opening line of the last reply, for a "finished" notification.
    pub fn last_reply(&self) -> Option<String> {
        let m = self
            .transcript
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)?;
        m.blocks.iter().rev().find_map(|b| match b {
            Block::Text { text, .. } => text
                .lines()
                .map(|l| l.trim().trim_start_matches(['#', '*', '-', '>', ' ']))
                .find(|l| !l.is_empty())
                .map(|l| l.chars().take(160).collect()),
            _ => None,
        })
    }

    /// What the agent is waiting on you for, in a line: the command or tool
    /// it wants to run, or the question it asked.
    pub fn waiting_for(&self) -> Option<String> {
        let line = match self.asks.first()? {
            Ask::Permission { tool, input, .. } => {
                match input.get("command").and_then(|c| c.as_str()) {
                    Some(cmd) => format!("Wants to run {}", cmd.lines().next().unwrap_or(cmd)),
                    None => format!("Wants to use {tool}"),
                }
            }
            Ask::Question { questions, .. } => questions.first()?.question.clone(),
        };
        let mut line: String = line.chars().take(180).collect();
        if line.len() < 4 {
            line = "Waiting for you".into();
        }
        Some(line)
    }

    /// The agent's own session id (Claude Code's, for `--resume`).
    pub fn native_id(&self) -> Option<&str> {
        self.native_id.as_deref()
    }

    /// The id its saved file goes by, which a restored layout refers to.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn agent_name(&self) -> SharedString {
        self.env
            .registry
            .get(&self.agent)
            .map(|e| SharedString::from(e.display_name().to_string()))
            .unwrap_or_else(|| self.agent.as_str().to_string().into())
    }

    /// The thread as plain text, for `deck pane read`.
    pub fn read_text(&self) -> String {
        let mut out = String::new();
        for m in &self.transcript.messages {
            out.push_str(match m.role {
                Role::User => "## user\n",
                Role::Assistant => "## assistant\n",
            });
            for b in &m.blocks {
                match b {
                    Block::Text { text, .. } => out.push_str(text),
                    Block::Tool(t) => out.push_str(&format!("[tool] {}", t.title)),
                    Block::Attachment { path, .. } => out.push_str(&format!("[attached] {path}")),
                    _ => continue,
                }
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }

    fn start(
        &mut self,
        hydrate: Option<Transcript>,
        resume: Option<String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(entry) = self.env.registry.get(&self.agent).cloned() else {
            self.fail(format!("{} is not available", self.agent.as_str()), cx);
            return false;
        };
        if !entry.is_ready() {
            let why = match &entry.availability {
                agent_core::Availability::NeedsAuth { hint }
                | agent_core::Availability::Missing { hint } => hint.clone(),
                _ => String::new(),
            };
            self.fail(format!("{} is not ready. {why}", entry.display_name()), cx);
            return false;
        }
        if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        let mut ctx = self.env.substrate.session_ctx(
            &self.env.backend,
            self.session_id,
            &self.cwd,
            self.model.clone(),
            self.mode,
            resume,
        );
        ctx.env = self.agent_env.clone();
        ctx.remote = self.remote.clone();
        // deck's own tools, acting for this chat, allowed without asking
        // like the `deck` command is.
        if let Some(deck) = crate::mcp::server_for(&self.agent_env) {
            let mut servers = (*ctx.mcp).clone();
            servers.push(deck);
            ctx.mcp = Arc::new(servers);
        }
        ctx.allowed_tools = vec!["Bash(deck:*)".into(), "mcp__deck".into()];
        // An agent on a host runs here, on this Mac, signed in as you, and
        // acts on the host through Deck's tools bound to it: nothing to
        // install there, and the login never leaves the Mac. Its own shell
        // and file tools would act on this Mac, so they are off (`host_only`).
        // Agents that cannot turn them off still start over SSH, on the host.
        if let Some(host) = self.remote.clone().filter(|_| crate::host_mcp::bridged(self.agent.as_str())) {
            let dir = self.cwd.to_string_lossy().into_owned();
            if let Some(spec) = crate::host_mcp::server_for(&host, &dir, &self.agent_env) {
                let mut servers = (*ctx.mcp).clone();
                servers.push(spec);
                ctx.mcp = Arc::new(servers);
            }
            ctx.remote = None;
            // A folder of its own per host, the same each time, so the
            // conversation resumes (Claude keeps sessions by folder).
            let safe: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
            let local = crate::control::socket_dir().join("remote-work").join(safe);
            let _ = std::fs::create_dir_all(&local);
            ctx.cwd = local;
            ctx.host_only = true;
            if self.agent.as_str() == "claude" {
                ctx.disallowed_tools = crate::host_mcp::LOCAL_TOOLS.iter().map(|t| t.to_string()).collect();
                // Looking needs no asking; changing the host still does,
                // unless the chat has full access.
                ctx.allowed_tools.extend(["mcp__host__read", "mcp__host__glob", "mcp__host__grep"].map(String::from));
            }
            ctx.system_prompt = format!(
                "{}\n\nYou are working on the remote machine `{host}`, in the folder `{dir}` there. You run on the user's Mac, but your own shell and file tools are turned off: act on `{host}` only through the `host` MCP server's tools (run, read, write, edit, glob, grep), which run there over SSH. Paths are on `{host}`, relative to `{dir}` unless absolute.",
                ctx.system_prompt
            )
            .trim_start()
            .into();
        }
        let mut conn = session::spawn(
            self.env.backend.handle(),
            entry.adapter.clone(),
            ctx,
            hydrate,
        );
        let Some(mut events) = conn.take_events() else {
            self.fail("the session did not start".into(), cx);
            return false;
        };
        self.conn = Some(conn);
        self.error = None;
        let sid = self.session_id;
        // Everything that arrived since the last wake is applied together,
        // with one repaint, so a fast stream costs one layout per frame.
        self._pump = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = events.next().await {
                let mut batch = vec![first];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                if this
                    .update(cx, |chat, cx| chat.apply(sid, batch, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        true
    }

    fn queues_input(&self) -> bool {
        self.env
            .registry
            .get(&self.agent)
            .is_some_and(|e| e.caps().queues_input)
    }

    /// Start a session that carries the thread so far: the agent's own
    /// session when it can resume one, else the thread replayed to it.
    fn start_carrying(&mut self, prior: &Transcript, cx: &mut Context<Self>) -> bool {
        let native = self
            .env
            .registry
            .get(&self.agent)
            .is_some_and(|e| e.caps().native_resume);
        match self.native_id.clone().filter(|_| native) {
            Some(id) => self.start(None, Some(id), cx),
            None => self.start(
                (!prior.messages.is_empty()).then(|| prior.clone()),
                None,
                cx,
            ),
        }
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        self.status = ChatStatus::Failed;
        cx.notify();
    }

    /// Send what is in the composer, with its attachments.
    fn submit(&mut self, now: bool, cx: &mut Context<Self>) {
        // Moved off its agent at a limit that has reset since: this message
        // goes back to it, with the whole conversation.
        if let Some((home, at)) = self.handback.clone() {
            if Instant::now() >= at && !self.busy() {
                self.handback = None;
                let away = self.agent_name().to_string();
                self.switch_agent(home, None, cx);
                self.refine_note = Some(format!("{}'s limit has reset; back from {away}", self.agent_name()).into());
            }
        }
        let text = self.composer.read(cx).text().trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        // With a card up, typing answers it: a question takes the words as
        // its answer; an approval is refused and the words go on to the agent.
        if !self.asks.is_empty() && !text.is_empty() && self.answer_in_words(text.clone(), cx) {
            self.composer.update(cx, |f, cx| f.set_text("", cx));
            return;
        }
        // On its own branch: the message becomes a task's brief, worked on
        // in a fresh worktree; this empty thread gives way to it.
        if self.own_branch && self.transcript.messages.is_empty() && !text.is_empty() && self.remote.is_none() {
            self.composer.update(cx, |f, cx| f.set_text("", cx));
            cx.emit(ChatEvent::StartTask { brief: text, agent: self.agent.as_str().to_string(), cwd: self.cwd.clone() });
            return;
        }
        let mode = crate::refine::mode();
        if self.refining.is_some() {
            if mode != crate::refine::Mode::Off {
                return;
            }
            // Switched off while a rewrite ran: it is dropped, and what you
            // typed goes as it is.
            self.refining = None;
            self._refine = None;
        }
        if self.mine.is_none() && !self.as_typed {
            if let Some(send) = crate::refine::on_enter(mode, &text) {
                self.rewrite(send, cx);
                return;
            }
        }
        // You are sending something yourself: any retry deck had lined up
        // gives way to it.
        self.retry = None;
        self._retry = None;
        self.retries = 0;
        self.mine = None;
        self.as_typed = false;
        self.refine_note = None;
        let attachments = std::mem::take(&mut self.attachments);
        self.mention = None;
        self.composer.update(cx, |f, cx| {
            if !text.is_empty() {
                f.push_history(&text);
            }
            f.popup = false;
            f.set_text("", cx);
        });
        self.send_turn(
            Outgoing {
                text,
                attachments,
                now,
            },
            cx,
        );
    }

    /// Send a message that did not come from the composer (a starter, or
    /// another agent through `deck pane send`); the draft is left alone.
    pub fn send(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_string();
        if !text.is_empty() {
            self.send_turn(
                Outgoing {
                    text,
                    attachments: Vec::new(),
                    now: false,
                },
                cx,
            );
        }
    }

    fn send_turn(&mut self, out: Outgoing, cx: &mut Context<Self>) {
        // Sending is reading the end: follow again.
        if self.paused {
            self.jump_to_latest(cx);
        }
        if matches!(self.status, ChatStatus::Working | ChatStatus::Waiting) && self.conn.is_some() {
            // An agent that queues input gets the message now and reads it
            // when its current step ends, inside the same turn, as in Claude
            // Code. Any other agent gets it from deck when the turn is over.
            if self.queues_input() {
                let id = uuid_like();
                if self
                    .conn
                    .as_ref()
                    .is_some_and(|c| c.send(out.turn(id.clone())))
                {
                    self.inflight.push((id, out));
                    cx.notify();
                    return;
                }
            }
            self.queued.push_back(out);
            cx.notify();
            return;
        }
        self.copied = None;
        if self.title.is_empty() {
            let first = out
                .text
                .lines()
                .next()
                .map(str::to_string)
                .filter(|l| !l.trim().is_empty());
            let first = first
                .or_else(|| out.attachments.first().map(|p| file_name(p)))
                .unwrap_or_default();
            let mut t: String = first.chars().take(48).collect();
            if first.chars().count() > 48 {
                t.push('…');
            }
            self.title = t.into();
        }
        // A session that ended (an error, a restart) is picked up again with
        // the thread so far, not started blank.
        let prior = self.conn.is_none().then(|| self.transcript.clone());
        let before = self.transcript.messages.len();
        self.transcript
            .push_user(out.text.clone(), out.blocks(), now());
        self.checkpoint(before, cx);
        self.sync_list(before, &[], cx);
        let turn = out.turn(uuid_like());
        if let Some(prior) = prior {
            if !self.start_carrying(&prior, cx) {
                return;
            }
            if self.native_id.is_some() {
                self.resuming = Some(turn.clone());
            }
        }
        match &self.conn {
            Some(conn) if conn.send(turn) => {
                self.status = ChatStatus::Working;
                self.last_activity = Instant::now();
                self.updated = now();
                self.turn_started = Some(Instant::now());
            }
            _ => self.fail("the session is no longer accepting input".into(), cx),
        }
        self.schedule_save(cx);
        cx.notify();
    }

    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        if self.mention.is_some() {
            self.close_mention(cx);
            return;
        }
        // Esc while a retry is lined up calls it off.
        if self.retry.is_some() {
            self.cancel_retry(cx);
            return;
        }
        if let (Some(conn), ChatStatus::Working | ChatStatus::Waiting) = (&self.conn, self.status) {
            self.interrupting = conn.interrupt();
        }
        cx.notify();
    }

    // -- attachments ----------------------------------------------------------

    pub fn attach_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for p in paths {
            if !self.attachments.contains(p) {
                self.attachments.push(p.clone());
            }
        }
        self.schedule_save(cx);
        cx.notify();
    }

    fn paste(&mut self, pasted: &Pasted, cx: &mut Context<Self>) {
        match pasted {
            Pasted::Files(paths) => self.attach_paths(paths, cx),
            Pasted::Image(bytes, ext) => {
                let bytes = bytes.clone();
                let ext = *ext;
                cx.spawn(async move |this, cx| {
                    let saved = cx
                        .background_executor()
                        .spawn(async move { pasteboard::save_image(&bytes, ext) })
                        .await;
                    let _ = this.update(cx, |c, cx| match saved {
                        Ok(path) => c.attach_paths(&[path], cx),
                        Err(e) => c.fail(format!("could not keep the pasted image: {e}"), cx),
                    });
                })
                .detach();
            }
        }
    }

    /// Choose the folder a new thread works in. Only before the first
    /// message: the agent was started in the old one, so it starts again.
    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Work here".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(dir) = paths.pop() {
                    let _ = this.update(cx, |c, cx| c.set_cwd(dir, cx));
                }
            }
        })
        .detach();
    }

    /// Run this thread's agent on `host` (an SSH host), in `dir` there.
    /// Before the first message only, as for the folder.
    pub fn set_remote(&mut self, host: String, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.transcript.messages.is_empty() {
            return;
        }
        self.remote = Some(host);
        self.cwd = dir;
        if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        self.warmed = false;
        self.schedule_save(cx);
        cx.emit(ChatEvent::Moved);
        cx.notify();
    }

    pub fn set_cwd(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.transcript.messages.is_empty() || dir == self.cwd || !dir.is_dir() {
            return;
        }
        self.cwd = dir;
        if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        self.warmed = false;
        self.schedule_save(cx);
        cx.emit(ChatEvent::Moved);
        cx.notify();
    }

    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |c, cx| c.attach_paths(&paths, cx));
            }
        })
        .detach();
    }

    fn detach(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.attachments.retain(|p| p != path);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Start square thumbnails for the images in `paths` that have none.
    fn want_thumbs(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if !is_image(&path) || self.thumbs.contains_key(&path) {
                continue;
            }
            self.thumbs.insert(path.clone(), Thumb::Making);
            cx.spawn(async move |this, cx| {
                let src = path.clone();
                let made = cx
                    .background_executor()
                    .spawn(async move { crate::thumb::make(&src) })
                    .await;
                let _ = this.update(cx, |c, cx| {
                    let thumb = match made {
                        Ok(t) => Thumb::Ready(t),
                        Err(e) => {
                            tracing::debug!("no thumbnail for {}: {e}", path.display());
                            Thumb::Failed
                        }
                    };
                    c.thumbs.insert(path, thumb);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Show `path` inside deck, over the thread.
    fn open_preview(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = Some(crate::preview::Preview::loading(path.clone()));
        window.focus(&self.preview_focus);
        cx.spawn(async move |this, cx| {
            let src = path.clone();
            let body = cx
                .background_executor()
                .spawn(async move { crate::preview::load(&src) })
                .await;
            let _ = this.update(cx, |c, cx| {
                if let Some(p) = c.preview.as_mut().filter(|p| p.path == path) {
                    p.body = body;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview.take().is_some() {
            window.focus(&self.composer.read(cx).focus);
            cx.notify();
        }
    }

    // -- @ mentions -------------------------------------------------------------

    fn update_mention(&mut self, cx: &mut Context<Self>) {
        // `/command` at the start or `$skill`: the agent's commands and the
        // user's skills, ranked by name.
        if let Some((sigil, start, query)) = self.composer.read(cx).command_token() {
            if self.mention_dismissed == Some(start) {
                self.mention = None;
            } else {
                let q = query.to_lowercase();
                let mut found: Vec<(i32, (String, String, String))> = commands_for(self.agent.as_str(), &self.cwd, sigil)
                    .into_iter()
                    .filter_map(|c| {
                        let name = c.0[1..].to_lowercase();
                        let score = if name.starts_with(&q) { 2 } else if name.contains(&q) { 1 } else { return None };
                        Some((score, c))
                    })
                    .collect();
                found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1 .0.len().cmp(&b.1 .0.len())));
                found.truncate(8);
                let selected = self.mention.as_ref().filter(|m| m.start == start && m.kind == sigil).map_or(0, |m| m.selected).min(found.len().saturating_sub(1));
                self.mention = Some(Mention {
                    start,
                    kind: sigil,
                    matches: found.iter().map(|(_, c)| c.0.clone()).collect(),
                    notes: found.iter().map(|(_, c)| c.1.clone()).collect(),
                    inserts: found.iter().map(|(_, c)| c.2.clone()).collect(),
                    selected,
                });
            }
            self.sync_popup(cx);
            cx.notify();
            return;
        }
        let token = self.composer.read(cx).mention();
        if token.as_ref().map(|t| t.0) != self.mention_dismissed {
            self.mention_dismissed = None;
        }
        self.mention = match token {
            Some((start, _)) if self.mention_dismissed == Some(start) => None,
            Some((start, query)) => match self.files.clone() {
                Some(files) => {
                    // A new `@` re-lists in the background if the list is
                    // old, so files made since show up; the old list serves
                    // meanwhile.
                    let fresh = self.mention.as_ref().is_some_and(|m| m.start == start);
                    if !fresh && self.files_at.is_some_and(|t| t.elapsed() > FILES_STALE) {
                        self.list_files(cx);
                    }
                    let selected = self
                        .mention
                        .as_ref()
                        .filter(|m| m.start == start)
                        .map_or(0, |m| m.selected);
                    let matches = rank_files(&files, &query, 8);
                    let selected = selected.min(matches.len().saturating_sub(1));
                    Some(Mention {
                        start,
                        kind: '@',
                        matches,
                        notes: Vec::new(),
                        inserts: Vec::new(),
                        selected,
                    })
                }
                None => {
                    self.list_files(cx);
                    Some(Mention {
                        start,
                        kind: '@',
                        matches: Vec::new(),
                        notes: Vec::new(),
                        inserts: Vec::new(),
                        selected: 0,
                    })
                }
            },
            None => None,
        };
        self.sync_popup(cx);
        cx.notify();
    }

    /// List the project's files once, in the background: git's view of it
    /// when it is a repository (so ignored build output stays out), else a
    /// bounded walk.
    fn list_files(&mut self, cx: &mut Context<Self>) {
        if self.listing {
            return;
        }
        self.listing = true;
        let cwd = self.cwd.clone();
        cx.spawn(async move |this, cx| {
            let files = cx
                .background_executor()
                .spawn(async move { project_files(&cwd) })
                .await;
            let _ = this.update(cx, |c, cx| {
                c.files = Some(Arc::new(files));
                c.files_at = Some(Instant::now());
                c.listing = false;
                c.update_mention(cx);
            });
        })
        .detach();
    }

    fn mention_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(m) = self.mention.as_mut() else {
            return;
        };
        let n = m.matches.len().max(1);
        match key {
            "up" => m.selected = (m.selected + n - 1) % n,
            "down" => m.selected = (m.selected + 1) % n,
            "enter" | "tab" => {
                let (start, kind) = (m.start, m.kind);
                let chosen = if kind == '@' { m.matches.get(m.selected).cloned() } else { m.inserts.get(m.selected).cloned() };
                if let Some(text) = chosen {
                    self.mention = None;
                    self.composer.update(cx, |f, cx| {
                        f.popup = false;
                        if kind == '@' {
                            f.complete_mention(start, &text, cx);
                        } else {
                            f.complete_token(start, &text, cx);
                        }
                    });
                }
            }
            _ => self.close_mention(cx),
        }
        cx.notify();
    }

    fn close_mention(&mut self, cx: &mut Context<Self>) {
        self.mention_dismissed = self.mention.take().map(|m| m.start);
        self.sync_popup(cx);
        cx.notify();
    }

    // -- approvals and questions ------------------------------------------------

    /// The composer's keys go to the open popup: the `@` picker, or the
    /// approval card while nothing is typed.
    fn sync_popup(&mut self, cx: &mut Context<Self>) {
        let mention = self.mention.as_ref().is_some_and(|m| !m.matches.is_empty());
        let empty = self.composer.read(cx).text().trim().is_empty();
        let open = mention || self.access_menu.is_some() || (!self.asks.is_empty() && empty);
        self.composer.update(cx, |f, _| f.popup = open);
    }

    fn popup_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(lit) = self.access_menu {
            let n = Access::ALL.len();
            match key {
                "up" => self.access_menu = Some((lit + n - 1) % n),
                "down" | "tab" => self.access_menu = Some((lit + 1) % n),
                "enter" => self.set_access(Access::ALL[lit], cx),
                "escape" => self.close_access_menu(cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        if self.mention.as_ref().is_some_and(|m| !m.matches.is_empty()) {
            self.mention_key(key, cx);
            return;
        }
        let Some(ask) = self.asks.first() else { return };
        let n = match ask {
            Ask::Permission { .. } => PERMISSION_CHOICES.len(),
            Ask::Question {
                questions, picked, ..
            } => {
                let q = &questions[picked.len().min(questions.len() - 1)];
                q.options.len() + usize::from(q.multi)
            }
        };
        match key {
            "up" => self.ask_choice = (self.ask_choice + n - 1) % n.max(1),
            "down" | "tab" => self.ask_choice = (self.ask_choice + 1) % n.max(1),
            "enter" => self.choose(self.ask_choice, cx),
            "escape" => self.decline(cx),
            _ => {}
        }
        cx.notify();
    }

    /// Pick option `i` of the card on top: an approval choice, or an answer.
    fn choose(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(ask) = self.asks.first_mut() else {
            return;
        };
        match ask {
            Ask::Permission { id, .. } => {
                let id = *id;
                let decision = PERMISSION_CHOICES.get(i).map_or(Decision::Allow, |c| c.0);
                self.settle_ask(id, cx, |conn| conn.decide(id, decision));
            }
            Ask::Question {
                questions,
                picked,
                toggled,
                ..
            } => {
                let qi = picked.len();
                let Some(q) = questions.get(qi) else { return };
                if q.multi && i < q.options.len() {
                    // Several may be picked: toggle, and Done sends them.
                    if !toggled.remove(&i) {
                        toggled.insert(i);
                    }
                    self.ask_choice = i;
                    cx.notify();
                    return;
                }
                let answer = if q.multi {
                    let mut chosen: Vec<usize> = toggled.drain().collect();
                    chosen.sort_unstable();
                    chosen
                        .iter()
                        .filter_map(|&o| q.options.get(o))
                        .map(|o| o.label.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    q.options
                        .get(i)
                        .map(|o| o.label.clone())
                        .unwrap_or_default()
                };
                picked.push(answer);
                self.ask_choice = 0;
                self.finish_question(cx);
            }
        }
    }

    /// What the card on top asks: `Some(true)` an approval, `Some(false)`
    /// a question, `None` nothing.
    pub fn ask_is_permission(&self) -> Option<bool> {
        self.asks.first().map(|a| matches!(a, Ask::Permission { .. }))
    }

    /// Approve the request on top, from outside the chat (a notification's
    /// button); `always` also stops asking for the same in this chat.
    pub fn approve(&mut self, always: bool, cx: &mut Context<Self>) {
        if let Some(Ask::Permission { id, .. }) = self.asks.first() {
            let id = *id;
            let decision = if always { Decision::AllowAlways } else { Decision::Allow };
            self.settle_ask(id, cx, |conn| conn.decide(id, decision));
        }
    }

    pub fn deny(&mut self, cx: &mut Context<Self>) {
        if self.asks.first().is_some() {
            self.decline(cx);
        }
    }

    /// An answer typed in a notification: the reply to a question, or for
    /// an approval, what to do instead. Without a card up, a message.
    pub fn reply(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        if !self.answer_in_words(text.clone(), cx) {
            self.send(text, cx);
        }
    }

    /// Esc on the card: deny the action, or leave the question unanswered.
    fn decline(&mut self, cx: &mut Context<Self>) {
        let Some(ask) = self.asks.first() else { return };
        let id = ask.id();
        match ask {
            Ask::Permission { .. } => {
                self.settle_ask(id, cx, |conn| conn.decide(id, Decision::Deny))
            }
            Ask::Question { .. } => self.settle_ask(id, cx, |conn| conn.decide(id, Decision::Deny)),
        }
    }

    /// Text typed while a card is up answers it in words: for a question it
    /// is the answer; for an approval it means no, and the text goes to the
    /// agent as what to do instead.
    fn answer_in_words(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        // A choice's number picks it, as in Claude Code's own prompt.
        let choices = match self.asks.first() {
            Some(Ask::Permission { .. }) => PERMISSION_CHOICES.len(),
            Some(Ask::Question {
                questions, picked, ..
            }) => questions.get(picked.len()).map_or(0, |q| q.options.len()),
            None => return false,
        };
        if let Ok(n @ 1..) = text.parse::<usize>() {
            if n <= choices {
                self.choose(n - 1, cx);
                return true;
            }
        }
        let Some(ask) = self.asks.first_mut() else {
            return false;
        };
        match ask {
            Ask::Question { picked, .. } => {
                picked.push(text);
                self.finish_question(cx);
                true
            }
            Ask::Permission { id, .. } => {
                let id = *id;
                self.settle_ask(id, cx, |conn| conn.decide(id, Decision::Deny));
                false
            }
        }
    }

    /// Send a question's answers once each of its questions has one.
    fn finish_question(&mut self, cx: &mut Context<Self>) {
        let Some(Ask::Question {
            id,
            questions,
            picked,
            ..
        }) = self.asks.first()
        else {
            return;
        };
        if picked.len() < questions.len() {
            cx.notify();
            return;
        }
        let id = *id;
        let answers: Vec<(String, String)> = questions
            .iter()
            .map(|q| q.question.clone())
            .zip(picked.iter().cloned())
            .collect();
        self.settle_ask(id, cx, move |conn| conn.answer(id, answers));
    }

    fn settle_ask(
        &mut self,
        id: RequestId,
        cx: &mut Context<Self>,
        send: impl FnOnce(&Conn) -> bool,
    ) {
        if let Some(conn) = &self.conn {
            send(conn);
        }
        self.asks.retain(|a| a.id() != id);
        self.ask_choice = 0;
        if self.asks.is_empty() && self.status == ChatStatus::Waiting {
            self.status = ChatStatus::Working;
        }
        self.sync_popup(cx);
        cx.notify();
    }

    fn render_ask(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ask = self.asks.first()?;
        let more = self.asks.len() - 1;
        let agent = self.agent_name();
        let card = div()
            .flex()
            .flex_col()
            .gap(u(8.))
            .p(u(12.))
            .rounded(u(14.))
            .bg(rgb(theme::raised()))
            .border_1()
            .border_color(rgba(theme::strong_edge()))
            .text_size(u(theme::TEXT_UI));
        let option = |i: usize,
                      selected: bool,
                      label: String,
                      detail: Option<String>,
                      key: Option<&'static str>| {
            div()
                .id(ElementId::NamedInteger("ask-opt".into(), i as u64))
                .flex()
                .items_center()
                .gap(u(10.))
                .min_h(u(30.))
                .px(u(10.))
                .py(u(4.))
                .rounded(u(8.))
                .cursor_pointer()
                .when(selected, |d| {
                    d.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                })
                .when(!selected, |d| d.text_color(rgb(theme::text_2())))
                .hover(|s| s.bg(rgb(theme::surface())))
                .child(
                    div()
                        .flex_none()
                        .w(u(14.))
                        .font_family(theme::mono())
                        .text_size(u(11.))
                        .text_color(rgb(theme::dim()))
                        .child(format!("{}", i + 1)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().child(label))
                        .when_some(detail, |d, t| {
                            d.child(
                                div()
                                    .text_size(u(theme::TEXT_SMALL))
                                    .text_color(rgb(theme::dim()))
                                    .child(t),
                            )
                        }),
                )
                .when_some(key, |d, k| d.child(crate::kbd::key(k)))
                .on_click(cx.listener(move |c, _, _, cx| c.choose(i, cx)))
        };
        let card = match ask {
            Ask::Permission {
                action,
                input,
                reason,
                tool,
                ..
            } => {
                let (title, preview) = permission_preview(action, input, tool, &self.cwd);
                let card = card
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(8.))
                            .text_color(rgb(theme::text()))
                            .font_weight(FontWeight::MEDIUM)
                            .child(icon(Icon::Info, 14., theme::text()))
                            .child(div().flex_1().child(format!("{agent} wants to {title}")))
                            .when(more > 0, |d| {
                                d.child(
                                    div()
                                        .text_size(u(11.))
                                        .text_color(rgb(theme::dim()))
                                        .child(format!("+{more} more")),
                                )
                            }),
                    )
                    .when_some(reason.clone(), |d, r| {
                        d.child(div().text_color(rgb(theme::text_2())).child(r))
                    })
                    .when_some(preview, |d, p| d.child(p));
                // The answers as one row of buttons, the one the keys
                // would pick filled, and typing a reply offered beside them.
                let short = ["Yes", "Always allow", "No"];
                let mut buttons = div().flex().items_center().gap(u(6.));
                for (i, (_, _, key)) in PERMISSION_CHOICES.iter().enumerate() {
                    let selected = i == self.ask_choice;
                    buttons = buttons.child(
                        div()
                            .id(ElementId::NamedInteger("ask-opt".into(), i as u64))
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .h(u(28.))
                            .px(u(12.))
                            .rounded(u(7.))
                            .cursor_pointer()
                            .text_size(u(theme::TEXT_UI))
                            .when(selected, |d| {
                                d.bg(rgb(theme::text())).text_color(rgb(theme::pane())).hover(|s| s.opacity(0.9))
                            })
                            .when(!selected, |d| {
                                d.bg(rgb(theme::surface()))
                                    .text_color(rgb(theme::text_2()))
                                    .hover(|s| s.bg(rgb(theme::surface_hover())).text_color(rgb(theme::text())))
                            })
                            .child(short[i])
                            .when(!key.is_empty() && !selected, |d| d.child(crate::kbd::key(*key)))
                            .on_click(cx.listener(move |c, _, _, cx| c.choose(i, cx))),
                    );
                }
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(12.))
                        .pt(u(2.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(u(theme::TEXT_SMALL))
                                .text_color(rgb(theme::dim()))
                                .child("or type what to do instead"),
                        )
                        .child(buttons),
                )
            }
            Ask::Question {
                questions,
                picked,
                toggled,
                ..
            } => {
                let qi = picked.len().min(questions.len().saturating_sub(1));
                let q = &questions[qi];
                let mut card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .text_color(rgb(theme::text()))
                        .font_weight(FontWeight::MEDIUM)
                        .child(icon(Icon::Info, 14., theme::text()))
                        .child(div().flex_1().child(q.question.clone()))
                        .when(!q.header.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(u(11.))
                                    .text_color(rgb(theme::dim()))
                                    .child(q.header.clone()),
                            )
                        })
                        .when(questions.len() > 1, |d| {
                            d.child(
                                div()
                                    .text_size(u(11.))
                                    .text_color(rgb(theme::dim()))
                                    .child(format!("{} of {}", qi + 1, questions.len())),
                            )
                        }),
                );
                for (i, o) in q.options.iter().enumerate() {
                    let label = if q.multi {
                        format!(
                            "{} {}",
                            if toggled.contains(&i) { "☑" } else { "☐" },
                            o.label
                        )
                    } else {
                        o.label.clone()
                    };
                    let detail = (!o.description.is_empty()).then(|| o.description.clone());
                    card = card.child(option(i, i == self.ask_choice, label, detail, None));
                }
                if q.multi {
                    let i = q.options.len();
                    card = card.child(option(
                        i,
                        i == self.ask_choice,
                        "Done".into(),
                        None,
                        Some("⏎"),
                    ));
                }
                card.child(div().text_size(u(11.)).text_color(rgb(theme::dim())).child(
                    crate::kbd::hints("↑↓ choose · ⏎ answer · or type your own answer · esc skip"),
                ))
            }
        };
        Some(card.into_any_element())
    }

    fn switch_agent(&mut self, to: AgentId, model: Option<smol_str::SmolStr>, cx: &mut Context<Self>) {
        self.model_menu = None;
        if to == self.agent {
            cx.notify();
            return;
        }
        self.agent = to;
        // The old agent's session id and model mean nothing to the new one.
        self.native_id = None;
        self.model = model;
        self.ran_model = None;
        // A fresh backend that is handed our transcript, so the new agent
        // picks up the thread; the handoff row says how faithfully.
        if !self.transcript.messages.is_empty() {
            let carry = self.transcript.clone();
            self.start(Some(carry), None, cx);
        } else if let Some(old) = self.conn.take() {
            old.shutdown();
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Where a new thread starts (Settings › Agents), set before its first
    /// turn so nothing needs a restart.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// The agent's own list of models, if it has one.
    fn models(&self) -> &'static [agent_core::ModelSpec] {
        self.env.registry.get(&self.agent).map(|e| e.adapter.models()).unwrap_or(&[])
    }

    /// Run on `model` from here on. It is a launch flag, so a running
    /// session restarts, resuming the agent's own conversation.
    fn choose_model(&mut self, model: Option<smol_str::SmolStr>, cx: &mut Context<Self>) {
        self.model_menu = None;
        if model == self.model {
            cx.notify();
            return;
        }
        self.model = model;
        if self.conn.is_some() && !self.busy() {
            let carry = self.transcript.clone();
            self.start_carrying(&carry, cx);
        } else if let Some(old) = self.conn.take().filter(|_| !self.busy()) {
            old.shutdown();
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Start the agent before the first message: Claude Code takes seconds
    /// to boot (its hooks, its MCP servers), which otherwise all comes out
    /// of the wait for the first reply. Done once, when you click in.
    fn warm(&mut self, cx: &mut Context<Self>) {
        if self.warmed || self.conn.is_some() {
            return;
        }
        self.warmed = true;
        let ready = self.env.registry.get(&self.agent).is_some_and(|e| e.is_ready());
        if !ready {
            return;
        }
        let prior = self.transcript.clone();
        self.start_carrying(&prior, cx);
    }

    /// Open the access menu on the current mode, or shut it.
    fn toggle_access_menu(&mut self, cx: &mut Context<Self>) {
        // One popup above the composer at a time.
        self.model_menu = None;
        self.access_menu = match self.access_menu {
            Some(_) => None,
            None => Access::ALL
                .iter()
                .position(|a| *a == Access::of(self.mode))
                .or(Some(0)),
        };
        self.sync_popup(cx);
        cx.notify();
    }

    fn close_access_menu(&mut self, cx: &mut Context<Self>) {
        if self.access_menu.take().is_some() {
            self.sync_popup(cx);
            cx.notify();
        }
    }

    /// Picked from the menu. Choosing a mode used to mean clicking through
    /// the others, with a restart of the agent at each.
    fn set_access(&mut self, access: Access, cx: &mut Context<Self>) {
        self.access_menu = None;
        self.sync_popup(cx);
        let mode = access.mode();
        if mode != self.mode {
            self.mode = mode;
            // The mode is a launch flag: restart with it, resuming the
            // agent's own session when it can so nothing is replayed.
            if self.conn.is_some() {
                let carry = self.transcript.clone();
                self.start_carrying(&carry, cx);
            }
            self.schedule_save(cx);
        }
        cx.notify();
    }

    /// The three modes above the chip, each with what it allows.
    fn render_access_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let lit = self.access_menu?;
        let current = Access::of(self.mode);
        let mut menu = div()
            .id("access-menu")
            .absolute()
            .left_0()
            .bottom(relative(1.))
            .mb(u(6.))
            .w(u(300.))
            .p(u(4.))
            .flex()
            .flex_col()
            .gap(u(2.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .occlude()
            .cursor_pointer();
        for (i, a) in Access::ALL.iter().copied().enumerate() {
            let on = a == current;
            menu = menu.child(
                div()
                    .id(ElementId::NamedInteger("access".into(), i as u64))
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .px(u(10.))
                    .py(u(7.))
                    .rounded(u(6.))
                    .when(i == lit, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(
                        access_icon(a),
                        14.,
                        if on { theme::text() } else { theme::text_2() },
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(u(13.))
                                    .line_height(u(18.))
                                    .text_color(rgb(theme::text()))
                                    .child(a.name()),
                            )
                            .child(
                                div()
                                    .text_size(u(11.))
                                    .line_height(u(15.))
                                    .text_color(rgb(theme::dim()))
                                    .child(a.detail()),
                            ),
                    )
                    .when(on, |d| d.child(icon(Icon::Check, 13., theme::text())))
                    // On press, not click: the menu hangs outside the chip's
                    // bounds, so the press already closed it (as a press
                    // outside), and by release this row would be gone.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |c, _, _, cx| c.set_access(a, cx)),
                    ),
            );
        }
        Some(gpui::deferred(menu).with_priority(1).into_any_element())
    }

    fn apply(&mut self, sid: SessionId, batch: Vec<DriverEvent>, cx: &mut Context<Self>) {
        if sid != self.session_id {
            return;
        }
        let was = self.status;
        let before = self.transcript.messages.len();
        let mut touched = Vec::new();
        let mut failed_resume = false;
        for ev in batch {
            match ev {
                DriverEvent::Agent(ev) => {
                    match &ev {
                        AgentEvent::Started {
                            native_session_id,
                            model,
                        } => {
                            if let Some(id) = native_session_id {
                                self.native_id = Some(id.clone());
                            }
                            self.ran_model = model.as_ref().map(|m| m.to_string());
                        }
                        AgentEvent::Fatal { message, .. } => {
                            failed_resume |= self.resuming.is_some();
                            self.error = Some(message.clone().into());
                            self.status = ChatStatus::Failed;
                        }
                        AgentEvent::ToolCall { id, .. } => {
                            self.resuming = None;
                            if self.asks.is_empty() {
                                self.status = ChatStatus::Working;
                            }
                            self.tool_times
                                .entry(id.0.to_string())
                                .or_insert((Instant::now(), None));
                        }
                        AgentEvent::ToolResult { id, .. } => {
                            if let Some((start, end)) = self.tool_times.get_mut(id.0.as_str()) {
                                end.get_or_insert(start.elapsed());
                            }
                        }
                        AgentEvent::Permission {
                            id,
                            action,
                            input,
                            reason,
                            tool_use,
                            ..
                        } => {
                            let tool = match action {
                                Action::Other { name, .. } => name.to_string(),
                                _ => String::new(),
                            };
                            self.asks.push(Ask::Permission {
                                id: *id,
                                action: action.clone(),
                                input: input.clone(),
                                reason: reason.clone(),
                                tool,
                                tool_use: tool_use.clone(),
                            });
                            self.status = ChatStatus::Waiting;
                        }
                        AgentEvent::Question {
                            id,
                            tool_use,
                            questions,
                        } if !questions.is_empty() => {
                            self.asks.push(Ask::Question {
                                id: *id,
                                tool_use: tool_use.clone(),
                                questions: questions.clone(),
                                picked: Vec::new(),
                                toggled: HashSet::new(),
                            });
                            self.status = ChatStatus::Waiting;
                        }
                        AgentEvent::AskCancelled { id } => {
                            self.asks.retain(|a| a.id() != *id);
                            if self.asks.is_empty() && self.status == ChatStatus::Waiting {
                                self.status = ChatStatus::Working;
                            }
                        }
                        AgentEvent::UserDelivered { id } => {
                            if let Some(i) = self.inflight.iter().position(|(q, _)| q == id) {
                                let (_, out) = self.inflight.remove(i);
                                self.deliver(out, &mut touched);
                            }
                        }
                        // Dropped unread: it goes back into the composer.
                        AgentEvent::UserDropped { id } => {
                            if let Some(i) = self.inflight.iter().position(|(q, _)| q == id) {
                                let (_, out) = self.inflight.remove(i);
                                self.return_to_composer(out, cx);
                            }
                        }
                        AgentEvent::Text { .. } | AgentEvent::Reasoning { .. } => {
                            // The resumed session answered, so it took.
                            self.resuming = None;
                            if self.asks.is_empty() {
                                self.status = ChatStatus::Working;
                            }
                        }
                        AgentEvent::TurnFailed { status, message } => {
                            self.failed = Some((*status, message.clone()));
                        }
                        AgentEvent::TurnEnd { stop } => {
                            self.resuming = None;
                            if stop.is_clean() {
                                self.failed = None;
                            }
                            // Whatever it was waiting on no longer matters.
                            self.asks.clear();
                            // Claude reports a turn stopped by Esc as an error.
                            let stopped = std::mem::take(&mut self.interrupting);
                            let stop = if stopped && *stop == StopReason::Error {
                                &StopReason::Interrupted
                            } else {
                                stop
                            };
                            self.status = if !self.inflight.is_empty() {
                                // It still holds messages it will read next,
                                // in a turn of their own.
                                ChatStatus::Working
                            } else if stop.is_clean() || *stop == StopReason::Interrupted {
                                ChatStatus::Idle
                            } else {
                                ChatStatus::Failed
                            };
                            if let Some(started) = self.turn_started.take() {
                                if let Some(ix) = self
                                    .transcript
                                    .messages
                                    .iter()
                                    .rposition(|m| m.role == Role::Assistant)
                                {
                                    self.worked.insert(ix, started.elapsed());
                                    touched.push(ix);
                                }
                            }
                            if self.status == ChatStatus::Working {
                                self.turn_started = Some(Instant::now());
                            }
                        }
                        _ => {}
                    }
                    let applied = self.transcript.apply(&self.agent, &ev, now());
                    if applied.kind != AppliedKind::Ignored {
                        if let Some(ix) = applied.message {
                            touched.push(ix);
                        }
                    }
                }
                DriverEvent::Hydrated(mode) => {
                    if !mode.is_lossless() && !self.transcript.messages.is_empty() {
                        self.transcript
                            .push_handoff(None, self.agent.clone(), mode, now());
                    }
                    self.status = ChatStatus::Idle;
                }
                DriverEvent::Ended { error } => {
                    self.asks.clear();
                    // The session died holding messages it never read: deck
                    // sends them to the next one.
                    for (_, out) in self.inflight.drain(..) {
                        self.queued.push_back(out);
                    }
                    if let Some(e) = error {
                        failed_resume |= self.resuming.is_some();
                        self.error = Some(e.into());
                        self.status = ChatStatus::Failed;
                    } else if self.busy() {
                        self.status = ChatStatus::Idle;
                    }
                    self.conn = None;
                }
            }
        }
        // A turn that went through resets the retries; one that just died
        // may get one.
        if self.status == ChatStatus::Idle && was != ChatStatus::Idle {
            self.retries = 0;
        }
        if self.status == ChatStatus::Failed && was != ChatStatus::Failed && !failed_resume {
            self.maybe_retry(cx);
        }
        // The saved session could not be resumed (expired, or made on another
        // machine): replay the thread into a fresh one and send the turn again.
        if failed_resume {
            if let Some(turn) = self.resuming.take() {
                tracing::warn!("resume failed; replaying the thread instead");
                self.native_id = None;
                let mut prior = self.transcript.clone();
                if let Some(i) = prior.messages.iter().rposition(|m| m.role == Role::User) {
                    prior.messages.truncate(i);
                }
                if self.start(Some(prior), None, cx)
                    && self.conn.as_ref().is_some_and(|c| c.send(turn))
                {
                    self.error = None;
                    self.status = ChatStatus::Working;
                }
            }
        }
        self.sync_popup(cx);
        self.last_activity = Instant::now();
        self.updated = now();
        self.sync_list(before, &touched, cx);
        // The turn is over: the next queued message goes now.
        if self.status == ChatStatus::Idle {
            if let Some(next) = self.queued.pop_front() {
                self.send_turn(next, cx);
            }
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// The agent read a message sent mid-turn: close the reply it was
    /// writing (with its time so far), and put the message in the thread at
    /// that point, so the rest of the turn reads as the answer to it.
    fn deliver(&mut self, out: Outgoing, touched: &mut Vec<usize>) {
        if let Some(ix) = self.transcript.messages.len().checked_sub(1) {
            let last = &mut self.transcript.messages[ix];
            if last.role == Role::Assistant && last.stop.is_none() {
                for b in &mut last.blocks {
                    if let Block::Text { open, .. } | Block::Reasoning { open, .. } = b {
                        *open = false;
                    }
                }
                last.stop = Some(StopReason::EndTurn);
                if let Some(started) = self.turn_started {
                    self.worked.insert(ix, started.elapsed());
                }
                touched.push(ix);
            }
        }
        self.transcript
            .push_user(out.text.clone(), out.blocks(), now());
        self.turn_started = Some(Instant::now());
        self.status = ChatStatus::Working;
    }

    /// A queued message the agent dropped comes back to the composer, ahead
    /// of anything typed since, with its attachments.
    fn return_to_composer(&mut self, out: Outgoing, cx: &mut Context<Self>) {
        for p in out.attachments.into_iter().rev() {
            if !self.attachments.contains(&p) {
                self.attachments.insert(0, p);
            }
        }
        self.composer.update(cx, |f, cx| {
            let draft = f.text().trim().to_string();
            let text = if draft.is_empty() {
                out.text
            } else {
                format!("{}\n\n{draft}", out.text)
            };
            f.set_text(&text, cx);
        });
    }

    /// Re-measure only what changed: touched rows, plus any new ones.
    fn sync_list(&mut self, before: usize, touched: &[usize], cx: &mut Context<Self>) {
        let now_len = self.transcript.messages.len();
        let mut seen = HashSet::new();
        for &ix in touched {
            if ix < before && seen.insert(ix) {
                self.list.splice(ix..ix + 1, 1);
            }
        }
        if now_len > before {
            self.list.splice(before..before, now_len - before);
            if self.paused {
                self.unread += now_len - before;
            }
        }
        cx.notify();
    }

    /// Scrolled down into a reply: a slim bar over the thread with the
    /// message it answers; a click goes back up to that message.
    /// The console: the thread's commands, oldest first, each as `$ cmd`
    /// and what it printed, the running one streaming. Follows the newest
    /// output while something runs.
    fn render_console(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.console {
            return None;
        }
        const KEEP: usize = 30;
        const TAIL: usize = 60;
        let cmds: Vec<&ToolInvocation> = self
            .transcript
            .messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                Block::Tool(t) if tool_kind(&t.name) == Kind::Command => Some(t),
                _ => None,
            })
            .collect();
        let start = cmds.len().saturating_sub(KEEP);
        let running = cmds.iter().any(|t| t.is_open());
        let mut body = div()
            .id("console-body")
            .flex()
            .flex_col()
            .gap(u(14.))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.console_scroll)
            .px(u(12.))
            .py(u(10.))
            .font_family(theme::mono())
            .text_size(u(theme::TEXT_SMALL))
            .line_height(u(17.));
        if cmds.is_empty() {
            body = body.child(div().text_color(rgb(theme::dim())).child("No commands in this chat yet."));
        }
        for t in &cmds[start..] {
            let (_, cmd) = step_phrase_full(t, t.is_open(), &self.cwd);
            let out = match &t.result {
                None => t.progress.clone(),
                Some(ToolOutput::Terminal { text, .. }) | Some(ToolOutput::Text(text)) => text.clone(),
                Some(ToolOutput::Truncated { preview, .. }) => preview.clone(),
                Some(other) => format!("{other:?}"),
            };
            let lines: Vec<&str> = out.lines().collect();
            let skipped = lines.len().saturating_sub(TAIL);
            let took = self
                .tool_times
                .get(&t.id.0.to_string())
                .map(|(start, end)| end.unwrap_or_else(|| start.elapsed()));
            let (state, tone) = if t.is_open() {
                ("running".to_string(), theme::accent())
            } else if t.is_error {
                ("failed".to_string(), crate::theme::DANGER)
            } else {
                (took.map(|d| human_duration(d)).unwrap_or_default(), theme::dim())
            };
            // The command on a tinted bar, so each entry starts plainly.
            let head = div()
                .flex()
                .items_start()
                .gap(u(8.))
                .px(u(8.))
                .py(u(4.))
                .rounded(u(6.))
                .bg(rgb(theme::surface()))
                .child(div().flex_none().text_color(rgb(theme::accent())).child("$"))
                .child(div().flex_1().min_w_0().text_color(rgb(theme::text())).child(cmd))
                .when(!state.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(theme::ui())
                            .text_size(u(11.))
                            .text_color(rgb(tone))
                            .child(state),
                    )
                });
            // What it printed, line by line under a rule, quieter than the
            // command; the oldest lines of a long run are left out.
            let mut output = div()
                .flex()
                .flex_col()
                .ml(u(12.))
                .pl(u(10.))
                .border_l_1()
                .border_color(rgba(theme::hairline()))
                .text_size(u(11.))
                .line_height(u(16.))
                .text_color(rgb(theme::text_2()));
            if skipped > 0 {
                output = output.child(div().text_color(rgb(theme::dim())).child(format!("… {skipped} earlier lines")));
            }
            for line in &lines[skipped..] {
                // An empty line keeps its height.
                output = output.child(div().min_h(u(16.)).child(line.to_string()));
            }
            if lines.iter().all(|l| l.trim().is_empty()) {
                output = output.child(
                    div().text_color(rgb(theme::dim())).child(if t.is_open() { "waiting for output…" } else { "no output" }),
                );
            }
            body = body.child(div().flex().flex_col().gap(u(6.)).child(head).child(output));
        }
        if running {
            self.console_scroll.scroll_to_bottom();
        }
        Some(
            Self::column(
                div()
                    .flex()
                    .flex_col()
                    .h(u(300.))
                    .mb(u(8.))
                    .rounded(u(12.))
                    .bg(rgb(theme::raised()))
                    .border_1()
                    .border_color(rgba(theme::hairline()))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(u(8.))
                            .h(u(30.))
                            .px(u(12.))
                            .border_b_1()
                            .border_color(rgba(theme::hairline()))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text_2()))
                            .child(icon(Icon::Terminal, 12., theme::dim()))
                            .child(div().flex_1().child("Console"))
                            .child(
                                div()
                                    .id("console-close")
                                    .cursor_pointer()
                                    .rounded(u(4.))
                                    .hover(|s| s.bg(rgb(theme::surface())))
                                    .child(icon(Icon::Close, 12., theme::dim()))
                                    .on_click(cx.listener(|c, _, _, cx| {
                                        c.console = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(body)
                    .into_any_element(),
            )
            .into_any_element(),
        )
    }

    /// The thread so far in a few lines, for the prompt rewriter: each of
    /// your messages and the first line of the answer to it.
    fn history_brief(&self) -> String {
        let mut out = String::new();
        for m in &self.transcript.messages {
            let text = message_text(m);
            let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) else { continue };
            let first: String = first.chars().take(160).collect();
            match m.role {
                Role::User => out.push_str(&format!("me: {first}\n")),
                Role::Assistant => out.push_str(&format!("agent: {first}\n")),
            }
        }
        out
    }

    /// Every message you sent in this thread: its row in the list and its
    /// first line. The sidebar shows these under the open chat.
    pub fn outline(&self) -> Vec<(usize, String)> {
        self.transcript
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .filter_map(|(ix, m)| {
                let text = message_text(m);
                let first = text.lines().find(|l| !l.trim().is_empty())?.trim().chars().take(80).collect::<String>();
                Some((ix, first))
            })
            .collect()
    }

    /// The message of yours the reader is in now: the last one at or above
    /// the top of the view.
    pub fn outline_at(&self) -> Option<usize> {
        let top = self.list.logical_scroll_top().item_ix;
        let msgs = &self.transcript.messages;
        msgs.get(..=top.min(msgs.len().saturating_sub(1)))?.iter().rposition(|m| m.role == Role::User)
    }

    /// Bring message `ix` to the top of the view, and stop following new
    /// output so it stays there.
    pub fn jump_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.paused = true;
        self.list.scroll_to(gpui::ListOffset { item_ix: ix, offset_in_item: px(0.) });
        cx.notify();
    }

    /// The outline on the right edge: your messages, the one you are in
    /// marked; a click jumps to it.
    fn render_outline(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.outline_open {
            return None;
        }
        let items = self.outline();
        let at = self.outline_at();
        let mut col = div()
            .id("outline")
            .flex()
            .flex_col()
            .gap(u(2.))
            .p(u(6.))
            .w(u(240.))
            .max_h(u(420.))
            .overflow_y_scroll()
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .h(u(24.))
                    .px(u(8.))
                    .flex()
                    .items_center()
                    .text_size(u(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::dim()))
                    .child(format!("Outline · {}", items.len())),
            );
        for (n, (ix, text)) in items.into_iter().enumerate() {
            let here = at == Some(ix);
            col = col.child(
                div()
                    .id(ElementId::NamedInteger("outline-item".into(), ix as u64))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(28.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .cursor_pointer()
                    .text_size(u(theme::TEXT_UI))
                    .text_color(rgb(if here { theme::text() } else { theme::text_2() }))
                    .when(here, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                    .child(div().flex_none().w(u(16.)).text_size(u(11.)).text_color(rgb(theme::dim())).child(format!("{}", n + 1)))
                    .child(div().flex_1().min_w_0().one_line().child(text))
                    .on_click(cx.listener(move |c, _, _, cx| c.jump_to(ix, cx))),
            );
        }
        Some(
            div()
                .absolute()
                .top(u(8.))
                .right(u(12.))
                .child(col)
                .into_any_element(),
        )
    }

    fn render_sticky_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let msgs = &self.transcript.messages;
        if msgs.len() < 3 {
            return None;
        }
        let raw = self.list.logical_scroll_top().item_ix;
        if raw >= msgs.len() {
            return None;
        }
        let top = raw;
        let asked = msgs[..=top].iter().rposition(|m| m.role == Role::User)?;
        // The question itself is on screen: nothing to remind of.
        if asked == top {
            return None;
        }
        // The next row comes in under where the bar sits (a message
        // arriving at the top as you scroll): the bar would cover it, and
        // when it is the question itself, say it twice.
        let band = self.list.viewport_bounds().top() + px(48.);
        if self.list.bounds_for_item(top + 1).is_some_and(|b| b.top() < band) {
            return None;
        }
        let text = message_text(&msgs[asked]);
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
        if first.is_empty() {
            return None;
        }
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .px(u(16.))
                .pt(u(8.))
                .child(
                    // A small pill sized to the question, floating over the
                    // thread: what this part answers, and a way back to it.
                    div()
                        .id("sticky-prompt")
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .max_w(u(520.))
                        .min_w_0()
                        .h(u(28.))
                        .pl(u(10.))
                        .pr(u(12.))
                        .rounded_full()
                        .bg(rgb(theme::menu()))
                        .border_1()
                        .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                        .shadow_md()
                        .text_size(u(12.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::raised())))
                        .child(icon(Icon::ArrowUp, 11., theme::dim()))
                        .child(div().min_w_0().one_line().text_color(rgb(theme::text_2())).child(first))
                        .on_click(cx.listener(move |c, _, _, cx| {
                            c.list.scroll_to(gpui::ListOffset { item_ix: asked, offset_in_item: px(0.) });
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// Something you clicked is about to open or fold: keep what you are
    /// reading where it is. The list is pinned to the bottom while it
    /// follows, so a log opening grew upward and slid the row from under
    /// the pointer; anchored at the top, it grows downward instead.
    fn hold_place(&mut self) {
        let top = self.list.logical_scroll_top();
        self.paused = true;
        self.list.scroll_to(top);
    }

    /// Back to the end of the thread, following again.
    fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list.reset(self.transcript.messages.len());
        self.paused = false;
        self.unread = 0;
        cx.notify();
    }

    /// While scrolled up: a pill over the composer with what is new, a
    /// click from the end.
    fn render_jump(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.paused {
            return None;
        }
        let label = match self.unread {
            0 => "Jump to latest".to_string(),
            1 => "1 new message".to_string(),
            n => format!("{n} new messages"),
        };
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(u(-40.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("jump-latest")
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .h(u(28.))
                        .px(u(12.))
                        .rounded_full()
                        .bg(rgb(theme::menu()))
                        .border_1()
                        .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                        .text_size(u(12.))
                        .text_color(rgb(if self.unread > 0 { theme::text() } else { theme::text_2() }))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::text())))
                        .child(icon(Icon::ArrowUp, 12., theme::text_2()).into_any_element())
                        .child(label)
                        .on_click(cx.listener(|c, _, _, cx| c.jump_to_latest(cx)))
                        .with_animation(
                            "jump-in",
                            Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                            |d, t| d.opacity(t),
                        ),
                )
                .into_any_element(),
        )
    }

    // -- rendering ----------------------------------------------------------

    /// Everything readable sits in one centred column: long lines are hard to
    /// read, and the composer shares the column's edges with the messages.
    fn column(content: impl IntoElement) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(div().w_full().max_w(u(COLUMN)).px(u(24.)).child(content))
    }

    /// Right-click on a message: copy all of its text, as Copy under it does.
    /// The right-click menu on a message. Your own messages (`mine` is the
    /// message's place) can also take the thread back to before them.
    fn open_message_menu(
        &mut self,
        text: String,
        mine: Option<usize>,
        at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut entries = vec![Item::new("Copy Message", move |_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()))
        })
        .icon(Icon::Copy)
        .into()];
        if let Some(ix) = mine {
            let chat = cx.entity().downgrade();
            let files = self.checkpoints.contains_key(&ix);
            entries.push(Entry::Separator);
            entries.push(
                Item::new("Edit from Here", {
                    let chat = chat.clone();
                    move |_, cx| {
                        let _ = chat.update(cx, |c, cx| c.rewind(ix, true, cx));
                    }
                })
                .icon(Icon::Pencil)
                .into(),
            );
            entries.push(
                Item::new(
                    if files {
                        "Undo to Here (Files and Chat)"
                    } else {
                        "Undo to Here"
                    },
                    {
                        let chat = chat.clone();
                        move |_, cx| {
                            let _ = chat.update(cx, |c, cx| c.rewind(ix, false, cx));
                        }
                    },
                )
                .icon(Icon::Refresh)
                .into(),
            );
        }
        if self.undo_rewind.is_some() {
            let chat = cx.entity().downgrade();
            entries.push(
                Item::new("Put Files Back", move |_, cx| {
                    let _ = chat.update(cx, |c, cx| c.put_files_back(cx));
                })
                .icon(Icon::ArrowUpRight)
                .into(),
            );
        }
        let restore = Some(self.composer.read(cx).focus.clone());
        self.menu = Some(menu::open(
            entries,
            at,
            restore,
            window,
            cx,
            |c: &mut Self, _, _| c.menu = None,
        ));
        cx.notify();
    }

    fn render_message(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.sel_msg.set(ix);
        self.sel_unit.set(0);
        let el = self.render_message_body(ix, window, cx);
        // Pieces it no longer has (a reply that re-rendered shorter) go.
        let n = self.sel_unit.get();
        let mut units = self.units.borrow_mut();
        let gone: Vec<select::Key> = units.range((ix, n)..(ix + 1, 0)).map(|(k, _)| *k).collect();
        for k in gone {
            units.remove(&k);
        }
        el
    }

    fn render_message_body(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(msg) = self.transcript.messages.get(ix) else {
            return div().into_any_element();
        };
        match msg.role {
            Role::User => {
                let text = message_text(msg);
                let files: Vec<PathBuf> = msg
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Attachment { path, .. } => Some(PathBuf::from(path)),
                        _ => None,
                    })
                    .collect();
                let mut col = div().flex().flex_col().items_end().gap(u(6.)).pt(u(24.));
                if !files.is_empty() {
                    col = col.child(self.render_attachments(
                        &format!("sent-{ix}"),
                        &files,
                        false,
                        cx,
                    ));
                }
                let waiting = ix + 1 == self.transcript.messages.len() && self.busy();
                if !text.is_empty() {
                    let copy = text.clone();
                    col = col.child(
                        div()
                            .max_w(relative(0.8))
                            .px(u(14.))
                            .py(u(9.))
                            .rounded(u(18.))
                            .bg(rgb(theme::surface()))
                            .text_color(rgb(theme::text()))
                            .on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(move |c, ev: &gpui::MouseDownEvent, window, cx| {
                                    c.open_message_menu(
                                        copy.clone(),
                                        Some(ix),
                                        ev.position,
                                        window,
                                        cx,
                                    )
                                }),
                            )
                            .child({
                                let runs = plain_runs(text.len(), theme::ui(), theme::text());
                                let (t, rec) = self.unit_text(text.into(), runs, "\n\n", "");
                                selectable(t.into_any_element(), rec)
                            }),
                    );
                }
                // Sent, and nothing back yet: say it is being worked on.
                if waiting {
                    col = col.child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .pt(u(18.))
                            .h(u(44.))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text_2()))
                            .child(spinner(
                                ElementId::Name(format!("wait-spin-{ix}").into()),
                                12.,
                                theme::text_2(),
                            ))
                            .child(working_label(format!("wait-{ix}"), "Working…".into())),
                    );
                }
                Self::column(col).into_any_element()
            }
            Role::Assistant => {
                let blocks = msg.blocks.clone();
                let text = message_text(msg);
                let last = ix + 1 == self.transcript.messages.len();
                let live = last && self.busy();
                let worked = self.worked.get(&ix).copied();
                let group = SharedString::from(format!("msg-{ix}"));

                // Text stands alone; consecutive tool calls and thinking fold
                // into one work log, which is how a turn reads as a story.
                let mut parts: Vec<Part> = Vec::new();
                for (bix, b) in blocks.iter().enumerate() {
                    match b {
                        Block::Tool(_) | Block::Reasoning { .. } => match parts.last_mut() {
                            Some(Part::Work(v)) => v.push(bix),
                            _ => parts.push(Part::Work(vec![bix])),
                        },
                        _ => parts.push(Part::One(bix)),
                    }
                }
                let last_work = parts.iter().rposition(|p| matches!(p, Part::Work(_)));

                let mut col = div().flex().flex_col().gap(u(12.)).pt(u(24.));
                if !text.is_empty() {
                    let copy = text.clone();
                    col = col.on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |c, ev: &gpui::MouseDownEvent, window, cx| {
                            c.open_message_menu(copy.clone(), None, ev.position, window, cx)
                        }),
                    );
                }
                for (pi, part) in parts.iter().enumerate() {
                    col = col.child(match part {
                        Part::One(bix) => self.render_block(ix, *bix, &blocks[*bix], cx),
                        Part::Work(bixes) => {
                            let duration = worked.filter(|_| Some(pi) == last_work);
                            let running = live && Some(pi) == last_work && pi + 1 == parts.len();
                            self.render_work_log(ix, pi, bixes, &blocks, running, duration, cx)
                        }
                    });
                }
                // Working, but nothing new for a few seconds and no step
                // turning: say it is thinking, and for how long, so it never
                // looks stuck.
                let quiet = self.last_activity.elapsed();
                let stepping = matches!(parts.last(), Some(Part::Work(_)));
                if live && !parts.is_empty() && !stepping && quiet >= Duration::from_secs(3) {
                    col = col.child(rise_in(
                        format!("quiet-{ix}"),
                        div()
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .h(u(24.))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text_2()))
                            .child(crate::icon::thinking(
                                ElementId::Name(format!("quiet-spin-{ix}").into()),
                                Icon::agent(self.agent.as_str()),
                                13.,
                                theme::text_2(),
                            ))
                            .child(working_label(
                                format!("quiet-label-{ix}"),
                                format!("Thinking · {}", human_duration(quiet)).into(),
                            ))
                            .into_any_element(),
                    ));
                }
                // The agent is working but has not said or done anything yet.
                if live && parts.is_empty() {
                    col = col.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .h(u(26.))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text_2()))
                            .child(spinner(
                                ElementId::Name(format!("pending-spin-{ix}").into()),
                                12.,
                                theme::text_2(),
                            ))
                            .child(working_label(format!("pending-{ix}"), "Working…".into())),
                    );
                }
                // Once the turn is over, what it changed, file by file.
                if !live {
                    if let Some(card) = self.render_changed_files(ix, &blocks, cx) {
                        col = col.child(card);
                    }
                }
                if !live && !text.is_empty() {
                    let key = format!("msg-{ix}");
                    let copied = self.copied.as_deref() == Some(key.as_str());
                    col = col.child(
                        div().flex().items_center().gap(u(4.)).h(u(24.)).child(
                            div()
                                .id(ElementId::NamedInteger("copy".into(), ix as u64))
                                .flex()
                                .items_center()
                                .gap(u(5.))
                                .h(u(24.))
                                .px(u(6.))
                                .rounded(u(6.))
                                .text_size(u(11.))
                                .text_color(rgb(theme::dim()))
                                .opacity(if copied { 1. } else { 0. })
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .cursor_pointer()
                                .hover(|s| {
                                    s.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                                })
                                .child(icon(
                                    if copied { Icon::Check } else { Icon::Copy },
                                    12.,
                                    theme::dim(),
                                ))
                                .child(if copied { "Copied" } else { "Copy" })
                                .on_click(cx.listener(move |c, _, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        text.clone(),
                                    ));
                                    c.copied = Some(key.clone());
                                    cx.notify();
                                })),
                        ),
                    );
                }
                Self::column(div().group(group).child(col)).into_any_element()
            }
        }
    }

    fn render_block(
        &mut self,
        ix: usize,
        bix: usize,
        block: &Block,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match block {
            Block::Text { text, open } => {
                let shown = self.reveal_prefix((ix, bix), text, *open);
                let caret = shown.len() < text.len() || (*open && self.busy());
                let doc = self
                    .docs
                    .entry((ix, bix))
                    .or_default()
                    .update(shown)
                    .clone();
                let doc = self.render_doc(&format!("{ix}-{bix}"), &doc, cx);
                if !caret {
                    return doc;
                }
                // A soft pulse where the text is still coming out.
                div()
                    .flex()
                    .flex_col()
                    .child(doc)
                    .child(
                        div()
                            .mt(u(6.))
                            .size(u(7.))
                            .rounded_full()
                            .bg(rgb(theme::text_2()))
                            .opacity({
                                let t = gpui::ease_in_out(crate::icon::phase(900));
                                0.25 + 0.75 * (1. - (t * 2. - 1.).abs())
                            }),
                    )
                    .into_any_element()
            }
            Block::Handoff { to, mode, .. } => div()
                .text_size(u(12.))
                .text_color(rgb(theme::dim()))
                .child(format!(
                    "Continued with {} ({})",
                    to.as_str(),
                    handoff_label(*mode)
                ))
                .into_any_element(),
            Block::Attachment { path, .. } => div()
                .text_size(u(12.))
                .text_color(rgb(theme::text_2()))
                .child(path.clone())
                .into_any_element(),
            // Tools and thinking are drawn by the work log.
            Block::Tool(_) | Block::Reasoning { .. } => div().into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_work_log(
        &mut self,
        ix: usize,
        pi: usize,
        bixes: &[usize],
        blocks: &[Block],
        running: bool,
        duration: Option<Duration>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (ix, pi);
        // Open while the agent is working in it, folded once it is done;
        // a click overrides either way.
        let open = self.log_open.get(&key).copied().unwrap_or(running);
        let tools: Vec<&ToolInvocation> = bixes
            .iter()
            .filter_map(|&b| match &blocks[b] {
                Block::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        let failed = tools.iter().filter(|t| t.is_error).count();
        let (added, removed) = tools
            .iter()
            .filter_map(|t| edit_stats(t))
            .fold((0, 0), |(a, r), (x, y)| (a + x, r + y));
        let label: SharedString = match (running, duration) {
            (true, _) => match self
                .turn_started
                .map(|t| t.elapsed())
                .filter(|d| d.as_secs() >= 1)
            {
                Some(d) => format!("Working for {}", human_duration(d)).into(),
                None => "Working…".into(),
            },
            (false, Some(d)) => format!("Worked for {}", human_duration(d)).into(),
            (false, None) => "Worked".into(),
        };
        let summary = summarize(&tools);
        let meta = if summary.is_empty() {
            String::new()
        } else {
            format!(" · {summary}")
        };

        let mut log = div().flex().flex_col().child(
            div()
                .id(ElementId::Name(format!("log-{ix}-{pi}").into()))
                .flex()
                .items_center()
                .gap(u(6.))
                .h(u(26.))
                .text_size(u(12.))
                .text_color(rgb(theme::text_2()))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::text())))
                .child(if running {
                    spinner(
                        ElementId::Name(format!("log-spin-{ix}-{pi}").into()),
                        12.,
                        theme::text_2(),
                    )
                } else {
                    icon(
                        if open {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        12.,
                        theme::dim(),
                    )
                })
                .child(if running {
                    working_label(format!("log-label-{ix}-{pi}"), label)
                } else {
                    div().flex_none().child(label).into_any_element()
                })
                .child(
                    div().flex_1().min_w_0().child(
                        div()
                            .w_full()
                            .one_line()
                            .text_color(rgb(theme::dim()))
                            .child(meta),
                    ),
                )
                .when(added + removed > 0, |d| d.child(diff_stat(added, removed)))
                .when(failed > 0, |d| {
                    d.child(div().flex_none().text_color(rgb(theme::text())).child(
                        if failed == 1 {
                            "· a step failed".to_string()
                        } else {
                            format!("· {failed} steps failed")
                        },
                    ))
                })
                .on_click(cx.listener(move |c, _, _, cx| {
                    c.log_open.insert(key, !open);
                    cx.notify();
                })),
        );
        if open {
            let mut list = div()
                .flex()
                .flex_col()
                .gap(u(2.))
                .ml(u(5.5))
                .pl(u(12.))
                .border_l_1()
                .border_color(rgba(theme::hairline()));
            // Only the newest plan in the reply opens by itself.
            let last_plan = blocks
                .iter()
                .rposition(|b| matches!(b, Block::Tool(t) if plan_items(t).is_some()));
            // A thought with nothing to read is no step at all; one still
            // coming shows while it is the newest thing.
            let shown: Vec<usize> = bixes
                .iter()
                .copied()
                .filter(|&b| match &blocks[b] {
                    Block::Reasoning { text, open } => {
                        !text.trim().is_empty() || (*open && Some(&b) == bixes.last())
                    }
                    _ => true,
                })
                .collect();
            // While it works, the newest few steps; the rest wait behind a
            // line that opens them.
            const LIVE_STEPS: usize = 4;
            let all = !running || self.log_all.contains(&key);
            let hidden = if all {
                0
            } else {
                shown.len().saturating_sub(LIVE_STEPS)
            };
            if hidden > 0 {
                list = list.child(
                    div()
                        .id(ElementId::Name(format!("log-more-{ix}-{pi}").into()))
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .h(u(24.))
                        .text_size(u(12.))
                        .text_color(rgb(theme::dim()))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(theme::text_2())))
                        .child(icon(Icon::ChevronUp, 11., theme::dim()))
                        .child(if hidden == 1 {
                            "1 earlier step".to_string()
                        } else {
                            format!("{hidden} earlier steps")
                        })
                        .on_click(cx.listener(move |c, _, _, cx| {
                            c.hold_place();
                            c.log_all.insert(key);
                            cx.notify();
                        })),
                );
            }
            for &bix in &shown[hidden..] {
                let step = match &blocks[bix] {
                    Block::Tool(t) => self.render_step(t, Some(bix) == last_plan, cx),
                    Block::Reasoning { text, open } => {
                        self.render_thought(ix, bix, text, *open, cx)
                    }
                    _ => div().into_any_element(),
                };
                // A step slides in as it arrives.
                list = list.child(if running {
                    rise_in(format!("step-{ix}-{bix}"), step)
                } else {
                    step
                });
            }
            log = log.child(fade_in(
                format!("log-body-{ix}-{pi}"),
                list.into_any_element(),
            ));
        }
        log.into_any_element()
    }

    fn render_thought(
        &mut self,
        ix: usize,
        bix: usize,
        text: &str,
        streaming: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("r{ix}:{bix}");
        let expanded = self.expanded.contains(&key);
        let k = key.clone();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(ElementId::Name(key.clone().into()))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(26.))
                    .text_size(u(12.))
                    .text_color(rgb(theme::dim()))
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(theme::text_2())))
                    .child(if streaming {
                        working_label(format!("think-{key}"), "Thinking…".into())
                    } else {
                        div().child("Thought").into_any_element()
                    })
                    .on_click(cx.listener(move |c, _, _, cx| {
                        if !c.expanded.remove(&k) {
                            c.expanded.insert(k.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(expanded, |d| {
                d.child(
                    div()
                        .pb(u(6.))
                        .text_size(u(12.))
                        .text_color(rgb(theme::dim()))
                        .child(text.to_string()),
                )
            })
            .into_any_element()
    }

    fn render_step(
        &mut self,
        t: &ToolInvocation,
        last_plan: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = t.id.0.to_string();
        let running = t.is_open();
        let kind = tool_kind(&t.name);
        let plan = plan_items(t);
        // The latest plan in a reply is open unless closed by hand; every
        // other step opens on a click.
        let expanded = self.expanded.contains(&key)
            || (last_plan && plan.is_some() && !self.collapsed.contains(&key));
        let stats = edit_stats(t);
        // Whether there is anything to open, decided without building it:
        // this runs every frame while a spinner turns.
        let has_output = match &t.result {
            None => !t.progress.trim().is_empty(),
            Some(ToolOutput::Terminal { text, .. })
            | Some(ToolOutput::Text(text))
            | Some(ToolOutput::FileExcerpt { text, .. }) => !text.trim().is_empty(),
            Some(ToolOutput::Diff { unified, .. }) => !unified.trim().is_empty(),
            Some(ToolOutput::Structured(_)) => true,
            Some(ToolOutput::Truncated { preview, .. }) => !preview.trim().is_empty(),
        };
        let has_body = plan.is_some() || kind == Kind::Command || stats.is_some() || has_output;
        // A command that is still running shows its latest output unasked.
        let live_tail = running && kind == Kind::Command && has_output && !expanded;
        let body = || -> (Option<String>, bool) {
            // An edit reads best as the change itself, whatever the tool said.
            if let Some(change) = (kind == Kind::Edit).then(|| edit_diff(t)).flatten() {
                return (Some(change), true);
            }
            match &t.result {
                None => (Some(t.progress.clone()), false),
                Some(ToolOutput::Terminal { text, .. }) | Some(ToolOutput::Text(text)) => {
                    (Some(text.clone()), false)
                }
                Some(ToolOutput::FileExcerpt { text, .. }) => (Some(text.clone()), false),
                Some(ToolOutput::Diff { unified, .. }) => (Some(unified.clone()), true),
                Some(ToolOutput::Structured(v)) => (
                    Some(serde_json::to_string_pretty(v).unwrap_or_default()),
                    false,
                ),
                Some(ToolOutput::Truncated { preview, .. }) => (Some(preview.clone()), false),
            }
        };
        let (verb, target) = step_phrase(t, running, &self.cwd);
        let took = self
            .tool_times
            .get(&key)
            .map(|(start, end)| end.unwrap_or_else(|| start.elapsed()));
        // Held up on the card above the composer.
        let held = running && self.asks.iter().any(|a| a.tool_use() == Some(&t.id));
        let k = key.clone();
        let row = div()
            .id(ElementId::Name(format!("step-{key}").into()))
            .flex()
            .items_center()
            .gap(u(8.))
            .h(u(26.))
            .overflow_hidden()
            .px(u(4.))
            .text_size(u(12.))
            .when(has_body, |d| {
                d.cursor_pointer().hover(|s| s.bg(rgb(theme::row_hover())))
            })
            .rounded(u(6.))
            .child(if held {
                icon(Icon::Info, 13., theme::text())
            } else if running {
                spinner(
                    ElementId::Name(format!("spin-{key}").into()),
                    12.,
                    theme::text_2(),
                )
            } else {
                icon(
                    tool_icon(&t.name),
                    13.,
                    if t.is_error {
                        theme::text()
                    } else {
                        theme::dim()
                    },
                )
            })
            // Verbs in one quiet column, so what each step touched lines
            // up down the log.
            .child(
                div()
                    .flex_none()
                    .min_w(u(54.))
                    .text_color(rgb(if running {
                        theme::text()
                    } else {
                        theme::dim()
                    }))
                    .child(verb),
            )
            .child(
                div().flex_1().min_w_0().child(
                    div()
                        .w_full()
                        .one_line()
                        .font_family(theme::mono())
                        .text_size(u(theme::TEXT_SMALL))
                        .text_color(rgb(if running {
                            theme::text_2()
                        } else {
                            theme::dim()
                        }))
                        .child(target),
                ),
            )
            .when_some(stats.filter(|(a, r)| a + r > 0), |d, (a, r)| {
                d.child(diff_stat(a, r))
            })
            .when(t.is_error, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::text()))
                        .child("failed"),
                )
            })
            // Seconds count up while it runs; afterwards only slow steps
            // say how long they took.
            .when(held, |d| {
                d.child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::text()))
                        .child("waiting for you"),
                )
            })
            .when_some(
                took.filter(|d| !held && (running || d.as_millis() >= 1000)),
                |d, took| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(theme::mono())
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child(step_duration(took)),
                    )
                },
            )
            .when(has_body, |d| {
                d.child(icon(
                    if expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    },
                    11.,
                    theme::dim(),
                ))
            })
            .on_click(cx.listener(move |c, _, _, cx| {
                c.hold_place();
                if expanded {
                    c.expanded.remove(&k);
                    c.collapsed.insert(k.clone());
                } else {
                    c.expanded.insert(k.clone());
                    c.collapsed.remove(&k);
                }
                cx.notify();
            }));

        let detail: Option<AnyElement> = if !expanded {
            live_tail.then(|| render_output(&body().0.unwrap_or_default(), false, 6))
        } else if let Some(items) = plan {
            Some(render_plan(&items))
        } else if kind == Kind::Command {
            // A command reads like a terminal: the prompt line, then what it
            // printed.
            let command = t
                .input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(&t.title)
                .to_string();
            Some(render_command(
                &command,
                body().0.as_deref().unwrap_or(""),
                t.is_error,
                running,
            ))
        } else {
            let (b, diff) = body();
            // A file read is coloured by the file's language.
            let grammar = match &t.result {
                Some(ToolOutput::FileExcerpt { path, .. }) if !diff => {
                    let input = t.input.get("file_path").and_then(|v| v.as_str());
                    crate::syntax::for_path(if path.is_empty() { input.unwrap_or("") } else { path })
                }
                _ => None,
            };
            b.filter(|b| !b.trim().is_empty()).map(|b| match grammar {
                Some(grammar) => render_excerpt(&key, grammar, &b, 40),
                None => render_output(&b, diff, 40),
            })
        };
        div()
            .flex()
            .flex_col()
            .child(row)
            .when_some(detail, |d, detail| {
                d.child(fade_in(format!("detail-{key}-{expanded}"), detail))
            })
            .into_any_element()
    }

    /// After a turn, the files it changed: one row each with its +/−, a
    /// click shows every change made to that file.
    fn render_changed_files(
        &mut self,
        ix: usize,
        blocks: &[Block],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let files = changed_files(blocks, &self.cwd);
        if files.is_empty() {
            return None;
        }
        let (added, removed) = files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let mut card = div()
            .flex()
            .flex_col()
            .rounded(u(10.))
            .border_1()
            .border_color(rgba(theme::input_edge()))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(32.))
                    .px(u(10.))
                    .bg(rgb(theme::raised()))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text_2()))
                    .child(icon(Icon::Files, 13., theme::dim()))
                    .child(div().flex_1().child(if files.len() == 1 {
                        "1 file changed".to_string()
                    } else {
                        format!("{} files changed", files.len())
                    }))
                    .child(diff_stat(added, removed)),
            );
        for f in files {
            let key = format!("cf-{ix}-{}", f.path);
            let open = self.expanded.contains(&key);
            let k = key.clone();
            let (dir, name) = match f.path.rfind('/') {
                Some(cut) => (f.path[..=cut].to_string(), f.path[cut + 1..].to_string()),
                None => (String::new(), f.path.clone()),
            };
            let full = self.cwd.join(&f.path);
            card = card.child(
                div()
                    .id(ElementId::Name(key.clone().into()))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(30.))
                    .px(u(10.))
                    .border_t_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(12.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::row_hover())))
                    .child(icon(Icon::Pencil, 12., theme::dim()))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .font_family(theme::mono())
                            .text_size(u(theme::TEXT_SMALL))
                            .child(div().flex_none().text_color(rgb(theme::text())).child(name))
                            .child(
                                div()
                                    .min_w_0()
                                    .one_line()
                                    .pl(u(8.))
                                    .text_color(rgb(theme::dim()))
                                    .child(dir),
                            ),
                    )
                    .child(diff_stat(f.added, f.removed))
                    .child(
                        div()
                            .id(ElementId::Name(format!("{key}-edit").into()))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(u(20.))
                            .rounded(u(5.))
                            .hover(|s| s.bg(rgb(theme::surface_hover())))
                            .child(icon(Icon::ArrowUpRight, 11., theme::dim()))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(ChatEvent::OpenFile(full.clone()));
                            })),
                    )
                    .child(icon(
                        if open {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        },
                        11.,
                        theme::dim(),
                    ))
                    .on_click(cx.listener(move |c, _, _, cx| {
                        if !c.expanded.remove(&k) {
                            c.expanded.insert(k.clone());
                        }
                        cx.notify();
                    })),
            );
            if open {
                card = card.child(fade_in(
                    format!("{key}-diff"),
                    div()
                        .px(u(8.))
                        .child(render_output(&f.diff(blocks), true, 200))
                        .into_any_element(),
                ));
            }
        }
        Some(card.into_any_element())
    }

    /// Attached files as a row: images as square thumbnails, everything
    /// else as a named chip. A click opens one in deck; in the composer each
    /// also has a × to take it off.
    fn render_attachments(
        &self,
        key: &str,
        files: &[PathBuf],
        removable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Images that came some way other than the composer (a restored
        // thread, a message sent from the CLI) get their thumbnails here.
        let missing: Vec<PathBuf> = files
            .iter()
            .filter(|p| is_image(p) && !self.thumbs.contains_key(*p))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = this.update(cx, |c, cx| c.want_thumbs(missing, cx));
            });
        }
        let mut row = div()
            .flex()
            .flex_wrap()
            // Room for the × that sits on each thumbnail's corner.
            .gap(u(if removable { 10. } else { 6. }))
            .when(!removable, |d| d.justify_end());
        for (i, path) in files.iter().enumerate() {
            let id = ElementId::Name(format!("att-{key}-{i}").into());
            // `None`: a file chip. `Some(None)`: a thumbnail on its way.
            let thumb = match (is_image(path), self.thumbs.get(path)) {
                (false, _) | (true, Some(Thumb::Failed)) => None,
                (true, Some(Thumb::Ready(t))) => Some(Some(t.clone())),
                (true, _) => Some(None),
            };
            let picture = thumb.is_some();
            let tile = match thumb {
                // Square, so it fills the tile exactly and the tile's own
                // corners round it; the border sits outside it, hence 9.
                Some(thumb) => div()
                    .size(u(if removable { 56. } else { 140. }))
                    .children(thumb.map(|t| img(t).size_full().rounded(u(9.)))),
                None => {
                    let dir = path.is_dir();
                    div()
                        .flex()
                        .items_center()
                        .gap(u(7.))
                        .h(u(32.))
                        .pl(u(10.))
                        .pr(u(if removable { 4. } else { 12. }))
                        .max_w(u(260.))
                        .text_size(u(12.))
                        .text_color(rgb(theme::text_2()))
                        .child(icon(
                            if dir { Icon::Folder } else { Icon::File },
                            13.,
                            theme::dim(),
                        ))
                        .child(div().min_w_0().one_line().child(file_name(path)))
                }
            };
            let p = path.clone();
            let tile = tile
                .id(id)
                .relative()
                .flex_none()
                .rounded(u(10.))
                .bg(rgb(theme::surface()))
                .border_1()
                .border_color(rgba(theme::input_edge()))
                .cursor_pointer()
                .hover(|s| s.border_color(rgba(theme::alpha(theme::text(), 0x40))))
                .on_click(
                    cx.listener(move |c, _, window, cx| c.open_preview(p.clone(), window, cx)),
                );
            let tile = if removable {
                let p = path.clone();
                let remove = div()
                    .id(ElementId::Name(format!("detach-{key}-{i}").into()))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_pointer()
                    .on_click(cx.listener(move |c, _, _, cx| {
                        // Not also a click on the tile, which would open it.
                        cx.stop_propagation();
                        c.detach(&p, cx)
                    }));
                tile.child(if picture {
                    // On the corner, off the picture: inverted like the send
                    // button, so it reads on any image and in every theme
                    // (it was the theme's text on fixed black, about 1.2:1
                    // in Light). The ring is the composer's colour, cut
                    // out of the thumbnail's corner.
                    remove
                        .absolute()
                        .top(u(-7.))
                        .right(u(-7.))
                        .size(u(20.))
                        .bg(rgb(theme::text()))
                        .border_2()
                        .border_color(rgb(theme::raised()))
                        .hover(|s| s.opacity(0.85))
                        .child(icon(Icon::Close, 9., theme::pane()))
                } else {
                    remove
                        .size(u(22.))
                        .hover(|s| s.bg(rgb(theme::surface_hover())))
                        .child(icon(Icon::Close, 10., theme::text_2()))
                })
            } else {
                tile
            };
            row = row.child(tile);
        }
        row.into_any_element()
    }

    /// An attachment opened over the thread. Esc, space, × or a click
    /// outside it closes it.
    fn render_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::preview::{self, Body};
        let p = self.preview.as_ref()?;
        let (open, reveal) = (p.path.clone(), p.path.clone());
        let glyph = if matches!(p.body, Body::Folder { .. }) {
            Icon::Folder
        } else {
            Icon::File
        };
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(u(8.))
            .h(u(52.))
            .pl(u(16.))
            .pr(u(10.))
            .border_b_1()
            .border_color(rgba(theme::hairline()))
            .child(icon(glyph, 15., theme::text_2()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .pl(u(2.))
                    .child(
                        div()
                            .one_line()
                            .text_size(u(13.))
                            .line_height(u(18.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::text()))
                            .child(file_name(&p.path)),
                    )
                    .child(
                        div()
                            .one_line()
                            .text_size(u(11.))
                            .line_height(u(15.))
                            .text_color(rgb(theme::dim()))
                            .child(preview::detail(p)),
                    ),
            )
            .child(
                preview::action("preview-open", "Open")
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_with_system(&open))),
            )
            .child(
                preview::action("preview-reveal", "Show in Finder")
                    .on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&reveal))),
            )
            .child(
                div()
                    .id("preview-close")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(u(28.))
                    .rounded(u(7.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(Icon::Close, 13., theme::text_2()))
                    .on_click(cx.listener(|c, _, window, cx| c.close_preview(window, cx))),
            );
        Some(
            div()
                .id("preview")
                .track_focus(&self.preview_focus)
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(u(24.))
                .bg(rgba(if theme::is_dark() {
                    0x00000059
                } else {
                    0x00000026
                }))
                // The thread under it gets neither the wheel nor the hover.
                .occlude()
                .on_key_down(cx.listener(|c, ev: &KeyDownEvent, window, cx| {
                    if matches!(ev.keystroke.key.as_str(), "escape" | "space") {
                        c.close_preview(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|c, _, window, cx| c.close_preview(window, cx)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .size_full()
                        .max_w(u(1000.))
                        .rounded(u(14.))
                        .bg(rgb(theme::pane()))
                        .border_1()
                        .border_color(rgba(theme::alpha(theme::text(), 0x1f)))
                        .overflow_hidden()
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(header)
                        .child(div().flex_1().min_h_0().child(preview::body(p))),
                )
                .into_any_element(),
        )
    }

    /// Files matching what follows `@`, above the composer. ↑↓ choose,
    /// Enter or Tab insert, Escape closes.
    fn render_mentions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.mention.as_ref().filter(|m| !m.matches.is_empty())?;
        let mut list = div()
            .id("mentions")
            .absolute()
            .left_0()
            .right_0()
            .bottom(relative(1.))
            .mb(u(6.))
            .p(u(4.))
            .flex()
            .flex_col()
            .rounded(u(12.))
            .bg(rgb(theme::raised()))
            .border_1()
            .border_color(rgba(theme::input_edge()))
            .occlude();
        if m.kind != '@' {
            for (i, name) in m.matches.iter().enumerate() {
                let selected = i == m.selected;
                let (start, insert) = (m.start, m.inserts.get(i).cloned().unwrap_or_default());
                let note = m.notes.get(i).cloned().unwrap_or_default();
                let skill = !insert.is_empty() && (m.kind == '$' || !matches!(name.as_str(), "/compact" | "/clear" | "/context" | "/cost" | "/init" | "/memory" | "/model" | "/review" | "/agents" | "/mcp" | "/diff" | "/status" | "/new"));
                list = list.child(
                    div()
                        .id(ElementId::NamedInteger("command".into(), i as u64))
                        .flex()
                        .items_center()
                        .gap(u(10.))
                        .h(u(32.))
                        .px(u(8.))
                        .rounded(u(8.))
                        .text_size(u(13.))
                        .cursor_pointer()
                        .when(selected, |d| d.bg(rgb(theme::surface())))
                        .hover(|s| s.bg(rgb(theme::surface())))
                        .child(icon(if skill { Icon::Snippet } else { Icon::Terminal }, 13., theme::dim()))
                        .child(div().flex_none().text_color(rgb(theme::text())).child(name.clone()))
                        .child(div().flex_1().min_w_0().one_line().text_size(u(12.)).text_color(rgb(theme::dim())).child(note))
                        .on_click(cx.listener(move |c, _, window, cx| {
                            c.mention = None;
                            let text = insert.clone();
                            c.composer.update(cx, |f, cx| {
                                f.popup = false;
                                f.complete_token(start, &text, cx);
                            });
                            window.focus(&c.composer.read(cx).focus);
                            cx.notify();
                        })),
                );
            }
            return Some(list.into_any_element());
        }
        for (i, path) in m.matches.iter().enumerate() {
            let dir = path.ends_with('/');
            let trimmed = path.trim_end_matches('/');
            let (parent, name) = match trimmed.rfind('/') {
                Some(cut) => (&trimmed[..=cut], &trimmed[cut + 1..]),
                None => ("", trimmed),
            };
            let selected = i == m.selected;
            let start = m.start;
            let chosen = path.clone();
            list = list.child(
                div()
                    .id(ElementId::NamedInteger("mention".into(), i as u64))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(28.))
                    .px(u(8.))
                    .rounded(u(8.))
                    .text_size(u(theme::TEXT_UI))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(
                        if dir { Icon::Folder } else { Icon::File },
                        13.,
                        theme::dim(),
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_color(rgb(theme::text()))
                            .child(format!("{name}{}", if dir { "/" } else { "" })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .one_line()
                            .text_color(rgb(theme::dim()))
                            .child(parent.to_string()),
                    )
                    .on_click(cx.listener(move |c, _, window, cx| {
                        c.mention = None;
                        let path = chosen.clone();
                        c.composer.update(cx, |f, cx| {
                            f.popup = false;
                            f.complete_mention(start, &path, cx);
                        });
                        window.focus(&c.composer.read(cx).focus);
                        cx.notify();
                    })),
            );
        }
        Some(list.into_any_element())
    }

    fn render_composer(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let working = self.busy();
        let has_text =
            !self.composer.read(cx).text().trim().is_empty() || !self.attachments.is_empty();
        // Stop only while running with nothing typed; typing while it runs
        // turns the button back into send, which queues.
        let stop = working && !has_text;
        let can_send = stop || has_text;
        let chip = |id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(6.))
                .h(u(28.))
                .px(u(9.))
                .rounded(u(8.))
                .text_size(u(12.))
                .cursor_pointer()
        };

        let toolbar = div()
            .flex()
            .items_center()
            .gap(u(4.))
            .child(
                chip("attach")
                    .px(u(0.))
                    .w(u(28.))
                    .justify_center()
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(Icon::Plus, 14., theme::text_2()))
                    .on_click(cx.listener(|c, _, _, cx| c.pick_files(cx))),
            )
            .child({
                // One chip for who answers: the agent's mark and the model.
                // Its menu lists the agents, each opening onto its models.
                let label: SharedString = match &self.model {
                    Some(id) => self
                        .models()
                        .iter()
                        .find(|m| m.id == id.as_str())
                        .map_or_else(|| id.to_string(), |m| m.label.to_string())
                        .into(),
                    None if self.models().is_empty() => self.agent_name().into(),
                    None => format!("{} · Default", self.agent_name()).into(),
                };
                // Its own width, up to a cap: it used to collapse to "OpenC…"
                // while the spacer beside it kept the room.
                div()
                    .relative()
                    .flex_none()
                    .max_w(u(240.))
                    .on_mouse_down_out(cx.listener(|c, _, _, cx| {
                        if c.model_menu.take().is_some() {
                            cx.notify();
                        }
                    }))
                    .child(
                        chip("agent")
                            .min_w_0()
                            .text_color(rgb(theme::text_2()))
                            .when(self.model_menu.is_some(), |d| {
                                d.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                            })
                            .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                            .child(icon(Icon::agent(self.agent.as_str()), 13., theme::text_2()))
                            .child(div().min_w_0().max_w(u(190.)).one_line().child(label))
                            .child(icon(Icon::ChevronDown, 11., theme::dim()))
                            .on_click(cx.listener(|c, _, _, cx| {
                                c.model_menu = match c.model_menu {
                                    Some(_) => None,
                                    None => Some(c.agent.clone()),
                                };
                                c.close_access_menu(cx);
                                cx.notify();
                            })),
                    )
                    .children(self.render_model_menu(cx))
            })
            .child({
                // A chip like the agent's beside it, a menu like it too: a
                // click used to move silently to the next mode, restarting
                // the agent at each step. Full access keeps full-strength
                // text and the accent mark; the menu says what each mode
                // allows.
                let access = Access::of(self.mode);
                let loud = access == Access::Full;
                let tint = if loud { theme::text() } else { theme::text_2() };
                let mark = if loud { theme::accent() } else { theme::text_2() };
                div()
                    .relative()
                    .on_mouse_down_out(cx.listener(|c, _, _, cx| c.close_access_menu(cx)))
                    .child(
                        chip("mode")
                            .text_color(rgb(tint))
                            .when(self.access_menu.is_some(), |d| {
                                d.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                            })
                            .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                            .child(icon(access_icon(access), 13., mark))
                            .child(access.name())
                            .child(icon(Icon::ChevronDown, 11., theme::dim()))
                            .on_click(cx.listener(|c, _, _, cx| c.toggle_access_menu(cx))),
                    )
                    .children(self.render_access_menu(cx))
            })
            .child(div().flex_1())
            // The thread's own views, beside send: they never squeeze the
            // agent's name out of its chip.
            .child(
                // The console: the agent's commands and their output.
                chip("console")
                    .px(u(0.))
                    .w(u(30.))
                    .justify_center()
                    .tooltip(|_, cx| crate::workspace::Tip::view("Console: commands and their output", cx))
                    .when(self.console, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(Icon::Terminal, 15., if self.console { theme::text() } else { theme::text_2() }))
                    .on_click(cx.listener(|c, _, _, cx| {
                        c.console = !c.console;
                        cx.notify();
                    })),
            )
            .child(
                // The outline: every message you sent, to jump back to.
                chip("outline")
                    .px(u(0.))
                    .w(u(30.))
                    .justify_center()
                    .tooltip(|_, cx| crate::workspace::Tip::view("Outline: jump to a message", cx))
                    .when(self.outline_open, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .child(icon(Icon::List, 15., if self.outline_open { theme::text() } else { theme::text_2() }))
                    .on_click(cx.listener(|c, _, _, cx| {
                        c.outline_open = !c.outline_open;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("send")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(u(28.))
                    .rounded(u(14.))
                    .when(can_send, |d| {
                        d.bg(rgb(theme::text()))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.85))
                    })
                    .when(!can_send, |d| d.bg(rgb(theme::surface())))
                    // Stop is a solid square, as everywhere else: an outline
                    // that small reads as a dot.
                    .child(if stop {
                        div()
                            .size(u(10.))
                            .rounded(u(2.5))
                            .bg(rgb(theme::pane()))
                            .into_any_element()
                    } else {
                        icon(
                            Icon::ArrowUp,
                            15.,
                            if can_send {
                                theme::pane()
                            } else {
                                theme::dim()
                            },
                        )
                    })
                    .on_click(cx.listener(move |c, _, _, cx| {
                        let empty = c.composer.read(cx).text().trim().is_empty()
                            && c.attachments.is_empty();
                        if c.busy() && empty {
                            c.interrupt(cx);
                        } else {
                            c.submit(false, cx);
                        }
                    })),
            );

        let mut stack = div().w_full().flex().flex_col().gap(u(8.));
        // Queued messages, oldest first: those already with the agent (it
        // reads them when its current step ends), then those deck holds.
        let rows = self
            .inflight
            .iter()
            .map(|(_, q)| (q, true))
            .chain(self.queued.iter().map(|q| (q, false)))
            .enumerate()
            .map(|(i, (q, with_agent))| (i, q.clone(), with_agent))
            .collect::<Vec<_>>();
        for (i, q, with_agent) in rows {
            let what = match (q.text.is_empty(), q.attachments.len()) {
                (_, 0) => q.text.clone(),
                (true, n) => format!("{n} attachment{}", if n == 1 { "" } else { "s" }),
                (false, n) => format!(
                    "{} · {n} attachment{}",
                    q.text,
                    if n == 1 { "" } else { "s" }
                ),
            };
            let deck_ix = i.saturating_sub(self.inflight.len());
            stack = stack.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .pl(u(12.))
                    .pr(u(if with_agent { 12. } else { 4. }))
                    .h(u(32.))
                    .rounded(u(8.))
                    .border_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text_2()))
                    .child(icon(Icon::ArrowUp, 12., theme::dim()))
                    .child(div().text_color(rgb(theme::dim())).child(if q.now {
                        "Sending now"
                    } else {
                        "Queued"
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().w_full().one_line().child(what)),
                    )
                    // Already handed over: the agent cannot give it back, so
                    // there is nothing to cancel; it reads it next.
                    .when(with_agent, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(u(11.))
                                .text_color(rgb(theme::dim()))
                                .child("after this step"),
                        )
                    })
                    .when(!with_agent, |d| {
                        d.child(
                            crate::workspace::icon_button(
                                ElementId::NamedInteger("unqueue".into(), deck_ix as u64),
                                Icon::Close,
                            )
                            .size(u(22.))
                            .on_click(cx.listener(
                                move |c, _, _, cx| {
                                    if deck_ix < c.queued.len() {
                                        c.queued.remove(deck_ix);
                                    }
                                    cx.notify();
                                },
                            )),
                        )
                    }),
            );
        }
        if let Some(e) = self.error.clone() {
            // A slim notice, not a card: what happened, and the one thing
            // to do about it.
            stack = stack.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(32.))
                    .pl(u(10.))
                    .pr(u(4.))
                    .rounded(u(8.))
                    .bg(rgba(theme::alpha(theme::DANGER, 0x1a)))
                    .text_size(u(theme::TEXT_UI))
                    .child(icon(Icon::Info, 13., theme::DANGER))
                    .child(div().flex_1().min_w_0().one_line().text_color(rgb(theme::text_2())).child(e))
                    .child(
                        div()
                            .id("try-again")
                            .flex()
                            .flex_none()
                            .items_center()
                            .h(u(24.))
                            .px(u(8.))
                            .rounded(u(6.))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text()))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::row_hover())))
                            .child("Try again")
                            .on_click(cx.listener(|c, _, _, cx| {
                                c.error = None;
                                c.warmed = false;
                                if let Some(old) = c.conn.take() {
                                    old.shutdown();
                                }
                                c.warm(cx);
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(card) = self.render_ask(cx) {
            stack = stack.child(card);
        }
        let focus = self.composer.read(cx).focus.clone();
        let focused = focus.contains_focused(window, cx);
        if focused && !self.warmed && self.conn.is_none() {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |c, cx| c.warm(cx));
            })
            .detach();
        }
        let refine_mode = crate::refine::mode();
        let retry_line = self.retry.as_ref().map(|(n, at, why)| {
            let left = at.saturating_duration_since(Instant::now()).as_secs();
            let left = if left >= 120 {
                format!("{}m", left.div_ceil(60))
            } else {
                format!("{left}s")
            };
            format!(
                "{why} · trying again in {left} ({n}/{}) · esc cancels",
                crate::retry::MAX_TRIES
            )
        });
        let hint: SharedString = if let Some(r) = retry_line {
            r.into()
        } else if self.refining.is_some() {
            format!("Rewriting with {}…", crate::refine::rewriter()).into()
        } else if let Some(note) = &self.refine_note {
            note.clone()
        } else if let Some(note) = self.limit_note.as_ref().filter(|_| !working) {
            note.clone()
        } else if self.mine.is_some() {
            "✦ rewritten · ⏎ send · ⌃Z my words · ⌃O again".into()
        } else if !self.asks.is_empty() {
            "↑↓ choose · ⏎ confirm · esc no · or type a reply".into()
        } else if working && self.queues_input() {
            "esc to stop · ⏎ queue for after this step · ⌘⏎ send now".into()
        } else if working {
            "esc to stop · messages you send now wait for the turn to end".into()
        } else if self.mention.as_ref().is_some_and(|m| !m.matches.is_empty()) {
            "↑↓ choose · ⏎ or ⇥ insert · esc close".into()
        } else if refine_mode == crate::refine::Mode::Off {
            "⏎ send · ⇧⏎ new line · @ file".into()
        } else {
            format!(
                "⏎ {} · ⌃O rewrite only · ⇧⏎ new line",
                if refine_mode == crate::refine::Mode::Auto {
                    "rewrite and send"
                } else {
                    "rewrite, again to send"
                }
            )
            .into()
        };
        let u_ = self.transcript.usage;
        let spent = (u_.input + u_.output > 0).then(|| {
            let cost = u_.cost_usd.filter(|c| *c > 0.).map(|c| format!(" · ${c:.2}")).unwrap_or_default();
            format!("{}{cost}", compact_count(u_.input + u_.output + u_.cache_read))
        });
        // Concentric: 8px chips sit 8px inside the composer's 16px card.
        stack
            .child(
                div()
                    .id("composer")
                    .key_context("Composer")
                    .on_action(
                        cx.listener(|c, _: &crate::workspace::RewritePrompt, _, cx| {
                            c.rewrite(false, cx)
                        }),
                    )
                    .on_action(
                        cx.listener(|c, _: &crate::workspace::MyWords, _, cx| c.my_words(cx)),
                    )
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(u(4.))
                    .p(u(8.))
                    .rounded(u(16.))
                    .bg(rgb(theme::raised()))
                    .border_1()
                    .border_color(rgba(if focused {
                        theme::input_edge()
                    } else {
                        theme::hairline()
                    }))
                    .cursor_text()
                    // A click anywhere on the card, not just on the text, types.
                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                        window.focus(&focus)
                    })
                    .when(!self.attachments.is_empty(), |d| {
                        // Room above for the × on each thumbnail's corner.
                        d.child(div().pt(u(6.)).px(u(2.)).child(self.render_attachments(
                            "draft",
                            &self.attachments,
                            true,
                            cx,
                        )))
                    })
                    .child(self.composer.clone())
                    .child(toolbar)
                    .children(self.render_mentions(cx)),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .justify_center()
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    // Quiet until looked at: the keys are there when needed,
                    // not a second toolbar under the composer.
                    .opacity(0.75)
                    .child(if self.refining.is_some() {
                        crate::icon::shimmer("refining", hint)
                    } else {
                        crate::kbd::hints(&hint).justify_center().into_any_element()
                    })
                    // What the thread has spent, under the send button: it
                    // used to sit in the toolbar and push send out of a
                    // narrow composer.
                    .when_some(spent, |d, s| {
                        d.child(div().absolute().right(u(10.)).top_0().bottom_0().flex().items_center().child(s))
                    }),
            )
            .into_any_element()
    }

    /// Pick an agent and a model in one go: picking another agent's model
    /// switches agent and starts it on that model.
    fn pick_model(&mut self, agent: AgentId, model: Option<smol_str::SmolStr>, cx: &mut Context<Self>) {
        if agent == self.agent {
            self.choose_model(model, cx);
        } else {
            self.switch_agent(agent, model, cx);
        }
    }

    /// The agents down the left, the models of the one under the pointer
    /// beside it, lined up with that agent's row, with a check on what
    /// this thread runs. Both ease in; the models slide over as the
    /// pointer moves between agents.
    fn render_model_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        const ROW: f32 = 30.;
        const HEAD: f32 = 24.;
        const GAP: f32 = 1.;
        const PAD: f32 = 4.;
        let lit = self.model_menu.clone()?;
        let panel = || {
            div()
                .flex()
                .flex_col()
                .gap(u(GAP))
                .p(u(PAD))
                .rounded(u(10.))
                .bg(rgb(theme::menu()))
                .border_1()
                .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                .shadow_lg()
                .occlude()
        };
        // A panel's small title: what its rows choose.
        let head = |label: String| {
            div()
                .flex()
                .items_center()
                .h(u(HEAD))
                .px(u(8.))
                .text_size(u(11.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::dim()))
                .child(label)
        };
        let row = |id: ElementId| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(8.))
                .h(u(ROW))
                .px(u(8.))
                .rounded(u(6.))
                .text_size(u(13.))
        };
        let entries = self.env.registry.all();
        let n = entries.len();
        let mut lit_ix = 0usize;
        let default_agent = crate::settings::load().default_agent;
        let mut agents = panel().w(u(196.)).child(head("Agent".into()));
        for (ix, entry) in entries.into_iter().enumerate() {
            let id = entry.id();
            let ready = entry.is_ready();
            let on = id == lit;
            if on {
                lit_ix = ix;
            }
            let current = id == self.agent;
            let hover_to = id.clone();
            let has_models = !entry.adapter.models().is_empty();
            let pick_to = id.clone();
            agents = agents.child(
                row(ElementId::Name(format!("menu-agent-{}", id.as_str()).into()))
                    .when(on && ready, |d| d.bg(rgb(theme::surface())))
                    .text_color(rgb(if !ready { theme::dim() } else if on { theme::text() } else { theme::text_2() }))
                    .when(ready, |d| d.cursor_pointer())
                    .on_hover(cx.listener(move |c, hovered: &bool, _, cx| {
                        if *hovered && c.model_menu.as_ref() != Some(&hover_to) {
                            c.model_menu = Some(hover_to.clone());
                            cx.notify();
                        }
                    }))
                    .child(crate::icon::agent_mark(
                        id.as_str(),
                        entry.display_name(),
                        14.,
                        match (ready, current) {
                            (false, _) => theme::dim(),
                            (true, true) => Icon::agent(id.as_str()).brand().unwrap_or(theme::text()),
                            (true, false) => theme::text_2(),
                        },
                    ))
                    .child(div().flex_1().min_w_0().one_line().child(entry.display_name().to_string()))
                    // The one new chats start with.
                    .when(ready && Some(id.as_str()) == default_agent.as_deref(), |d| {
                        d.child(div().flex_none().text_size(u(theme::TEXT_META)).text_color(rgb(theme::dim())).child("default"))
                    })
                    .when(!ready, |d| d.child(div().text_size(u(theme::TEXT_META)).text_color(rgb(theme::dim())).child("Not set up")))
                    .when(ready && current, |d| d.child(icon(Icon::Check, 11., theme::text())))
                    .when(ready && has_models, |d| d.child(icon(Icon::ChevronRight, 10., theme::dim())))
                    .when(ready && !has_models, |d| {
                        d.on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |c, _, _, cx| c.pick_model(pick_to.clone(), None, cx)),
                        )
                    }),
            );
        }
        // Make the agent under the pointer the one new chats start with.
        let mut below = 0.;
        if let Some(entry) = self.env.registry.get(&lit).filter(|e| e.is_ready()) {
            if default_agent.as_deref() != Some(lit.as_str()) {
                // The divider (1 + 4 margin) and the row, under the agents.
                below = 5. + GAP + ROW + GAP;
                let to = lit.as_str().to_string();
                agents = agents.child(div().h(u(1.)).mx(u(6.)).my(u(2.)).bg(rgba(theme::hairline()))).child(
                    row(ElementId::Name("menu-make-default".into()))
                        .cursor_pointer()
                        .text_size(u(12.))
                        .text_color(rgb(theme::text_2()))
                        .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                        .child(format!("Make {} the default", entry.display_name()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |_, _, _, cx| cx.emit(ChatEvent::DefaultAgent(to.clone()))),
                        ),
                );
            }
        }
        let models = self
            .env
            .registry
            .get(&lit)
            .filter(|e| e.is_ready())
            .map(|e| e.adapter.models())
            .unwrap_or(&[]);
        let mut menu = div()
            .absolute()
            .left_0()
            .bottom(relative(1.))
            .mb(u(6.))
            .flex()
            .items_end()
            .gap(u(4.))
            .child(agents);
        if !models.is_empty() {
            // At most nine rows tall; longer lists scroll inside it.
            const MAX_ROWS: usize = 9;
            let count = models.len() + 1;
            let visible = count.min(MAX_ROWS);
            // Its top on the hovered agent's row, unless it is taller than
            // the room above; then it sits on the menu's floor. Both panels
            // open with a title row.
            let height = PAD * 2. + HEAD + GAP + visible as f32 * ROW + (visible - 1) as f32 * GAP + 2.;
            let row_top = (n - lit_ix) as f32 * (ROW + GAP) - GAP + PAD + 1. + HEAD + GAP + below;
            let lift = (row_top - height).max(0.);
            let agent_label = lit.as_str().to_lowercase();
            let agent_name = self.env.registry.get(&lit).map(|e| e.display_name().to_string()).unwrap_or_default();
            let mut col = panel()
                .id("menu-models")
                .w(u(280.))
                .mb(u(lift))
                .max_h(u(height))
                .overflow_y_scroll()
                .child(head(format!("{agent_name} models")));
            let choices = std::iter::once((None, "Default".to_string(), "what the CLI picks"))
                .chain(models.iter().map(|m| (Some(smol_str::SmolStr::new(m.id)), m.label.to_string(), m.note)));
            for (i, (id, label, note)) in choices.enumerate() {
                // A note that only repeats the agent's name says nothing.
                let note = if note.to_lowercase() == agent_label { "" } else { note };
                let current = lit == self.agent && id == self.model;
                let to = lit.clone();
                col = col.child(
                    row(ElementId::NamedInteger("menu-model".into(), i as u64))
                        .cursor_pointer()
                        .text_color(rgb(if current { theme::text() } else { theme::text_2() }))
                        .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                        .child(div().flex_1().min_w_0().one_line().child(label))
                        // Notes in one column on the right, the check after.
                        .child(div().flex_none().one_line().text_size(u(theme::TEXT_SMALL)).text_color(rgb(theme::dim())).child(note))
                        .child(div().flex_none().w(u(12.)).when(current, |d| d.child(icon(Icon::Check, 11., theme::text()))))
                        // On press: the menu hangs outside the chip, so by
                        // release the press outside has shut it.
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |c, _, _, cx| c.pick_model(to.clone(), id.clone(), cx)),
                        ),
                );
            }
            // Swapped at once as the pointer moves between agents.
            menu = menu.child(col);
        }
        // Drawn after everything else, so nothing under it (the composer's
        // edge, its placeholder) shows through.
        Some(
            gpui::deferred(menu.with_animation(
                "model-menu",
                Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                |d, t| d.opacity(t).mb(u(6. - 4. * (1. - t))),
            ))
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// A new thread: the composer is the centre of the screen, with a few
    /// things worth asking underneath.
    fn render_empty(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        // Quiet rows, not boxes: a mark, what it does, and the ask in full
        // as the row's second half.
        let mut starters = div().flex().flex_col().gap(u(2.));
        for (i, (glyph, title, prompt)) in [
            (
                Icon::Bolt,
                "Run it",
                "Start the dev server in a terminal here and open it in the browser",
            ),
            (Icon::Info, "Explain it", "Explain how this project is put together"),
            (Icon::Check, "Test it", "Run the tests and fix what fails"),
        ]
        .into_iter()
        .enumerate()
        {
            let group = SharedString::from(format!("starter-{i}"));
            starters = starters.child(
                div()
                    .id(ElementId::NamedInteger("starter".into(), i as u64))
                    .group(group.clone())
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .px(u(10.))
                    .h(u(34.))
                    .rounded(u(8.))
                    .text_size(u(13.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::raised())))
                    .child(div().w(u(16.)).flex_none().flex().justify_center().child(icon(glyph, 14., theme::dim())))
                    .child(
                        div()
                            .flex_none()
                            .w(u(80.))
                            .text_color(rgb(theme::text_2()))
                            .group_hover(group.clone(), |s| s.text_color(rgb(theme::text())))
                            .child(title),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            div()
                                .w_full()
                                .one_line()
                                .text_color(rgb(theme::dim()))
                                .child(prompt),
                        ),
                    )
                    .on_click(cx.listener(move |c, _, _, cx| c.send(prompt.to_string(), cx))),
            );
        }
        // Where it works, as a short path: the folder's name, its parents
        // trimmed to fit.
        let place = tidy_place(&self.cwd);
        div()
            .flex_1()
            .flex()
            .flex_col()
            .justify_center()
            .child(Self::column(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(20.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(u(8.))
                            .child(
                                div()
                                    .text_size(u(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::text()))
                                    .child("What should we work on?"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(u(6.))
                                    .min_w_0()
                                    .text_size(u(theme::TEXT_UI))
                                    .text_color(rgb(theme::text_2()))
                                    .child(icon(Icon::agent(self.agent.as_str()), 13., theme::text_2()))
                                    .child(self.agent_name())
                                    .child(div().text_color(rgb(theme::dim())).child("in"))
                                    .child(
                                        div()
                                            .id("pick-folder")
                                            .flex()
                                            .items_center()
                                            .gap(u(4.))
                                            .min_w_0()
                                            .h(u(24.))
                                            .px(u(6.))
                                            .ml(u(-2.))
                                            .rounded(u(6.))
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                                            .child(icon(Icon::Folder, 13., theme::dim()))
                                            .child(div().min_w_0().one_line().child(place))
                                            .child(icon(Icon::ChevronDown, 11., theme::dim()))
                                            .on_click(cx.listener(|c, _, _, cx| c.pick_folder(cx))),
                                    )
                                    .when(self.remote.is_none(), |d| {
                                        let on = self.own_branch;
                                        d.child(
                                            div()
                                                .id("own-branch")
                                                .flex()
                                                .items_center()
                                                .gap(u(5.))
                                                .h(u(24.))
                                                .px(u(8.))
                                                .rounded(u(6.))
                                                .border_1()
                                                .border_color(rgba(if on { theme::alpha(theme::accent(), 0x66) } else { theme::hairline() }))
                                                .when(on, |d| d.bg(rgba(theme::alpha(theme::accent(), 0x14))).text_color(rgb(theme::text())))
                                                .cursor_pointer()
                                                .hover(|s| s.text_color(rgb(theme::text())))
                                                .child(icon(Icon::Git, 12., if on { theme::accent() } else { theme::dim() }))
                                                .child(if on { "On its own branch" } else { "Own branch" })
                                                .on_click(cx.listener(|c, _, _, cx| {
                                                    c.own_branch = !c.own_branch;
                                                    cx.notify();
                                                })),
                                        )
                                    }),
                            ),
                    )
                    .child(self.render_composer(window, cx))
                    .child(starters),
            ))
            .into_any_element()
    }

    /// A paragraph's text with its inline styles: inline code in the mono
    /// face, links underlined and clickable (they open in deck's browser).
    #[allow(clippy::too_many_arguments)]
    fn rich_text(
        &self,
        id: String,
        spans: &Spans,
        color: u32,
        weight: FontWeight,
        sep: &'static str,
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, rec) = self.unit_text(
            spans.text.clone().into(),
            text_runs(spans, color, weight),
            sep,
            prefix,
        );
        if spans.links.is_empty() {
            return selectable(text.into_any_element(), rec);
        }
        let ranges: Vec<std::ops::Range<usize>> =
            spans.links.iter().map(|(r, _)| r.clone()).collect();
        let urls: Vec<String> = spans.links.iter().map(|(_, u)| u.clone()).collect();
        let chat = cx.entity().downgrade();
        let linked = InteractiveText::new(ElementId::Name(id.into()), text)
            .on_click(ranges, move |i, _, cx| {
                if let Some(url) = urls.get(i).cloned() {
                    let _ = chat.update(cx, |_, cx| cx.emit(ChatEvent::OpenUrl(url)));
                }
            })
            .into_any_element();
        selectable(linked, rec)
    }

    /// A piece of selectable text: its key in reading order, the selected
    /// part lit, and its layout kept once painted. Returns the text and an
    /// element to lay over it that records where it was painted.
    fn unit_text(
        &self,
        text: SharedString,
        runs: Vec<TextRun>,
        sep: &'static str,
        prefix: &str,
    ) -> (StyledText, AnyElement) {
        let key = (self.sel_msg.get(), self.sel_unit.get());
        self.sel_unit.set(key.1 + 1);
        self.units.borrow_mut().insert(
            key,
            select::Unit {
                text: text.clone(),
                sep,
                prefix: prefix.to_string().into(),
            },
        );
        let runs = match self.selection.and_then(|s| s.covers(key, text.len())) {
            Some(r) => select::highlight(runs, r, rgb(theme::selection()).into()),
            None => runs,
        };
        let runs = theme::safe_runs(&text, runs);
        let styled = StyledText::new(text).with_runs(runs);
        let layout = styled.layout().clone();
        let painted = self.painted.clone();
        // Laid over the text and prepainted after it, so the layout it
        // keeps has this frame's position.
        let rec = canvas(
            move |bounds, _, _| {
                painted.borrow_mut().push(select::Painted {
                    key,
                    layout,
                    bounds,
                })
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element();
        (styled, rec)
    }

    // -- selecting text ---------------------------------------------------------

    fn select_down(&mut self, ev: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        let hit = select::hit(&self.painted.borrow(), ev.position);
        let had = self.selection.is_some();
        self.selection = None;
        self.selecting = false;
        if let Some(spot) = hit {
            let len = self
                .units
                .borrow()
                .get(&spot.key)
                .map_or(0, |u| u.text.len());
            let range = match ev.click_count {
                1 => spot.at..spot.at,
                2 => self
                    .units
                    .borrow()
                    .get(&spot.key)
                    .map_or(spot.at..spot.at, |u| select::word(&u.text, spot.at)),
                _ => 0..len,
            };
            self.selection = Some(select::Selection {
                anchor: select::Spot {
                    key: spot.key,
                    at: range.start,
                },
                focus: select::Spot {
                    key: spot.key,
                    at: range.end,
                },
            });
            self.selecting = ev.click_count == 1;
        }
        if had || hit.is_some() {
            cx.notify();
        }
    }

    fn select_drag(&mut self, p: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        if let Some(spot) = select::nearest(&self.painted.borrow(), p) {
            if sel.focus != spot {
                sel.focus = spot;
                cx.notify();
            }
        }
    }

    fn select_up(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.selection.is_some_and(|s| s.is_empty()) {
            self.selection = None;
            cx.notify();
        }
    }

    // -- checkpoints ------------------------------------------------------------

    /// Keep the project's files as they are now, in the background, as the
    /// checkpoint for message `ix`. The agent takes seconds to make its
    /// first change; the snapshot takes milliseconds.
    fn checkpoint(&mut self, ix: usize, cx: &mut Context<Self>) {
        let dir = self.cwd.clone();
        let name = crate::checkpoint::refname(&self.thread_id, &ix.to_string());
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move { crate::checkpoint::save(&dir, &name) })
                .await;
            match saved {
                Ok(commit) => {
                    let _ = this.update(cx, |c, cx| {
                        c.checkpoints.insert(ix, commit);
                        c.schedule_save(cx);
                    });
                }
                Err(e) => tracing::debug!("no checkpoint: {e}"),
            }
        })
        .detach();
    }

    /// Go back to before message `ix`: the project's files as they were
    /// when it was sent (when a checkpoint has them) and the thread cut
    /// there. With `edit`, the message comes back into the composer.
    fn rewind(&mut self, ix: usize, edit: bool, cx: &mut Context<Self>) {
        let Some(msg) = self.transcript.messages.get(ix) else {
            return;
        };
        let text = message_text(msg);
        // The agent's session holds the turns being dropped: it ends, and
        // the next message starts a fresh one with the thread so far.
        if let Some(conn) = self.conn.take() {
            conn.shutdown();
        }
        self.native_id = None;
        self.status = ChatStatus::Idle;
        self.asks.clear();
        self.inflight.clear();
        self.queued.clear();
        self.retry = None;
        self._retry = None;
        self.retries = 0;
        self.error = None;
        self.failed = None;
        self.turn_started = None;
        self.resuming = None;
        self.interrupting = false;
        self.selection = None;
        self.transcript.messages.truncate(ix);
        self.docs.retain(|(m, _), _| *m < ix);
        self.worked.retain(|m, _| *m < ix);
        self.reveal.retain(|(m, _), _| *m < ix);
        self.log_all.retain(|(m, _)| *m < ix);
        self.log_open.retain(|(m, _), _| *m < ix);
        self.units.borrow_mut().retain(|k, _| k.0 < ix);
        let target = self.checkpoints.get(&ix).cloned();
        self.checkpoints.retain(|m, _| *m < ix);
        self.list.reset(self.transcript.messages.len());
        if edit {
            self.composer.update(cx, |f, cx| f.set_text(&text, cx));
        }
        match target {
            Some(target) => {
                self.refine_note = Some("Putting the files back…".into());
                self.restore_files(target, "before-rewind", true, cx);
            }
            None => {
                self.refine_note = Some(
                    "Went back. That message has no checkpoint, so the files are as they were."
                        .into(),
                );
            }
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Undo the last going back: the files as they were just before it.
    fn put_files_back(&mut self, cx: &mut Context<Self>) {
        if let Some(commit) = self.undo_rewind.take() {
            self.restore_files(commit, "after-rewind", false, cx);
        }
    }

    /// Restore the files to `target` in the background, first keeping how
    /// they are now under `keep_as` (kept to undo it when `undoable`).
    fn restore_files(
        &mut self,
        target: String,
        keep_as: &str,
        undoable: bool,
        cx: &mut Context<Self>,
    ) {
        let dir = self.cwd.clone();
        let keep = crate::checkpoint::refname(&self.thread_id, keep_as);
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    let now = crate::checkpoint::save(&dir, &keep)?;
                    let n = crate::checkpoint::restore(&dir, &target, &now)?;
                    Ok::<_, String>((now, n))
                })
                .await;
            let _ = this.update(cx, |c, cx| {
                c.refine_note = Some(match done {
                    Ok((now, n)) => {
                        let files = if n == 1 { "file" } else { "files" };
                        if undoable {
                            c.undo_rewind = Some(now);
                            format!("Went back: {n} {files} restored. Right-click a message to put them back.")
                        } else {
                            format!("Put back: {n} {files} restored.")
                        }
                        .into()
                    }
                    Err(e) => format!("The files could not be restored: {e}").into(),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// `;keyword ` just typed in the composer: put that snippet in its place.
    fn expand_keyword(&mut self, cx: &mut Context<Self>) {
        let found = {
            let f = self.composer.read(cx);
            crate::snippets::trigger(f.before_caret()).map(|(len, w)| (len, w.to_string()))
        };
        let Some((len, word)) = found else { return };
        let root = crate::inspect::project_root(&self.cwd);
        let Some(s) = crate::snippets::load_all(Some(&root))
            .into_iter()
            .find(|s| s.keyword == word)
        else {
            return;
        };
        let text = crate::snippets::expand(&s.body, |var| match var {
            "clipboard" => cx.read_from_clipboard().and_then(|c| c.text()),
            "selection" => self.selected_text(),
            "branch" => crate::snippets::branch(&self.cwd),
            "folder" => Some(self.cwd.display().to_string()),
            _ => None,
        });
        self.composer
            .update(cx, |f, cx| f.replace_before_caret(len, &text, cx));
    }

    /// The part of a streaming text block to show now. Text arrives in
    /// bursts; on screen it comes out at a steady pace that speeds up with
    /// how far behind it is (all of it within a fraction of a second), so
    /// nothing jumps. A block first seen finished (a restored thread, a
    /// reply that arrived whole) shows at once.
    fn reveal_prefix<'a>(&mut self, key: (usize, usize), text: &'a str, open: bool) -> &'a str {
        let now = Instant::now();
        let total = text.len() as f32;
        let (shown, at) = self
            .reveal
            .entry(key)
            .or_insert((if open { 0. } else { total }, now));
        if *shown >= total {
            *shown = total;
            *at = now;
            return text;
        }
        let dt = now.duration_since(*at).as_secs_f32().min(0.1);
        let behind = total - *shown;
        // Catches up within about an eighth of a second of what arrived:
        // smooth through bursts without trailing the stream.
        let pace = (behind * 8.).max(240.);
        *shown = (*shown + pace * dt).min(total);
        *at = now;
        self.revealing.set(true);
        let mut cut = *shown as usize;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        &text[..cut]
    }

    /// The selected text, if any is selected.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection.filter(|s| !s.is_empty())?;
        let text = select::text(&self.units.borrow(), &sel);
        (!text.is_empty()).then_some(text)
    }

    /// Markdown blocks as elements: monochrome, one text layout per paragraph.
    fn render_doc(&mut self, key: &str, doc: &markdown::Doc, cx: &mut Context<Self>) -> AnyElement {
        let mut col = div()
            .flex()
            .flex_col()
            .gap(u(10.))
            .text_color(rgb(theme::text()));
        for (bi, block) in doc.blocks.iter().enumerate() {
            let id = format!("md-{key}-{bi}");
            col = col.child(match block {
                MdBlock::Paragraph(s) => div()
                    .child(self.rich_text(id, s, theme::text(), FontWeight::NORMAL, "\n\n", "", cx))
                    .into_any_element(),
                MdBlock::Heading { level, spans } => div()
                    .pt(u(if *level <= 2 { 8. } else { 4. }))
                    .text_size(u(match level {
                        1 => 19.,
                        2 => 16.5,
                        _ => 14.5,
                    }))
                    .line_height(u(match level {
                        1 => 26.,
                        2 => 23.,
                        _ => 21.,
                    }))
                    .child(self.rich_text(
                        id,
                        spans,
                        theme::text(),
                        FontWeight::SEMIBOLD,
                        "\n\n",
                        "",
                        cx,
                    ))
                    .into_any_element(),
                MdBlock::Code { lang, code, open } => self.render_code(&id, lang, code, *open, cx),
                MdBlock::List { items } => {
                    let mut l = div().flex().flex_col().gap(u(4.));
                    for (ii, item) in items.iter().enumerate() {
                        let task = item.marker == "☐" || item.marker == "☑";
                        let ordered = item.marker.ends_with('.');
                        // Copied as Markdown would write it.
                        let prefix = format!(
                            "{}{} ",
                            "  ".repeat(item.depth as usize),
                            match item.marker.as_str() {
                                "☐" => "- [ ]",
                                "☑" => "- [x]",
                                m if ordered => m,
                                _ => "-",
                            }
                        );
                        l = l.child(
                            div()
                                .flex()
                                .gap(u(8.))
                                .pl(u(item.depth as f32 * 18.))
                                .child(
                                    div()
                                        .flex_none()
                                        .min_w(u(if ordered { 18. } else { 12. }))
                                        .when(ordered, |d| d.flex().justify_end())
                                        .text_color(rgb(if task {
                                            theme::text_2()
                                        } else {
                                            theme::dim()
                                        }))
                                        .child(item.marker.clone()),
                                )
                                .child(div().flex_1().min_w_0().child(self.rich_text(
                                    format!("{id}-{ii}"),
                                    &item.spans,
                                    if item.marker == "☑" {
                                        theme::text_2()
                                    } else {
                                        theme::text()
                                    },
                                    FontWeight::NORMAL,
                                    if ii == 0 { "\n\n" } else { "\n" },
                                    &prefix,
                                    cx,
                                ))),
                        );
                    }
                    l.into_any_element()
                }
                MdBlock::Quote(s) => div()
                    .pl(u(12.))
                    .border_l_2()
                    .border_color(rgb(theme::surface_hover()))
                    .child(self.rich_text(
                        id,
                        s,
                        theme::text_2(),
                        FontWeight::NORMAL,
                        "\n\n",
                        "> ",
                        cx,
                    ))
                    .into_any_element(),
                MdBlock::Rule => div()
                    .my(u(4.))
                    .h(px(1.))
                    .bg(rgba(theme::input_edge()))
                    .into_any_element(),
                MdBlock::Table { head, rows } => {
                    let mut t = div()
                        .flex()
                        .flex_col()
                        .rounded(u(8.))
                        .border_1()
                        .border_color(rgba(theme::input_edge()))
                        .overflow_hidden()
                        .text_size(u(theme::TEXT_UI))
                        .line_height(u(19.));
                    for (ri, cells) in std::iter::once(head).chain(rows.iter()).enumerate() {
                        let mut r = div()
                            .flex()
                            .gap(u(12.))
                            .px(u(10.))
                            .py(u(6.))
                            .when(ri == 0, |d| d.bg(rgb(theme::raised())))
                            .when(ri > 0, |d| {
                                d.border_t_1().border_color(rgba(theme::hairline()))
                            });
                        for (ci, c) in cells.iter().enumerate() {
                            let weight = if ri == 0 {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            };
                            let sep = match (ri, ci) {
                                (0, 0) => "\n\n",
                                (_, 0) => "\n",
                                _ => "\t",
                            };
                            r = r.child(div().flex_1().min_w_0().child(self.rich_text(
                                format!("{id}-{ri}-{ci}"),
                                c,
                                theme::text(),
                                weight,
                                sep,
                                "",
                                cx,
                            )));
                        }
                        t = t.child(r);
                    }
                    t.into_any_element()
                }
            });
        }
        col.into_any_element()
    }

    /// A fenced code block: lines never wrap (the block scrolls sideways
    /// instead), with its language and a Copy button on top.
    fn render_code(
        &mut self,
        id: &str,
        lang: &str,
        code: &str,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.copied.as_deref() == Some(id);
        let group = SharedString::from(format!("{id}-g"));
        let key = id.to_string();
        let text = code.to_string();
        div()
            .group(group.clone())
            .flex()
            .flex_col()
            .rounded(u(10.))
            .bg(rgb(theme::raised()))
            .border_1()
            .border_color(rgba(theme::hairline()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(u(28.))
                    .pl(u(12.))
                    .pr(u(4.))
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child(div().flex_1().child(if lang.is_empty() {
                        "code".to_string()
                    } else {
                        lang.to_string()
                    }))
                    .when(!open, |d| {
                        d.child(
                            div()
                                .id(ElementId::Name(format!("{id}-copy").into()))
                                .flex()
                                .items_center()
                                .gap(u(5.))
                                .h(u(22.))
                                .px(u(6.))
                                .rounded(u(6.))
                                .opacity(if copied { 1. } else { 0. })
                                .group_hover(group, |s| s.opacity(1.))
                                .cursor_pointer()
                                .hover(|s| {
                                    s.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                                })
                                .child(icon(
                                    if copied { Icon::Check } else { Icon::Copy },
                                    11.,
                                    theme::dim(),
                                ))
                                .child(if copied { "Copied" } else { "Copy" })
                                .on_click(cx.listener(move |c, _, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        text.clone(),
                                    ));
                                    c.copied = Some(key.clone());
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id(ElementId::Name(format!("{id}-scroll").into()))
                    .overflow_x_scroll()
                    .px(u(12.))
                    .pb(u(10.))
                    .font_family(theme::mono())
                    .text_size(u(12.))
                    .line_height(u(18.))
                    .text_color(rgb(theme::text()))
                    .child(div().whitespace_nowrap().child({
                        // Coloured by the fence's language, when it names one.
                        let runs = match crate::syntax::for_lang(lang) {
                            Some(grammar) => crate::syntax::runs(
                                &crate::syntax::spans(id, grammar, code),
                                theme::mono(),
                            ),
                            None => plain_runs(code.len(), theme::mono(), theme::text()),
                        };
                        let (t, rec) = self.unit_text(code.to_string().into(), runs, "\n\n", "");
                        selectable(t.into_any_element(), rec)
                    })),
            )
            .into_any_element()
    }
}

/// How a run of tool calls and thinking is laid out in a message.
enum Part {
    One(usize),
    Work(Vec<usize>),
}

/// One run over all of a plain text.
fn plain_runs(len: usize, family: &'static str, color: u32) -> Vec<TextRun> {
    if len == 0 {
        return Vec::new();
    }
    vec![TextRun {
        len,
        font: gpui::font(family),
        color: rgb(color).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }]
}

/// Selectable text with the I-beam, and the recorder laid over it.
fn selectable(text: AnyElement, rec: AnyElement) -> AnyElement {
    div()
        .relative()
        .cursor_text()
        .child(text)
        .child(rec)
        .into_any_element()
}

/// The width readable text never exceeds.
const COLUMN: f32 = 760.;

fn message_text(msg: &agent_core::Message) -> String {
    msg.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A label that breathes while the agent works on it.
fn working_label(id: String, label: SharedString) -> AnyElement {
    crate::icon::shimmer(ElementId::Name(id.into()), label)
}

/// `+12 −3`, additions bright and removals dim, in the mono face so the
/// numbers line up down a log.
fn diff_stat(added: usize, removed: usize) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .gap(u(4.))
        .font_family(theme::mono())
        .text_size(u(11.))
        .when(added > 0, |d| {
            d.child(
                div()
                    .text_color(rgb(theme::text()))
                    .child(format!("+{added}")),
            )
        })
        .when(removed > 0, |d| {
            d.child(
                div()
                    .text_color(rgb(theme::dim()))
                    .child(format!("−{removed}")),
            )
        })
        .into_any_element()
}

/// Lines added and removed by an edit, from its diff when it reported one,
/// else from what it was asked to write.
fn edit_stats(t: &ToolInvocation) -> Option<(usize, usize)> {
    if tool_kind(&t.name) != Kind::Edit {
        return None;
    }
    if let Some(ToolOutput::Diff { unified, .. }) = &t.result {
        let added = unified
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .count();
        let removed = unified
            .lines()
            .filter(|l| l.starts_with('-') && !l.starts_with("---"))
            .count();
        return Some((added, removed));
    }
    let lines =
        |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map_or(0, |s| s.lines().count());
    let i = &t.input;
    if let Some(edits) = i.get("edits").and_then(|e| e.as_array()) {
        return Some(edits.iter().fold((0, 0), |(a, r), e| {
            (
                a + lines(e.get("new_string")),
                r + lines(e.get("old_string")),
            )
        }));
    }
    if i.get("old_string").is_some() || i.get("new_string").is_some() {
        return Some((lines(i.get("new_string")), lines(i.get("old_string"))));
    }
    i.get("content").map(|c| (lines(Some(c)), 0))
}

/// An edit as a diff, built from the tool's input when it reported none.
fn edit_diff(t: &ToolInvocation) -> Option<String> {
    if let Some(ToolOutput::Diff { unified, .. }) = &t.result {
        return Some(unified.clone());
    }
    input_diff(&t.input)
}

/// The change an edit or write would make, from its input.
fn input_diff(i: &serde_json::Value) -> Option<String> {
    let pair =
        |old: Option<&serde_json::Value>, new: Option<&serde_json::Value>, out: &mut String| {
            for l in old.and_then(|v| v.as_str()).unwrap_or("").lines() {
                out.push_str(&format!("-{l}\n"));
            }
            for l in new.and_then(|v| v.as_str()).unwrap_or("").lines() {
                out.push_str(&format!("+{l}\n"));
            }
        };
    let mut out = String::new();
    if let Some(edits) = i.get("edits").and_then(|e| e.as_array()) {
        for (n, e) in edits.iter().enumerate() {
            if n > 0 {
                out.push_str("@@\n");
            }
            pair(e.get("old_string"), e.get("new_string"), &mut out);
        }
    } else if i.get("old_string").is_some() || i.get("new_string").is_some() {
        pair(i.get("old_string"), i.get("new_string"), &mut out);
    } else if let Some(c) = i.get("content") {
        pair(None, Some(c), &mut out);
    }
    (!out.is_empty()).then_some(out)
}

/// A path in a tool title, shown relative to the project when inside it.
fn relative_to(title: &str, cwd: &std::path::Path) -> String {
    let root = cwd.display().to_string();
    let short = title.replace(&format!("{root}/"), "");
    match std::env::var("HOME") {
        Ok(home) if short.starts_with(&home) => format!("~{}", &short[home.len()..]),
        _ => short,
    }
}

/// 1234 → "1.2k", 1500000 → "1.5M".
pub fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        _ => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
    }
}

/// Text runs for a paragraph: the base face, with bold, italic, struck,
/// linked and inline-code stretches over it.
fn text_runs(spans: &Spans, color: u32, weight: FontWeight) -> Vec<TextRun> {
    let base = |st: markdown::Style| {
        let mut f = gpui::font(if st.code { theme::mono() } else { theme::ui() });
        f.weight = if st.bold {
            FontWeight::SEMIBOLD
        } else {
            weight
        };
        f.style = if st.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        f
    };
    let run = |len: usize, st: markdown::Style| TextRun {
        len,
        font: base(st),
        color: rgb(if st.link || st.code {
            theme::text()
        } else if st.kbd {
            theme::text_2()
        } else {
            color
        })
        .into(),
        // Keys in the keycap's tint (see `kbd`): square, since a text run
        // can't round its corners.
        background_color: if st.code {
            Some(rgb(theme::surface()).into())
        } else if st.kbd {
            Some(rgba(theme::tint()).into())
        } else {
            None
        },
        underline: st.link.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(rgb(theme::dim()).into()),
            wavy: false,
        }),
        strikethrough: st.strike.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(rgb(theme::dim()).into()),
        }),
    };
    let mut out = Vec::new();
    let mut at = 0;
    for (r, st) in &spans.runs {
        if r.start < at || r.end > spans.text.len() {
            continue;
        }
        if r.start > at {
            out.push(run(r.start - at, markdown::Style::default()));
        }
        out.push(run(r.end - r.start, *st));
        at = r.end;
    }
    if at < spans.text.len() {
        out.push(run(spans.text.len() - at, markdown::Style::default()));
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Command,
    Edit,
    Read,
    Web,
    Other,
}

fn tool_kind(name: &str) -> Kind {
    let n = name.to_lowercase();
    // Planning tools mention "write" but touch no file.
    if n.contains("todo") || n == "task" || n.contains("plan") {
        return Kind::Other;
    }
    if n.contains("bash") || n.contains("shell") || n.contains("exec") || n.contains("command") {
        Kind::Command
    } else if n.contains("edit") || n.contains("write") || n.contains("patch") {
        Kind::Edit
    } else if n.contains("read")
        || n.contains("grep")
        || n.contains("glob")
        || n == "ls"
        || n.contains("list")
    {
        Kind::Read
    } else if n.contains("web") || n.contains("fetch") || n.contains("search") {
        Kind::Web
    } else {
        Kind::Other
    }
}

/// "Ran 2 commands and edited 3 files": at most two kinds named, commands
/// and edits first because they changed something; the rest are counted.
fn summarize(tools: &[&ToolInvocation]) -> String {
    let count = |k: Kind| tools.iter().filter(|t| tool_kind(&t.name) == k).count();
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let named: Vec<(usize, String)> = [
        (count(Kind::Command), "ran", "command", "commands"),
        (count(Kind::Edit), "edited", "file", "files"),
        (count(Kind::Read), "read", "file", "files"),
        (count(Kind::Web), "searched the web", "time", "times"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, verb, one, many)| (n, format!("{verb} {}", plural(n, one, many))))
    .collect();
    let shown: Vec<String> = named.iter().take(2).map(|(_, s)| s.clone()).collect();
    let rest = tools.len() - named.iter().take(2).map(|(n, _)| n).sum::<usize>();
    let mut out = shown.join(" and ");
    if rest > 0 {
        let other = plural(rest, "other action", "other actions");
        out = if out.is_empty() {
            format!("performed {other}")
        } else {
            format!("{out}, and {other}")
        };
    }
    let mut chars = out.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => out,
    }
}

fn tool_icon(name: &str) -> Icon {
    let n = name.to_lowercase();
    if n.contains("todo") || n.contains("plan") {
        return Icon::ListCheck;
    }
    match tool_kind(name) {
        Kind::Command => Icon::Terminal,
        Kind::Edit => Icon::Pencil,
        Kind::Web => Icon::Globe,
        _ if n.contains("grep") || n.contains("glob") || n.contains("search") => Icon::Search,
        _ if n == "ls" || n.contains("list") => Icon::Folder,
        _ => Icon::File,
    }
}

/// What a step did, as a verb and its object: "Ran" `cargo test`, "Edited"
/// `src/chat.rs`. Present tense while it runs. The object is one line: a
/// script fed to a command keeps its first line and an ellipsis, since the
/// row is one line tall.
/// `cd /the/chat/folder && cargo test` reads as `cargo test`: agents lead
/// most commands with a `cd` back into the folder the chat is already in.
fn strip_cd<'a>(cmd: &'a str, cwd: &std::path::Path) -> &'a str {
    let Some(rest) = cmd.strip_prefix("cd ") else {
        return cmd;
    };
    let Some((dir, after)) = rest.split_once(" && ").or_else(|| rest.split_once("; ")) else {
        return cmd;
    };
    let dir = dir.trim().trim_matches(|c| c == '"' || c == '\'');
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = match dir.strip_prefix('~') {
        Some(tail) => format!("{home}{tail}"),
        None => dir.to_string(),
    };
    if std::path::Path::new(dir.trim_end_matches('/')) == cwd || dir == "." {
        after.trim_start()
    } else {
        cmd
    }
}

fn step_phrase(t: &ToolInvocation, running: bool, cwd: &std::path::Path) -> (&'static str, String) {
    let (verb, object) = step_phrase_full(t, running, cwd);
    (verb, one_line(&object))
}

/// The first line of `s` with anything in it, and "…" when more follows.
pub fn one_line_pub(s: &str) -> String {
    one_line(s)
}

fn one_line(s: &str) -> String {
    let mut lines = s.lines().map(str::trim).filter(|l| !l.is_empty());
    match (lines.next(), lines.next()) {
        (Some(first), None) => first.to_string(),
        (Some(first), Some(_)) => format!("{first} …"),
        (None, _) => String::new(),
    }
}

fn step_phrase_full(t: &ToolInvocation, running: bool, cwd: &std::path::Path) -> (&'static str, String) {
    let n = t.name.to_lowercase();
    let input = |k: &str| t.input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let path = || {
        input("file_path")
            .or_else(|| input("path"))
            .or_else(|| input("notebook_path"))
            .map(|p| relative_to(&p, cwd))
            .unwrap_or_else(|| relative_to(&t.title, cwd))
    };
    let pick = |now: &'static str, done: &'static str| if running { now } else { done };
    if n.contains("todo") || n.contains("plan") {
        return (pick("Planning", "Updated the plan"), String::new());
    }
    match tool_kind(&t.name) {
        Kind::Command => (
            pick("Running", "Ran"),
            relative_to(strip_cd(&input("command").unwrap_or_else(|| t.title.clone()), cwd), cwd),
        ),
        Kind::Edit if n.contains("write") || n.contains("create") => {
            (pick("Writing", "Wrote"), path())
        }
        Kind::Edit => (pick("Editing", "Edited"), path()),
        Kind::Read if n.contains("grep") || n.contains("glob") || n.contains("search") => (
            pick("Searching", "Searched"),
            input("pattern").unwrap_or_else(|| t.title.clone()),
        ),
        Kind::Read if n == "ls" || n.contains("list") => (pick("Listing", "Listed"), path()),
        Kind::Read => (pick("Reading", "Read"), path()),
        Kind::Web if n.contains("fetch") => (
            pick("Fetching", "Fetched"),
            input("url").unwrap_or_else(|| t.title.clone()),
        ),
        Kind::Web => (
            pick("Searching the web for", "Searched the web for"),
            input("query").unwrap_or_else(|| t.title.clone()),
        ),
        Kind::Other if n == "task" || n == "agent" => {
            (pick("Delegating", "Delegated"), t.title.clone())
        }
        Kind::Other => ("", t.title.clone()),
    }
}

/// "0.8s", "12s", "2m 5s": how long a step ran.
fn step_duration(d: Duration) -> String {
    let ms = d.as_millis();
    match ms {
        0..=9_999 => format!("{:.1}s", ms as f64 / 1000.),
        10_000..=59_999 => format!("{}s", ms / 1000),
        _ => format!("{}m {}s", ms / 60_000, (ms / 1000) % 60),
    }
}

/// The checklist in a plan update (Claude's TodoWrite): each item's text
/// and state.
fn plan_items(t: &ToolInvocation) -> Option<Vec<(String, PlanState)>> {
    let todos = t.input.get("todos")?.as_array()?;
    Some(
        todos
            .iter()
            .filter_map(|todo| {
                let text = todo.get("content").and_then(|v| v.as_str())?.to_string();
                let state = match todo.get("status").and_then(|v| v.as_str()) {
                    Some("completed") => PlanState::Done,
                    Some("in_progress") => PlanState::Doing,
                    _ => PlanState::Todo,
                };
                Some((text, state))
            })
            .collect(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlanState {
    Todo,
    Doing,
    Done,
}

fn render_plan(items: &[(String, PlanState)]) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .gap(u(2.))
        .my(u(4.))
        .pl(u(26.))
        .text_size(u(theme::TEXT_UI));
    for (text, state) in items {
        col = col.child(
            div()
                .flex()
                .items_start()
                .gap(u(8.))
                .child(
                    div()
                        .flex_none()
                        .w(u(14.))
                        .text_color(rgb(theme::dim()))
                        .child(match state {
                            PlanState::Done => "☑",
                            PlanState::Doing => "◐",
                            PlanState::Todo => "☐",
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(*state == PlanState::Done, |d| {
                            d.text_color(rgb(theme::dim())).line_through()
                        })
                        .when(*state == PlanState::Doing, |d| {
                            d.text_color(rgb(theme::text()))
                                .font_weight(FontWeight::MEDIUM)
                        })
                        .when(*state == PlanState::Todo, |d| {
                            d.text_color(rgb(theme::text_2()))
                        })
                        .child(text.clone()),
                ),
        );
    }
    col.into_any_element()
}

/// What an approval card says the agent wants ("run a command", "edit
/// src/a.rs") and what it shows of it: the command, or the change.
fn permission_preview(
    action: &Action,
    input: &serde_json::Value,
    tool: &str,
    cwd: &std::path::Path,
) -> (String, Option<AnyElement>) {
    let diff = || input_diff(input).map(|d| render_output(&d, true, 14));
    match action {
        Action::Bash { command, .. } => (
            "run a command".into(),
            Some(render_command(command, "", false, true)),
        ),
        Action::EditFile { path } => (format!("edit {}", relative_to(path, cwd)), diff()),
        Action::WriteFile { path } => (format!("write {}", relative_to(path, cwd)), diff()),
        Action::ReadFile { path } => (format!("read {}", relative_to(path, cwd)), None),
        Action::Fetch { url } => (format!("fetch {url}"), None),
        Action::McpCall { server, tool, .. } => (
            format!("use {tool} from {server}"),
            Some(render_output(
                &serde_json::to_string_pretty(input).unwrap_or_default(),
                false,
                10,
            )),
        ),
        Action::Other { .. } if tool == "ExitPlanMode" => {
            let plan = input
                .get("plan")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            (
                "start on this plan".into(),
                Some(render_output(&plan, false, 30)),
            )
        }
        Action::Other { name, .. } => (
            format!("use {name}"),
            Some(render_output(
                &serde_json::to_string_pretty(input).unwrap_or_default(),
                false,
                10,
            )),
        ),
    }
}

/// A command as it would look in a terminal: `$ command`, then its output.
fn render_command(command: &str, output: &str, failed: bool, running: bool) -> AnyElement {
    let lines: Vec<&str> = output.lines().collect();
    let skip = lines.len().saturating_sub(40);
    let mut col = div()
        .flex()
        .flex_col()
        .my(u(4.))
        .px(u(10.))
        .py(u(8.))
        .rounded(u(8.))
        .bg(rgb(theme::raised()))
        .border_1()
        .border_color(rgba(if failed {
            theme::strong_edge()
        } else {
            theme::hairline()
        }))
        .font_family(theme::mono())
        .text_size(u(theme::TEXT_SMALL))
        .line_height(u(17.))
        .overflow_x_hidden();
    for (i, line) in command.lines().enumerate() {
        col = col.child(
            div()
                .flex()
                .gap(u(8.))
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::dim()))
                        .child(if i == 0 { "$" } else { " " }),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_color(rgb(theme::text()))
                        .child(line.to_string()),
                ),
        );
    }
    if skip > 0 {
        col = col.child(
            div()
                .mt(u(4.))
                .text_color(rgb(theme::dim()))
                .child(format!("… {skip} earlier lines")),
        );
    }
    for line in &lines[skip..] {
        col = col.child(
            div()
                .whitespace_nowrap()
                .text_color(rgb(if failed {
                    theme::text()
                } else {
                    theme::text_2()
                }))
                .child(line.to_string()),
        );
    }
    if lines.is_empty() && !running {
        col = col.child(div().text_color(rgb(theme::dim())).child("(no output)"));
    }
    col.into_any_element()
}

/// Content that eases in as it appears, so opening a log or a step does not
/// pop.
/// Fades in while rising a few points into place, for steps and lines that
/// arrive while the agent works.
fn rise_in(id: String, el: AnyElement) -> AnyElement {
    div()
        .child(el)
        .with_animation(
            ElementId::Name(id.into()),
            Animation::new(Duration::from_millis(220)).with_easing(gpui::ease_in_out),
            |d, t| d.opacity(t).mt(u(6. * (1. - t))),
        )
        .into_any_element()
}

fn fade_in(id: String, el: AnyElement) -> AnyElement {
    div()
        .child(el)
        .with_animation(
            ElementId::Name(id.into()),
            Animation::new(Duration::from_millis(180)).with_easing(gpui::ease_in_out),
            |d, t| d.opacity(t),
        )
        .into_any_element()
}

struct ChangedFile {
    path: String,
    added: usize,
    removed: usize,
    /// The edits that touched it, by block index.
    edits: Vec<usize>,
}

impl ChangedFile {
    fn diff(&self, blocks: &[Block]) -> String {
        let parts: Vec<String> = self
            .edits
            .iter()
            .filter_map(|&b| match &blocks[b] {
                Block::Tool(t) => edit_diff(t),
                _ => None,
            })
            .collect();
        parts.join("@@\n")
    }
}

/// Every file a reply's edits touched, in the order first touched, with
/// their changes added up.
fn changed_files(blocks: &[Block], cwd: &std::path::Path) -> Vec<ChangedFile> {
    let mut out: Vec<ChangedFile> = Vec::new();
    for (bix, b) in blocks.iter().enumerate() {
        let Block::Tool(t) = b else { continue };
        if t.is_error || t.is_open() {
            continue;
        }
        let Some((added, removed)) = edit_stats(t) else {
            continue;
        };
        let path = ["file_path", "path", "notebook_path"]
            .iter()
            .find_map(|k| t.input.get(*k).and_then(|v| v.as_str()))
            .map(|p| relative_to(p, cwd))
            .unwrap_or_else(|| relative_to(&t.title, cwd));
        match out.iter_mut().find(|f| f.path == path) {
            Some(f) => {
                f.added += added;
                f.removed += removed;
                f.edits.push(bix);
            }
            None => out.push(ChangedFile {
                path,
                added,
                removed,
                edits: vec![bix],
            }),
        }
    }
    out
}

fn handoff_label(mode: agent_core::HydrateMode) -> &'static str {
    use agent_core::HydrateMode as H;
    match mode {
        H::NativeResume => "resumed",
        H::Replayed => "with the full thread",
        H::ReplayedPartial { .. } => "with the recent thread",
        H::Summarized => "with a summary",
        H::Fresh => "fresh",
    }
}

pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => "under a second".into(),
        1..=59 => format!("{s}s"),
        _ => format!("{}m {}s", s / 60, s % 60),
    }
}

/// A folder as a short label: `~/deck`, or for a deep one its last two
/// parts after an ellipsis (`…/f3691f90/scratchpad`).
fn tidy_place(p: &std::path::Path) -> String {
    let s = short_path(p);
    if s == "~" {
        return "Home".into();
    }
    let parts: Vec<&str> = s.split('/').filter(|x| !x.is_empty()).collect();
    if s.chars().count() <= 32 || parts.len() <= 3 {
        return s;
    }
    let tail = parts[parts.len() - 2..].join("/");
    let tail = if tail.chars().count() > 40 { parts[parts.len() - 1].to_string() } else { tail };
    format!("…/{tail}")
}

pub fn short_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if s.starts_with(&home) => format!("~{}", &s[home.len()..]),
        _ => s,
    }
}

/// A tool's output: the last `keep` lines in mono; a diff shows additions
/// in full white and removals dimmed and struck, so it reads without colour.
fn render_output(text: &str, diff: bool, keep: usize) -> AnyElement {
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(keep);
    let mut col = div()
        .flex()
        .flex_col()
        .my(u(4.))
        .px(u(10.))
        .py(u(8.))
        .rounded(u(8.))
        .bg(rgb(theme::raised()))
        .font_family(theme::mono())
        .text_size(u(theme::TEXT_SMALL))
        .line_height(u(17.));
    if skip > 0 {
        col = col.child(
            div()
                .text_color(rgb(theme::dim()))
                .child(format!("… {skip} earlier lines")),
        );
    }
    for line in &lines[skip..] {
        let el = div().whitespace_nowrap().child(line.to_string());
        col = col.child(if !diff {
            el.text_color(rgb(theme::text_2()))
        } else if line.starts_with('+') && !line.starts_with("+++") {
            el.text_color(rgb(theme::text())).bg(rgba(theme::tint()))
        } else if line.starts_with('-') && !line.starts_with("---") {
            el.text_color(rgb(theme::dim())).line_through()
        } else {
            el.text_color(rgb(theme::dim()))
        });
    }
    col.overflow_x_hidden().into_any_element()
}

/// A file read: its last `keep` lines in mono, coloured by the file's
/// language. The line numbers the Read tool puts in front stay dim and out of
/// the parse, so the code is read as the file has it.
fn render_excerpt(key: &str, grammar: &'static crate::syntax::Grammar, text: &str, keep: usize) -> AnyElement {
    let lines: Vec<&str> = text.lines().collect();
    let numbers: Vec<usize> = lines.iter().map(|l| line_number_len(l)).collect();
    let mut code = String::with_capacity(text.len());
    for (line, n) in lines.iter().zip(&numbers) {
        code.push_str(&line[*n..]);
        code.push('\n');
    }
    let spans = crate::syntax::spans(&format!("excerpt-{key}"), grammar, &code);
    let by_line = crate::syntax::by_line(&code, &spans);
    let skip = lines.len().saturating_sub(keep);
    let mut col = div()
        .flex()
        .flex_col()
        .my(u(4.))
        .px(u(10.))
        .py(u(8.))
        .rounded(u(8.))
        .bg(rgb(theme::raised()))
        .font_family(theme::mono())
        .text_size(u(theme::TEXT_SMALL))
        .line_height(u(17.));
    if skip > 0 {
        col = col.child(
            div()
                .text_color(rgb(theme::dim()))
                .child(format!("… {skip} earlier lines")),
        );
    }
    for (i, line) in lines.iter().enumerate().skip(skip) {
        let kinds: Vec<_> = by_line
            .get(i)
            .into_iter()
            .flatten()
            .map(|(range, kind)| (range.len(), *kind))
            .collect();
        let mut runs = plain_runs(numbers[i], theme::mono(), theme::dim());
        runs.extend(crate::syntax::runs(&kinds, theme::mono()));
        col = col.child(
            div()
                .whitespace_nowrap()
                .child(StyledText::new(line.to_string()).with_runs(theme::safe_runs(line, runs))),
        );
    }
    col.overflow_x_hidden().into_any_element()
}

/// Bytes of the line number the Read tool puts before a line (`   12→` or
/// `   12` and a tab), or 0 when it has none.
fn line_number_len(line: &str) -> usize {
    let pad = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[pad..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return 0;
    }
    match rest[digits..].chars().next() {
        Some(c @ ('→' | '\t')) => pad + digits + c.len_utf8(),
        _ => 0,
    }
}

impl Render for ChatPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.transcript.messages.is_empty();
        // Refilled as the transcript's text is painted below.
        self.painted.borrow_mut().clear();
        // Text still coming out on screen: draw again next frame.
        if self.revealing.replace(false) {
            window.request_animation_frame();
        }
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::pane()))
            .text_size(u(theme::TEXT_READ))
            .line_height(u(21.))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|c, ev: &gpui::MouseDownEvent, _, cx| {
                    c.select_down(ev, cx);
                    cx.emit(ChatEvent::Focused)
                }),
            )
            .on_mouse_move(cx.listener(|c, ev: &gpui::MouseMoveEvent, _, cx| {
                if c.selecting && ev.pressed_button == Some(gpui::MouseButton::Left) {
                    c.select_drag(ev.position, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|c, _, _, cx| c.select_up(cx)),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|c, _, _, cx| c.select_up(cx)),
            )
            // ⌘C copies the transcript's selection before the composer,
            // which has the keyboard, can take it.
            .capture_action(cx.listener(|c, _: &crate::workspace::Copy, _, cx| {
                if let Some(text) = c.selected_text() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    cx.stop_propagation();
                }
            }))
            // Files dragged in from Finder are attached to the next message.
            .on_drop(cx.listener(|c, paths: &ExternalPaths, window, cx| {
                c.attach_paths(paths.paths(), cx);
                window.focus(&c.composer.read(cx).focus);
            }))
            .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(rgb(theme::raised())))
            .children(self.menu.as_ref().map(menu::Open::render))
            .when(empty, |d| d.child(self.render_empty(window, cx)))
            .when(!empty, |d| {
                d.child(
                    div()
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(
                            list(
                                self.list.clone(),
                                cx.processor(|c, ix, window, cx| c.render_message(ix, window, cx)),
                            )
                            .flex_1()
                            .pb(u(16.)),
                        )
                        .children(self.render_sticky_prompt(cx))
                        .children(self.render_outline(cx)),
                )
                .child(
                    div()
                        .relative()
                        .flex_none()
                        .pb(u(12.))
                        .children(self.render_jump(cx))
                        .children(self.render_console(cx))
                        .child(Self::column(self.render_composer(window, cx))),
                )
            })
            .children(self.render_preview(cx))
    }
}

/// A thread saved mid-turn comes back finished: nothing still streaming, no
/// step spinning forever, and the last reply closed so the next turn starts
/// a message of its own.
fn settle(t: &mut Transcript) {
    for m in &mut t.messages {
        for b in &mut m.blocks {
            match b {
                Block::Text { open, .. } | Block::Reasoning { open, .. } => *open = false,
                Block::Tool(tool) if tool.result.is_none() => {
                    tool.result = Some(ToolOutput::Text("Stopped when deck quit".into()));
                    tool.is_error = true;
                }
                _ => {}
            }
        }
        if m.role == Role::Assistant && m.stop.is_none() {
            m.stop = Some(StopReason::Interrupted);
        }
    }
}

fn uuid_like() -> String {
    SessionId::new().0.to_string()
}

/// How long a listing of the project's files is trusted.
const FILES_STALE: Duration = Duration::from_secs(20);

/// Most files offered to `@`; past this a project is better found by typing.
const MAX_FILES: usize = 50_000;

/// Listings by project, shared by every ⌘K, so a big repository is not
/// listed again each time the palette opens.
#[allow(clippy::type_complexity)]
static FILE_CACHE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, (Instant, Arc<Vec<String>>)>>,
> = std::sync::LazyLock::new(Default::default);

/// `project_files`, from the cache when it has the project. One older than
/// `FILES_STALE` is still returned at once and listed again behind it, for
/// next time. Blocking on a miss: call it off the main thread.
pub fn project_files_cached(cwd: &std::path::Path) -> Arc<Vec<String>> {
    let cache = || FILE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let hit = cache().get(cwd).cloned();
    if let Some((at, files)) = hit {
        if at.elapsed() > FILES_STALE {
            // Marked fresh first, so opening ⌘K twice lists it once.
            cache().insert(cwd.to_path_buf(), (Instant::now(), files.clone()));
            let cwd = cwd.to_path_buf();
            std::thread::spawn(move || {
                let files = Arc::new(project_files(&cwd));
                FILE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(cwd, (Instant::now(), files));
            });
        }
        return files;
    }
    let files = Arc::new(project_files(cwd));
    cache().insert(cwd.to_path_buf(), (Instant::now(), files.clone()));
    files
}

/// Paths under `cwd`, relative to it, with each folder listed once (ending
/// in `/`) so folders can be mentioned too.
pub fn project_files(cwd: &std::path::Path) -> Vec<String> {
    let git = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success());
    let mut files: Vec<String> = match git {
        Some(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .take(MAX_FILES)
            .map(str::to_string)
            .collect(),
        None => walk(cwd),
    };
    let mut dirs: HashSet<String> = HashSet::new();
    for f in &files {
        let mut p = f.as_str();
        while let Some(i) = p.rfind('/') {
            p = &p[..i];
            if !dirs.insert(format!("{p}/")) {
                break;
            }
        }
    }
    files.extend(dirs);
    files
}

fn walk(root: &std::path::Path) -> Vec<String> {
    const SKIP: [&str; 6] = [
        "node_modules",
        "target",
        "dist",
        "build",
        ".git",
        "__pycache__",
    ];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                if depth < 8 {
                    stack.push((path, depth + 1));
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
                if out.len() >= MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// The best `n` paths for what was typed after `@`: a match in the file name
/// beats one in the folders, a contiguous match beats letters scattered
/// along the path, and shorter names, then paths, win ties.
fn rank_files(files: &[String], query: &str, n: usize) -> Vec<String> {
    let q = query.to_lowercase();
    let mut scored: Vec<(usize, &String)> = files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            let name = lower
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(&lower);
            let score = if q.is_empty() {
                // Nothing typed yet: top-level things first.
                f.trim_end_matches('/').matches('/').count() * 100
            } else if name.starts_with(&q) {
                0
            } else if name.contains(&q) {
                100
            } else if lower.contains(&q) {
                200
            } else {
                1000 + subsequence_gaps(&lower, &q)?
            };
            // Among equal matches, the shorter name is the closer one.
            Some((score + name.len() * 4 + f.len(), f))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, f)| f.clone()).collect()
}

/// How spread out `q`'s letters are along `s`, or `None` if they are not
/// all there in order.
fn subsequence_gaps(s: &str, q: &str) -> Option<usize> {
    let mut gaps = 0;
    let mut it = s.chars();
    for qc in q.chars() {
        let mut skipped = 0;
        loop {
            match it.next() {
                Some(c) if c == qc => break,
                Some(_) => skipped += 1,
                None => return None,
            }
        }
        gaps += skipped;
    }
    Some(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cd_into_the_chat_folder_is_left_out() {
        let cwd = std::path::Path::new("/home/me/deck");
        assert_eq!(strip_cd("cd /home/me/deck && git status", cwd), "git status");
        assert_eq!(strip_cd("cd '/home/me/deck'; ls", cwd), "ls");
        assert_eq!(strip_cd("cd /tmp && ls", cwd), "cd /tmp && ls");
        assert_eq!(strip_cd("cargo test", cwd), "cargo test");
    }
    use agent_core::ToolId;

    fn tool(name: &str) -> ToolInvocation {
        ToolInvocation {
            id: ToolId::new(name),
            name: name.into(),
            input: serde_json::Value::Null,
            title: name.into(),
            progress: String::new(),
            result: None,
            is_error: false,
        }
    }

    #[test]
    fn work_logs_name_what_changed_first_and_count_the_rest() {
        let t: Vec<ToolInvocation> = ["Read", "Read", "Bash", "Edit", "Edit", "Edit", "TodoWrite"]
            .iter()
            .map(|n| tool(n))
            .collect();
        let refs: Vec<&ToolInvocation> = t.iter().collect();
        assert_eq!(
            summarize(&refs),
            "Ran 1 command and edited 3 files, and 3 other actions"
        );
        let reads: Vec<ToolInvocation> = ["Grep", "Glob"].iter().map(|n| tool(n)).collect();
        assert_eq!(summarize(&reads.iter().collect::<Vec<_>>()), "Read 2 files");
        assert_eq!(summarize(&[]), "");
        let odd = [tool("TodoWrite")];
        assert_eq!(
            summarize(&odd.iter().collect::<Vec<_>>()),
            "Performed 1 other action"
        );
    }

    #[test]
    fn mentions_rank_file_names_before_folders_and_scattered_letters() {
        let files: Vec<String> = [
            "crates/app/src/chat.rs",
            "crates/app/src/chat_render_old.rs",
            "docs/chatting.md",
            "crates/agent-core/src/transcript.rs",
            "crates/",
            "crates/app/",
            "README.md",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = rank_files(&files, "chat", 3);
        assert_eq!(
            got,
            [
                "crates/app/src/chat.rs",
                "docs/chatting.md",
                "crates/app/src/chat_render_old.rs"
            ]
        );
        // Letters in order, spread along the path.
        assert_eq!(
            rank_files(&files, "trsc", 1),
            ["crates/agent-core/src/transcript.rs"]
        );
        assert!(rank_files(&files, "zzz", 5).is_empty());
        // Nothing typed: shallow things first.
        assert_eq!(rank_files(&files, "", 2), ["crates/", "README.md"]);
    }

    #[test]
    fn edits_count_lines_from_their_input() {
        let mut t = tool("Edit");
        t.input = serde_json::json!({"file_path": "/a.rs", "old_string": "a\nb", "new_string": "a\nb\nc"});
        assert_eq!(edit_stats(&t), Some((3, 2)));
        assert_eq!(edit_diff(&t).as_deref(), Some("-a\n-b\n+a\n+b\n+c\n"));
        let mut w = tool("Write");
        w.input = serde_json::json!({"file_path": "/a.rs", "content": "x\ny\n"});
        assert_eq!(edit_stats(&w), Some((2, 0)));
        let mut m = tool("MultiEdit");
        m.input = serde_json::json!({"edits": [{"old_string": "a", "new_string": "b"}, {"old_string": "", "new_string": "c\nd"}]});
        assert_eq!(edit_stats(&m), Some((3, 1)));
        assert_eq!(edit_stats(&tool("Bash")), None);
    }

    #[test]
    fn counts_and_paths_read_short() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(1000), "1k");
        assert_eq!(compact_count(495_400), "495.4k");
        assert_eq!(compact_count(2_000_000), "2M");
        let cwd = std::path::Path::new("/w/proj");
        assert_eq!(relative_to("/w/proj/src/a.rs", cwd), "src/a.rs");
        assert_eq!(relative_to("cat /w/proj/x && ls", cwd), "cat x && ls");
    }

    #[test]
    fn inline_styles_become_runs_that_cover_the_text() {
        let doc = markdown::parse("a `code` and **b** [l](https://x.dev)");
        let MdBlock::Paragraph(s) = &doc.blocks[0] else {
            panic!()
        };
        let runs = text_runs(s, theme::text(), FontWeight::NORMAL);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), s.text.len());
        let code = runs
            .iter()
            .find(|r| r.font.family.as_ref() == theme::mono())
            .unwrap();
        assert_eq!(code.len, "code".len());
        assert!(runs.iter().any(|r| r.underline.is_some()));
    }

    #[test]
    fn a_thread_saved_mid_turn_comes_back_settled() {
        let mut t = Transcript::default();
        t.push_user("go".into(), vec![], 1);
        let agent = AgentId::new("claude");
        t.apply(
            &agent,
            &AgentEvent::Text {
                delta: "half".into(),
            },
            2,
        );
        let mut open = tool("Bash");
        open.result = None;
        t.messages[1].blocks.push(Block::Tool(open));
        settle(&mut t);
        let m = &t.messages[1];
        assert_eq!(m.stop, Some(StopReason::Interrupted));
        assert!(m.blocks.iter().all(|b| !b.is_open()));
    }

    #[test]
    fn steps_read_as_what_they_did() {
        let cwd = std::path::Path::new("/w/p");
        let mut bash = tool("Bash");
        bash.input = serde_json::json!({"command": "cargo test -p app"});
        assert_eq!(
            step_phrase(&bash, true, cwd),
            ("Running", "cargo test -p app".to_string())
        );
        assert_eq!(step_phrase(&bash, false, cwd).0, "Ran");
        assert_eq!(one_line("\n  cd /w && python3 - <<'EOF'\nprint(1)\nEOF\n"), "cd /w && python3 - <<'EOF' …");
        assert_eq!(one_line("cargo test"), "cargo test");
        assert_eq!(one_line("  \n "), "");
        let mut edit = tool("Edit");
        edit.input =
            serde_json::json!({"file_path": "/w/p/src/a.rs", "old_string": "a", "new_string": "b"});
        assert_eq!(
            step_phrase(&edit, false, cwd),
            ("Edited", "src/a.rs".to_string())
        );
        let mut grep = tool("Grep");
        grep.input = serde_json::json!({"pattern": "fn main"});
        assert_eq!(
            step_phrase(&grep, false, cwd),
            ("Searched", "fn main".to_string())
        );
        assert_eq!(
            step_phrase(&tool("TodoWrite"), false, cwd).0,
            "Updated the plan"
        );
    }

    #[test]
    fn plans_and_changed_files_come_out_of_tool_inputs() {
        let mut plan = tool("TodoWrite");
        plan.input = serde_json::json!({"todos": [
            {"content": "a", "status": "completed"},
            {"content": "b", "status": "in_progress"},
            {"content": "c", "status": "pending"}]});
        let items = plan_items(&plan).unwrap();
        assert_eq!(
            items.iter().map(|i| i.1).collect::<Vec<_>>(),
            [PlanState::Done, PlanState::Doing, PlanState::Todo]
        );
        assert!(plan_items(&tool("Bash")).is_none());

        let edit = |path: &str, old: &str, new: &str| {
            let mut t = tool("Edit");
            t.id = agent_core::ToolId::new(format!("{path}{old}"));
            t.input = serde_json::json!({"file_path": path, "old_string": old, "new_string": new});
            t.result = Some(ToolOutput::Text("ok".into()));
            Block::Tool(t)
        };
        let blocks = vec![
            edit("/w/p/a.rs", "x", "y\nz"),
            edit("/w/p/b.rs", "", "q"),
            edit("/w/p/a.rs", "m", "n"),
        ];
        let files = changed_files(&blocks, std::path::Path::new("/w/p"));
        let got: Vec<(&str, usize, usize)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.added, f.removed))
            .collect();
        assert_eq!(got, [("a.rs", 3, 2), ("b.rs", 1, 0)]);
    }

    #[test]
    fn step_durations_stay_short() {
        assert_eq!(step_duration(Duration::from_millis(850)), "0.8s");
        assert_eq!(step_duration(Duration::from_millis(12_400)), "12s");
        assert_eq!(step_duration(Duration::from_secs(125)), "2m 5s");
    }

    #[test]
    fn durations_read_like_people_say_them() {
        assert_eq!(human_duration(Duration::from_millis(400)), "under a second");
        assert_eq!(human_duration(Duration::from_secs(14)), "14s");
        assert_eq!(human_duration(Duration::from_secs(133)), "2m 13s");
    }

    #[test]
    fn read_line_numbers_are_measured_in_both_forms() {
        assert_eq!(line_number_len("     1→fn main() {"), "     1→".len());
        assert_eq!(line_number_len("    12\tlet x = 1;"), 7);
        assert_eq!(line_number_len("1234→"), "1234→".len());
        // Code that happens to start with digits, or no numbers at all.
        assert_eq!(line_number_len("42 is the answer"), 0);
        assert_eq!(line_number_len("    let x = 1;"), 0);
        assert_eq!(line_number_len(""), 0);
    }
}
