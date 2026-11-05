//! Chat threads that outlive a launch: one JSON file per thread beside the
//! control socket, holding the transcript, the unsent draft and its
//! attachments, the ↑ history and the agent's own session id (so Claude can
//! resume losslessly). Closing a thread moves its file to `archive/` rather
//! than deleting it.

use agent_core::Transcript;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub id: String,
    /// The pinned control chat, reopened as the first tab.
    pub control: bool,
    pub agent: String,
    pub cwd: PathBuf,
    /// The SSH host its agent runs on, when not this Mac.
    pub remote: Option<String>,
    pub full_access: bool,
    /// The model this thread asked for; `None` is the agent's default.
    pub model: Option<String>,
    /// Ask first. Older files have only `full_access`, so this stays apart
    /// from it rather than replacing it.
    pub ask_first: bool,
    pub title: String,
    pub transcript: Transcript,
    pub draft: String,
    pub attachments: Vec<PathBuf>,
    pub history: Vec<String>,
    /// The agent's session id, for `--resume`.
    pub native_id: Option<String>,
    /// How long each finished turn took: (assistant message, millis).
    pub worked: Vec<(usize, u64)>,
    /// The project's files when each message was sent: (message, commit),
    /// to go back to.
    pub checkpoints: Vec<(usize, String)>,
    /// Unix millis of the last change; restored threads open oldest first.
    pub updated: i64,
}

pub fn dir() -> PathBuf {
    crate::control::socket_dir().join("threads")
}

/// Where pasted images are kept until the thread is done with them.
pub fn attachments_dir() -> PathBuf {
    crate::control::socket_dir().join("attachments")
}

/// Pasted images are kept this long even when no thread names them: one
/// pasted into a terminal goes to the agent there, which deck cannot see.
const KEEP_UNUSED: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Delete pasted images no saved thread (open or archived) refers to, once
/// they are a week old. Runs at startup, off the main thread.
pub fn prune_attachments() {
    let Ok(entries) = std::fs::read_dir(attachments_dir()) else {
        return;
    };
    let mut used = std::collections::HashSet::new();
    for d in [dir(), dir().join("archive")] {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                used.extend(pasted_names(&text));
            }
        }
    }
    let now = std::time::SystemTime::now();
    let mut freed = 0u64;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("pasted-") || used.contains(&name) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > KEEP_UNUSED);
        if old && std::fs::remove_file(e.path()).is_ok() {
            freed += meta.len();
        }
    }
    if freed > 0 {
        tracing::info!("pruned {freed} bytes of pasted images no thread uses");
    }
}

/// The `pasted-<millis>.<ext>` file names that appear in `text`.
fn pasted_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("pasted-") {
        let tail = &rest[i..];
        let end = tail[7..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
            .map_or(tail.len(), |j| 7 + j);
        let name = tail[..end].trim_end_matches('.');
        if name.len() > 7 && name.contains('.') {
            out.push(name.to_string());
        }
        rest = &tail[7..];
    }
    out
}

/// Every saved thread, oldest first. A file that no longer parses is left
/// where it is and skipped, never deleted.
pub fn load_all() -> Vec<Saved> {
    let mut out: Vec<Saved> = std::fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(
            |p| match std::fs::read(&p).map(|b| serde_json::from_slice::<Saved>(&b)) {
                Ok(Ok(s)) if !s.id.is_empty() => Some(s),
                Ok(Err(e)) => {
                    tracing::warn!(path = %p.display(), "unreadable thread: {e}");
                    None
                }
                _ => None,
            },
        )
        .collect();
    out.sort_by_key(|s| s.updated);
    out
}

/// Written whole to a temporary file, then renamed over the old one, so a
/// crash mid-write never leaves half a thread.
pub fn write(saved: &Saved) -> std::io::Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", saved.id));
    let tmp = dir.join(format!(".{}.tmp", saved.id));
    std::fs::write(&tmp, serde_json::to_vec(saved)?)?;
    std::fs::rename(&tmp, &path)
}

/// Closed threads, from `archive/`.
pub fn load_archived() -> Vec<Saved> {
    std::fs::read_dir(dir().join("archive"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice::<Saved>(&std::fs::read(&p).ok()?).ok())
        .filter(|s| !s.id.is_empty())
        .collect()
}

/// Back out of the archive, for a thread reopened.
pub fn unarchive(id: &str) {
    let from = dir().join("archive").join(format!("{id}.json"));
    if from.exists() {
        let _ = std::fs::rename(&from, dir().join(format!("{id}.json")));
    }
}

pub fn archive(id: &str) {
    let from = dir().join(format!("{id}.json"));
    let to_dir = dir().join("archive");
    if from.exists() && std::fs::create_dir_all(&to_dir).is_ok() {
        if let Err(e) = std::fs::rename(&from, to_dir.join(format!("{id}.json"))) {
            tracing::warn!("could not archive thread {id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_names_are_found_in_thread_text() {
        let text = r#"{"path":"/x/attachments/pasted-1712.png","b":"see pasted-99.jpeg."} pasted- pasted-x"#;
        assert_eq!(pasted_names(text), ["pasted-1712.png", "pasted-99.jpeg"]);
    }

    #[test]
    fn an_older_file_with_missing_fields_still_loads() {
        let s: Saved = serde_json::from_str(r#"{"id":"a","agent":"claude","title":"hi"}"#).unwrap();
        assert_eq!(s.id, "a");
        assert!(s.transcript.messages.is_empty());
        assert!(s.draft.is_empty());
    }

    #[test]
    fn a_saved_thread_round_trips() {
        let mut t = Transcript::default();
        t.push_user("hello".into(), vec![], 1);
        let s = Saved {
            id: "x".into(),
            agent: "claude".into(),
            transcript: t,
            draft: "half".into(),
            ..Default::default()
        };
        let back: Saved = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(back.transcript.messages.len(), 1);
        assert_eq!(back.draft, "half");
    }
}
