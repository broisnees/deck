//! The bridge between the async agent world and the UI thread.
//!
//! Adapters live on a tokio runtime (they own child processes and sockets); the
//! UI lives on gpui's foreground executor. This module owns exactly one
//! crossing point: commands go in on a tokio channel, events come out on a
//! futures channel the UI can await.

use agent_core::{AgentAdapter, AgentEvent, AgentSession, SessionCtx, Transcript, UserTurn};
use futures::channel::mpsc as fmpsc;
use futures::future::{select, Either};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::mpsc as tmpsc;

/// Requests the UI can make of a running session.
#[derive(Debug)]
pub enum Cmd {
    Send(UserTurn),
    Interrupt,
    /// Answer a permission request.
    Decide(agent_core::RequestId, agent_core::Decision),
    /// Answer a question: (question, answer) pairs.
    Answer(agent_core::RequestId, Vec<(String, String)>),
    /// Stop the driver and drop the backend.
    Shutdown,
}

/// Handle to a live session. Dropping it shuts the backend down, because the
/// driver task exits when the command channel closes.
pub struct Conn {
    cmd: tmpsc::UnboundedSender<Cmd>,
    /// Taken exactly once, by whoever drives the UI update loop.
    events: Option<fmpsc::UnboundedReceiver<DriverEvent>>,
}

/// Events the UI consumes: adapter events plus driver-level lifecycle news.
// `Agent` dwarfs the other variants, but boxing it would allocate once per
// streamed token on the hottest path; the rare small variants can carry the
// padding instead.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum DriverEvent {
    Agent(AgentEvent),
    /// Emitted once the backend has absorbed a prior transcript.
    Hydrated(agent_core::HydrateMode),
    /// The driver stopped; no further events will arrive.
    Ended {
        error: Option<String>,
    },
}

impl Conn {
    /// Hand the event stream to the consumer. Returns `None` on a second call.
    pub fn take_events(&mut self) -> Option<fmpsc::UnboundedReceiver<DriverEvent>> {
        self.events.take()
    }

    pub fn send(&self, turn: UserTurn) -> bool {
        self.cmd.send(Cmd::Send(turn)).is_ok()
    }

    pub fn interrupt(&self) -> bool {
        self.cmd.send(Cmd::Interrupt).is_ok()
    }

    pub fn decide(&self, id: agent_core::RequestId, decision: agent_core::Decision) -> bool {
        self.cmd.send(Cmd::Decide(id, decision)).is_ok()
    }

    pub fn answer(&self, id: agent_core::RequestId, answers: Vec<(String, String)>) -> bool {
        self.cmd.send(Cmd::Answer(id, answers)).is_ok()
    }

    pub fn shutdown(&self) {
        let _ = self.cmd.send(Cmd::Shutdown);
    }
}

/// Spawn a session on the given tokio handle.
///
/// `hydrate_with` is the canonical transcript to hand the backend before the
/// first turn — this is what makes mid-thread agent switching work.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    adapter: Arc<dyn AgentAdapter>,
    ctx: SessionCtx,
    hydrate_with: Option<Transcript>,
) -> Conn {
    let (cmd_tx, cmd_rx) = tmpsc::unbounded_channel();
    let (ev_tx, ev_rx) = fmpsc::unbounded();

    rt.spawn(async move {
        let err = run(adapter, ctx, hydrate_with, cmd_rx, &ev_tx).await;
        let _ = ev_tx.unbounded_send(DriverEvent::Ended {
            error: err.map(|e| e.to_string()),
        });
    });

    Conn {
        cmd: cmd_tx,
        events: Some(ev_rx),
    }
}

async fn run(
    adapter: Arc<dyn AgentAdapter>,
    ctx: SessionCtx,
    hydrate_with: Option<Transcript>,
    mut cmd_rx: tmpsc::UnboundedReceiver<Cmd>,
    ev_tx: &fmpsc::UnboundedSender<DriverEvent>,
) -> Option<agent_core::AgentError> {
    let mut session: Box<dyn AgentSession> = match adapter.spawn(ctx).await {
        Ok(s) => s,
        Err(e) => return Some(e),
    };

    if let Some(prior) = hydrate_with {
        match session.hydrate(&prior).await {
            Ok(mode) => {
                let _ = ev_tx.unbounded_send(DriverEvent::Hydrated(mode));
            }
            Err(e) => return Some(e),
        }
    }

    loop {
        // The event stream borrows `session` mutably, and so do `send` and
        // `interrupt` — so the race is resolved into an owned `Step` inside this
        // block, releasing the borrow before we act on a command.
        let step = {
            let mut events = session.events();
            let next_event = events.next();
            let next_cmd = Box::pin(cmd_rx.recv());
            match select(next_event, next_cmd).await {
                Either::Left((Some(ev), _)) => Step::Event(ev),
                Either::Left((None, _)) => Step::EventsClosed,
                Either::Right((Some(cmd), _)) => Step::Cmd(cmd),
                Either::Right((None, _)) => Step::CmdClosed,
            }
        };

        match step {
            Step::Event(ev) => {
                let fatal = matches!(ev, AgentEvent::Fatal { .. });
                if ev_tx.unbounded_send(DriverEvent::Agent(ev)).is_err() {
                    return None; // UI dropped the session
                }
                if fatal {
                    return None;
                }
            }
            Step::Cmd(Cmd::Send(turn)) => {
                if let Err(e) = session.send(turn).await {
                    return Some(e);
                }
            }
            Step::Cmd(Cmd::Interrupt) => {
                if let Err(e) = session.interrupt().await {
                    tracing::warn!(error = %e, "interrupt failed");
                }
            }
            Step::Cmd(Cmd::Decide(id, decision)) => {
                if let Err(e) = session.decide(id, decision).await {
                    tracing::warn!(error = %e, "permission answer failed");
                }
            }
            Step::Cmd(Cmd::Answer(id, answers)) => {
                if let Err(e) = session.answer(id, answers).await {
                    tracing::warn!(error = %e, "question answer failed");
                }
            }
            Step::Cmd(Cmd::Shutdown) | Step::CmdClosed => return None,
            Step::EventsClosed => return None,
        }
    }
}

enum Step {
    Event(AgentEvent),
    Cmd(Cmd),
    EventsClosed,
    CmdClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        AgentError, AgentId, Availability, Caps, Decision, HydrateMode, RequestId, StopReason,
    };
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::path::PathBuf;

    /// A scripted backend: replays a fixed event list, and echoes each user turn
    /// back as a text event so command plumbing is observable.
    struct FakeAdapter {
        script: Vec<AgentEvent>,
    }

    struct FakeSession {
        rx: tmpsc::UnboundedReceiver<AgentEvent>,
        tx: tmpsc::UnboundedSender<AgentEvent>,
        hydrated: bool,
        interrupts: Arc<parking_lot::Mutex<u32>>,
    }

    #[async_trait]
    impl AgentAdapter for FakeAdapter {
        fn id(&self) -> AgentId {
            AgentId::new("fake")
        }
        fn display_name(&self) -> &str {
            "Fake"
        }
        fn caps(&self) -> Caps {
            Caps::FULL
        }
        async fn probe(&self) -> Result<Availability, AgentError> {
            Ok(Availability::Ready { version: None })
        }
        async fn spawn(&self, _ctx: SessionCtx) -> Result<Box<dyn AgentSession>, AgentError> {
            let (tx, rx) = tmpsc::unbounded_channel();
            for ev in &self.script {
                tx.send(clone_event(ev)).unwrap();
            }
            Ok(Box::new(FakeSession {
                rx,
                tx,
                hydrated: false,
                interrupts: Arc::new(parking_lot::Mutex::new(0)),
            }))
        }
    }

    #[async_trait]
    impl AgentSession for FakeSession {
        async fn send(&mut self, turn: UserTurn) -> Result<(), AgentError> {
            self.tx
                .send(AgentEvent::Text {
                    delta: format!("echo:{}", turn.text),
                })
                .unwrap();
            Ok(())
        }
        fn events(&mut self) -> BoxStream<'_, AgentEvent> {
            let rx = &mut self.rx;
            Box::pin(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)))
        }
        async fn interrupt(&mut self) -> Result<(), AgentError> {
            *self.interrupts.lock() += 1;
            self.tx
                .send(AgentEvent::TurnEnd {
                    stop: StopReason::Interrupted,
                })
                .unwrap();
            Ok(())
        }
        async fn decide(&mut self, _id: RequestId, _d: Decision) -> Result<(), AgentError> {
            Ok(())
        }
        async fn hydrate(&mut self, t: &Transcript) -> Result<HydrateMode, AgentError> {
            self.hydrated = true;
            Ok(if t.messages.is_empty() {
                HydrateMode::Fresh
            } else {
                HydrateMode::Replayed
            })
        }
        fn native_session_id(&self) -> Option<String> {
            Some("fake-1".into())
        }
    }

    fn clone_event(e: &AgentEvent) -> AgentEvent {
        serde_json::from_value(serde_json::to_value(e).unwrap()).unwrap()
    }

    fn ctx() -> SessionCtx {
        SessionCtx {
            session_id: agent_core::SessionId::new(),
            cwd: PathBuf::from("/tmp"),
            system_prompt: Arc::from(""),
            model: None,
            mcp: Arc::new(vec![]),
            skills_dir: None,
            runtime_dir: PathBuf::from("/tmp"),
            resume: None,
            permission_mode: agent_core::PermissionMode::Ask,
            env: Vec::new(),
            allowed_tools: Vec::new(),
            remote: None,
            disallowed_tools: Vec::new(),
            host_only: false,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn events_from_the_backend_reach_the_ui() {
        let rt = runtime();
        let adapter = Arc::new(FakeAdapter {
            script: vec![
                AgentEvent::Text { delta: "hi".into() },
                AgentEvent::TurnEnd {
                    stop: StopReason::EndTurn,
                },
            ],
        });
        let mut conn = spawn(rt.handle(), adapter, ctx(), None);
        let mut events = conn.take_events().unwrap();
        let got = rt.block_on(async {
            let mut out = Vec::new();
            for _ in 0..2 {
                out.push(events.next().await.unwrap());
            }
            out
        });
        assert!(matches!(
            &got[0],
            DriverEvent::Agent(AgentEvent::Text { delta }) if delta == "hi"
        ));
        assert!(matches!(
            &got[1],
            DriverEvent::Agent(AgentEvent::TurnEnd {
                stop: StopReason::EndTurn
            })
        ));
    }

    #[test]
    fn commands_are_delivered_while_events_stream() {
        let rt = runtime();
        let adapter = Arc::new(FakeAdapter { script: vec![] });
        let mut conn = spawn(rt.handle(), adapter, ctx(), None);
        let mut events = conn.take_events().unwrap();
        assert!(conn.send(UserTurn::text("ping")));
        let got = rt.block_on(async { events.next().await.unwrap() });
        assert!(
            matches!(&got, DriverEvent::Agent(AgentEvent::Text { delta }) if delta == "echo:ping"),
            "got {got:?} — a command must be handled even though the event \
             stream holds a mutable borrow of the session"
        );
    }

    #[test]
    fn interrupt_reaches_the_backend() {
        let rt = runtime();
        let adapter = Arc::new(FakeAdapter { script: vec![] });
        let mut conn = spawn(rt.handle(), adapter, ctx(), None);
        let mut events = conn.take_events().unwrap();
        assert!(conn.interrupt());
        let got = rt.block_on(async { events.next().await.unwrap() });
        assert!(matches!(
            got,
            DriverEvent::Agent(AgentEvent::TurnEnd {
                stop: StopReason::Interrupted
            })
        ));
    }

    #[test]
    fn prior_transcript_is_hydrated_before_the_first_turn() {
        let rt = runtime();
        let mut prior = Transcript::default();
        prior.push_user("earlier question".into(), vec![], 0);
        let adapter = Arc::new(FakeAdapter { script: vec![] });
        let mut conn = spawn(rt.handle(), adapter, ctx(), Some(prior));
        let mut events = conn.take_events().unwrap();
        let got = rt.block_on(async { events.next().await.unwrap() });
        assert!(
            matches!(got, DriverEvent::Hydrated(HydrateMode::Replayed)),
            "got {got:?}"
        );
    }

    #[test]
    fn spawn_failure_is_reported_as_ended_with_error() {
        struct Broken;
        #[async_trait]
        impl AgentAdapter for Broken {
            fn id(&self) -> AgentId {
                AgentId::new("broken")
            }
            fn display_name(&self) -> &str {
                "Broken"
            }
            fn caps(&self) -> Caps {
                Caps::DEGRADED
            }
            async fn probe(&self) -> Result<Availability, AgentError> {
                Ok(Availability::Missing {
                    hint: String::new(),
                })
            }
            async fn spawn(&self, _c: SessionCtx) -> Result<Box<dyn AgentSession>, AgentError> {
                Err(AgentError::NotInstalled("broken".into()))
            }
        }

        let rt = runtime();
        let mut conn = spawn(rt.handle(), Arc::new(Broken), ctx(), None);
        let mut events = conn.take_events().unwrap();
        let got = rt.block_on(async { events.next().await.unwrap() });
        match got {
            DriverEvent::Ended { error: Some(e) } => assert!(e.contains("not installed"), "{e}"),
            other => panic!("expected Ended with error, got {other:?}"),
        }
    }

    #[test]
    fn fatal_event_ends_the_driver() {
        let rt = runtime();
        let adapter = Arc::new(FakeAdapter {
            script: vec![AgentEvent::Fatal {
                message: "died".into(),
                retryable: true,
            }],
        });
        let mut conn = spawn(rt.handle(), adapter, ctx(), None);
        let mut events = conn.take_events().unwrap();
        let got = rt.block_on(async {
            let a = events.next().await.unwrap();
            let b = events.next().await.unwrap();
            (a, b)
        });
        assert!(matches!(
            got.0,
            DriverEvent::Agent(AgentEvent::Fatal { .. })
        ));
        assert!(
            matches!(got.1, DriverEvent::Ended { error: None }),
            "driver must close the stream after a fatal, got {:?}",
            got.1
        );
    }
}
