//! Drive one agent CLI through its deck adapter, headless, and print every
//! event with the milliseconds since the turn was sent.
//!
//! ```text
//! cargo run -p agent-cli --example drive -- <agent> <dir> <ask|edit|yolo> <prompt>...
//! ```
//! A prompt written `!stop=3000 <text>` is interrupted after 3 s. Set
//! `DECK_RESUME=<id>` to re-attach to a session from an earlier run.

use agent_cli::CliAdapter;
use agent_core::{AgentAdapter, AgentEvent, PermissionMode, SessionCtx, UserTurn};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn short(ev: &AgentEvent) -> String {
    let s = match ev {
        AgentEvent::Text { delta } => format!("Text {delta:?}"),
        AgentEvent::Reasoning { delta } => format!("Reasoning {delta:?}"),
        other => format!("{other:?}"),
    };
    s.chars().take(220).collect()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: drive <agent> <dir> <ask|edit|yolo> <prompt>...");
        std::process::exit(2);
    }
    let spec = agent_cli::spec::ALL
        .iter()
        .find(|s| s.id == args[0])
        .unwrap_or_else(|| panic!("no agent {}", args[0]));
    let mode = match args[2].as_str() {
        "ask" => PermissionMode::Ask,
        "edit" => PermissionMode::Edit,
        _ => PermissionMode::Yolo,
    };
    let ctx = SessionCtx {
        session_id: agent_core::SessionId::new(),
        cwd: PathBuf::from(&args[1]),
        system_prompt: Arc::from(""),
        model: std::env::var("DECK_MODEL").ok().map(Into::into),
        mcp: Arc::new(vec![]),
        skills_dir: None,
        runtime_dir: std::env::temp_dir(),
        resume: std::env::var("DECK_RESUME").ok(),
        permission_mode: mode,
        env: Vec::new(),
        allowed_tools: Vec::new(),
        remote: None,
        disallowed_tools: Vec::new(),
        host_only: false,
    };
    let adapter = CliAdapter::new(spec);
    let opened = Instant::now();
    let mut session = adapter.spawn(ctx).await.expect("spawn");
    if let Ok(ms) = std::env::var("DECK_WAIT_MS") {
        // Like a person typing the first message after opening the chat.
        tokio::time::sleep(Duration::from_millis(ms.parse().unwrap_or(0))).await;
    }
    for raw in &args[3..] {
        let (stop, prompt) = match raw.strip_prefix("!stop=") {
            Some(rest) => {
                let (ms, text) = rest.split_once(' ').unwrap_or((rest, ""));
                (
                    Some(Duration::from_millis(ms.parse().unwrap_or(3000))),
                    text,
                )
            }
            None => (None, raw.as_str()),
        };
        println!(
            "== send {prompt:?} ({} ms after open)",
            opened.elapsed().as_millis()
        );
        let sent = Instant::now();
        session.send(UserTurn::text(prompt)).await.expect("send");
        let mut first = None;
        let mut stopped = false;
        loop {
            let wait = match stop {
                Some(d) if !stopped => d.saturating_sub(sent.elapsed()),
                _ => Duration::from_secs(300),
            };
            let next = {
                let mut events = session.events();
                tokio::time::timeout(wait, events.next()).await
            };
            match next {
                Ok(Some(ev)) => {
                    let ms = sent.elapsed().as_millis();
                    if first.is_none()
                        && matches!(
                            ev,
                            AgentEvent::Text { .. }
                                | AgentEvent::Reasoning { .. }
                                | AgentEvent::ToolCall { .. }
                        )
                    {
                        first = Some(ms);
                    }
                    println!("{ms:>7} {}", short(&ev));
                    if let (AgentEvent::Permission { id, .. }, Ok(answer)) =
                        (&ev, std::env::var("DECK_DECIDE"))
                    {
                        let d = match answer.as_str() {
                            "deny" => agent_core::Decision::Deny,
                            _ => agent_core::Decision::Allow,
                        };
                        println!("{ms:>7} -- decide {d:?}");
                        session.decide(*id, d).await.expect("decide");
                    }
                    if matches!(ev, AgentEvent::TurnEnd { .. } | AgentEvent::Fatal { .. }) {
                        break;
                    }
                }
                Ok(None) => {
                    println!("stream ended");
                    break;
                }
                Err(_) if stop.is_some() && !stopped => {
                    stopped = true;
                    println!("{:>7} -- interrupt", sent.elapsed().as_millis());
                    session.interrupt().await.expect("interrupt");
                }
                Err(_) => {
                    println!("timed out");
                    break;
                }
            }
        }
        println!(
            "== first content after {first:?} ms, turn took {} ms",
            sent.elapsed().as_millis()
        );
    }
    println!("== native session {:?}", session.native_session_id());
}
