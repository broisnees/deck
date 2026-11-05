//! Session orchestration: what agents exist, how a session is configured, and
//! how its I/O crosses from tokio to the UI thread.

pub mod driver;
pub mod registry;

pub use driver::{spawn, Cmd, Conn, DriverEvent};
pub use registry::{AgentEntry, Registry};

use agent_core::{McpServerSpec, PermissionMode, SessionCtx, SessionId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Owns the tokio runtime that every adapter runs on.
///
/// gpui drives the UI on its own executor, so the async backend world needs a
/// runtime of its own; one per process, created at startup.
pub struct Backend {
    rt: tokio::runtime::Runtime,
    root: PathBuf,
}

impl Backend {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .thread_name("yourcode-agent")
            .enable_all()
            .build()?;
        Ok(Self {
            rt,
            root: root.into(),
        })
    }

    pub fn handle(&self) -> &tokio::runtime::Handle {
        self.rt.handle()
    }

    /// Per-session scratch dir for generated agent config. Kept under one root
    /// so a crash leaves a single directory to clean up.
    pub fn runtime_dir(&self, id: SessionId) -> PathBuf {
        self.root.join("sessions").join(id.to_string())
    }
}

/// Everything shared across every agent: one prompt, one MCP registry, one
/// skills directory. Adapters project this into their own native config.
#[derive(Debug, Clone, Default)]
pub struct Substrate {
    pub system_prompt: Arc<str>,
    pub mcp: Arc<Vec<McpServerSpec>>,
    pub skills_dir: Option<PathBuf>,
}

impl Substrate {
    /// Build the per-session context handed to an adapter.
    pub fn session_ctx(
        &self,
        backend: &Backend,
        session_id: SessionId,
        cwd: &Path,
        model: Option<smol_str::SmolStr>,
        permission_mode: PermissionMode,
        resume: Option<String>,
    ) -> SessionCtx {
        SessionCtx {
            session_id,
            cwd: cwd.to_path_buf(),
            system_prompt: self.system_prompt.clone(),
            model,
            mcp: self.mcp.clone(),
            skills_dir: self.skills_dir.clone(),
            runtime_dir: backend.runtime_dir(session_id),
            resume,
            permission_mode,
            env: Vec::new(),
            allowed_tools: Vec::new(),
            remote: None,
            disallowed_tools: Vec::new(),
            host_only: false,
        }
    }
}
