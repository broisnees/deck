//! Tasks: a piece of work handed to an agent on a branch of its own, in a git
//! worktree of its own, so several run at once without touching each other or
//! the checkout you work in. When one is done you review its changes and
//! merge them, throw them away, or open a pull request.
//!
//! Everything here is git, run as a child process; nothing touches the
//! repository you are in except a merge you asked for.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    #[default]
    Open,
    Merged,
    Discarded,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// The brief's first line, short enough for a sidebar row.
    pub title: String,
    pub brief: String,
    /// The main checkout's root.
    pub repo: PathBuf,
    /// The branch it started from, which a merge goes back into.
    pub base: String,
    /// The commit it started from: what its changes are measured against.
    pub base_commit: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// The chat thread doing the work.
    #[serde(default)]
    pub thread: Option<String>,
    /// The chat thread that handed this task out, if an agent did: it is
    /// told when the task's agent finishes.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub state: State,
}

/// What a task has changed so far, against where it started.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
}

fn path() -> PathBuf {
    crate::control::socket_dir().join("tasks.json")
}

pub fn load_all() -> Vec<Task> {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_all(tasks: &[Task]) {
    let path = path();
    let tmp = path.with_extension("json.tmp");
    let written = serde_json::to_vec_pretty(tasks)
        .map_err(std::io::Error::other)
        .and_then(|j| std::fs::write(&tmp, j))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        tracing::warn!("could not save tasks: {e}");
    }
}

/// Run git in `dir`; its stdout, or its stderr as the error.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    git_env(dir, args, &[])
}

fn git_env(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    crate::os::no_window(&mut cmd);
    cmd.arg("-C").arg(dir).args(args);
    // Polling never takes the index lock from under a commit you are making.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("git {} failed", args.first().unwrap_or(&""))
        } else {
            err
        })
    }
}

/// The repository `dir` is in, at its root; `None` outside one.
pub fn repo_of(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

/// `"Add dark mode to settings!"` → `add-dark-mode-to-settings`.
pub fn slug(brief: &str) -> String {
    let mut out = String::new();
    for word in brief
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if out.len() + word.len() > 40 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word.to_ascii_lowercase());
    }
    if out.is_empty() {
        out.push_str("task");
    }
    out
}

fn title_of(brief: &str) -> String {
    let line = brief
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Task");
    let line = line.trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_string()
    }
}

/// A new task for `brief` in the repository `dir` is in: a branch from what
/// is checked out there, and a worktree for it beside deck's other data.
pub fn create(dir: &Path, brief: &str) -> Result<Task, String> {
    create_in(dir, brief, &crate::control::socket_dir().join("worktrees"))
}

fn create_in(dir: &Path, brief: &str, worktrees: &Path) -> Result<Task, String> {
    let repo = repo_of(dir).ok_or("this folder is not in a git repository")?;
    let base_commit = git(&repo, &["rev-parse", "HEAD"])
        .map_err(|_| "the repository has no commits yet".to_string())?;
    let base = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let id = format!("{}-{:x}", slug(brief), (stamp as u64) & 0xfffff);
    let branch = format!("deck/{id}");
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let worktree = worktrees.join(name).join(&id);
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let wt = worktree.to_string_lossy().into_owned();
    git(
        &repo,
        &["worktree", "add", "-b", &branch, &wt, &base_commit],
    )?;
    Ok(Task {
        id,
        title: title_of(brief),
        brief: brief.to_string(),
        repo,
        base,
        base_commit,
        branch,
        worktree,
        thread: None,
        parent: None,
        created: stamp,
        state: State::Open,
    })
}

/// Everything in the worktree, new files and deletions included, staged into
/// a throwaway index so the worktree's own index is left alone.
fn with_scratch_index<T>(
    task: &Task,
    f: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let index = std::env::temp_dir().join(format!("deck-task-{}.index", task.id));
    let env = [("GIT_INDEX_FILE", index.as_path())];
    let wt = &task.worktree;
    git_env(wt, &["read-tree", "HEAD"], &env)?;
    git_env(wt, &["add", "-A"], &env)?;
    let out = f(&index);
    let _ = std::fs::remove_file(&index);
    out
}

/// Files, lines added and removed since the task started.
pub fn changes(task: &Task) -> Result<Changes, String> {
    with_scratch_index(task, |index| {
        let numstat = git_env(
            &task.worktree,
            &["diff", "--cached", "--numstat", &task.base_commit],
            &[("GIT_INDEX_FILE", index)],
        )?;
        Ok(parse_numstat(&numstat))
    })
}

fn parse_numstat(out: &str) -> Changes {
    let mut c = Changes::default();
    for line in out.lines() {
        let mut parts = line.split('\t');
        let (a, r) = (parts.next(), parts.next());
        // Binary files show `-` for both counts.
        c.added += a.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        c.removed += r.and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        c.files += 1;
    }
    c
}

/// The task's whole diff against where it started, as `git diff` prints it.
pub fn diff(task: &Task) -> Result<String, String> {
    with_scratch_index(task, |index| {
        git_env(
            &task.worktree,
            &["diff", "--cached", "--no-color", "-M", &task.base_commit],
            &[("GIT_INDEX_FILE", index)],
        )
    })
}

/// Commit whatever the agent left uncommitted, so the branch holds it all.
fn commit_all(task: &Task) -> Result<(), String> {
    git(&task.worktree, &["add", "-A"])?;
    let staged = crate::os::no_window(&mut Command::new("git"))
        .arg("-C")
        .arg(&task.worktree)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .map_err(|e| e.to_string())?;
    if !staged.success() {
        git(&task.worktree, &["commit", "-q", "-m", &task.title])?;
    }
    Ok(())
}

/// Squash the task into its base branch, as one commit named after it, then
/// clear the worktree and branch away. The main checkout has to be on the
/// base branch; a merge into the wrong one would be worse than a refusal.
pub fn merge(task: &Task) -> Result<(), String> {
    commit_all(task)?;
    let here = git(&task.repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if here != task.base {
        return Err(format!(
            "{} is on {here}; switch it to {} to merge this task",
            task.repo.display(),
            task.base
        ));
    }
    git(&task.repo, &["merge", "--squash", &task.branch])?;
    let staged = crate::os::no_window(&mut Command::new("git"))
        .arg("-C")
        .arg(&task.repo)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .map_err(|e| e.to_string())?;
    if !staged.success() {
        let message = format!("{}\n\n{}", task.title, task.brief);
        git(&task.repo, &["commit", "-q", "-m", &message])?;
    }
    remove(task)
}

/// Throw the task away: its worktree and its branch.
pub fn discard(task: &Task) -> Result<(), String> {
    remove(task)
}

fn remove(task: &Task) -> Result<(), String> {
    let wt = task.worktree.to_string_lossy().into_owned();
    // Already gone (removed by hand) is as good as removed.
    if task.worktree.exists() {
        git(&task.repo, &["worktree", "remove", "--force", &wt])?;
    }
    let _ = git(&task.repo, &["worktree", "prune"]);
    let _ = git(&task.repo, &["branch", "-D", &task.branch]);
    Ok(())
}

/// Push the branch and open a pull request with `gh`; the PR's address.
pub fn open_pr(task: &Task) -> Result<String, String> {
    commit_all(task)?;
    git(&task.worktree, &["push", "-u", "origin", &task.branch])?;
    let out = crate::os::no_window(&mut Command::new("gh"))
        .current_dir(&task.worktree)
        .args([
            "pr",
            "create",
            "--head",
            &task.branch,
            "--base",
            &task.base,
            "--title",
            &task.title,
            "--body",
            &task.brief,
        ])
        .output()
        .map_err(|e| format!("gh: {e} (is the GitHub CLI installed?)"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .rev()
        .find(|l| l.starts_with("https://"))
        .unwrap_or(text.trim())
        .to_string())
}

/// One file of a diff, its lines ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Hunk(String),
    Add(String),
    Del(String),
    Same(String),
    /// "Binary file changed", a rename, a mode change.
    Note(String),
}

/// `git diff` output, file by file.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            files.push(FileDiff {
                path,
                added: 0,
                removed: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
            continue;
        }
        if line.starts_with("@@") {
            f.lines.push(Line::Hunk(line.to_string()));
        } else if let Some(t) = line.strip_prefix('+') {
            f.added += 1;
            f.lines.push(Line::Add(t.to_string()));
        } else if let Some(t) = line.strip_prefix('-') {
            f.removed += 1;
            f.lines.push(Line::Del(t.to_string()));
        } else if let Some(t) = line.strip_prefix(' ') {
            f.lines.push(Line::Same(t.to_string()));
        } else if line.starts_with("Binary files") {
            f.lines.push(Line::Note("Binary file changed".into()));
        } else if line.starts_with("new file") {
            f.lines.push(Line::Note("New file".into()));
        } else if line.starts_with("deleted file") {
            f.lines.push(Line::Note("Deleted".into()));
        } else if let Some(from) = line.strip_prefix("rename from ") {
            f.lines.push(Line::Note(format!("Renamed from {from}")));
        }
    }
    files
}

// -- preview -------------------------------------------------------------------

/// The command that starts the project's dev server, from what the project
/// has, with `$PORT` for the port it should take. A fresh worktree has no
/// `node_modules`, so it installs first.
pub fn dev_command(dir: &Path) -> Option<String> {
    if let Ok(b) = std::fs::read(dir.join("package.json")) {
        let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
        let scripts = v.get("scripts")?;
        let script = ["dev", "start", "serve"]
            .into_iter()
            .find(|s| scripts.get(s).is_some())?;
        let pm = if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else {
            "npm"
        };
        let install = if dir.join("node_modules").exists() {
            String::new()
        } else {
            format!("{pm} install && ")
        };
        return Some(format!("{install}{pm} run {script}"));
    }
    if dir.join("bin/dev").exists() {
        return Some("bin/dev".into());
    }
    if dir.join("manage.py").exists() {
        return Some("python manage.py runserver 127.0.0.1:$PORT".into());
    }
    if dir.join("Gemfile").exists() && dir.join("config.ru").exists() {
        return Some("bundle exec rails server -p $PORT".into());
    }
    None
}

/// A port nothing listens on right now, for the preview's server to take.
pub fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dev_command_comes_from_the_project() {
        let d = std::env::temp_dir().join(format!("deck-dev-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(dev_command(&d), None);
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"build":"x","start":"y"}}"#,
        )
        .unwrap();
        std::fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            dev_command(&d).as_deref(),
            Some("pnpm install && pnpm run start")
        );
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        assert_eq!(dev_command(&d).as_deref(), Some("pnpm run dev"));
        let _ = std::fs::remove_dir_all(&d);
        assert!(free_port().is_some_and(|p| p > 0));
    }

    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(
            slug("Add dark mode to settings!"),
            "add-dark-mode-to-settings"
        );
        assert_eq!(slug("??"), "task");
        assert!(slug(&"word ".repeat(40)).len() <= 40);
        assert_eq!(title_of("\n  Fix the bug\nmore"), "Fix the bug");
    }

    #[test]
    fn numstat_and_diff_parse() {
        let c = parse_numstat("3\t1\tsrc/a.rs\n-\t-\timg.png\n10\t0\tnew.rs");
        assert_eq!(
            c,
            Changes {
                files: 3,
                added: 13,
                removed: 1
            }
        );
        let d = parse_diff(
            "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\ndiff --git a/x.png b/x.png\nBinary files a/x.png and b/x.png differ\n",
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "src/a.rs");
        assert_eq!((d[0].added, d[0].removed), (1, 1));
        assert_eq!(d[0].lines[1], Line::Same("keep".into()));
        assert_eq!(d[1].lines, vec![Line::Note("Binary file changed".into())]);
    }

    /// The whole life of a task against a real repository in a temp folder:
    /// create, change, count, merge back.
    #[test]
    fn a_task_branches_counts_and_merges() {
        let root = std::env::temp_dir().join(format!("deck-task-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| git(&repo, args).unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);

        let task = create_in(&repo, "Change the greeting", &root.join("worktrees")).unwrap();
        assert_eq!(task.base, "main");
        assert!(task.worktree.join("a.txt").exists());
        std::fs::write(task.worktree.join("a.txt"), "two\n").unwrap();
        std::fs::write(task.worktree.join("b.txt"), "new\nfile\n").unwrap();
        let c = changes(&task).unwrap();
        assert_eq!(
            c,
            Changes {
                files: 2,
                added: 3,
                removed: 1
            }
        );
        assert_eq!(parse_diff(&diff(&task).unwrap()).len(), 2);

        merge(&task).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(repo.join("b.txt").exists());
        assert!(!task.worktree.exists());
        assert!(git(&repo, &["rev-parse", "--verify", &task.branch]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
