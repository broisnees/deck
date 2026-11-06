//! Checkpoints: the project's files as they were when you sent a message,
//! so a turn that went wrong can be undone. Each one is a commit object made
//! through a scratch index and kept under `refs/deck/checkpoints/`, so your
//! branch, index and stash are never touched. Files git ignores are left out
//! of both saving and restoring.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str], env: &[(&str, &Path)]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    crate::os::no_window(&mut cmd);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The top of the repository `dir` is in.
pub fn root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], &[])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
}

/// The ref a thread's checkpoint is kept under.
pub fn refname(thread: &str, name: &str) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("refs/deck/checkpoints/{}/{}", safe(thread), safe(name))
}

/// Save the working tree of the repository `dir` is in as a commit, kept
/// under `refname`. Returns the commit.
pub fn save(dir: &Path, refname: &str) -> Result<String, String> {
    let root = root(dir).ok_or("not in a git repository")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let index =
        std::env::temp_dir().join(format!("deck-checkpoint-{}-{stamp}", std::process::id()));
    // Start from the real index, so files that did not change are known by
    // their stat and only the changed ones are read.
    let real = git(&root, &["rev-parse", "--git-path", "index"], &[])?;
    let real = root.join(real.trim());
    if std::fs::copy(&real, &index).is_err() {
        git(
            &root,
            &["read-tree", "--empty"],
            &[("GIT_INDEX_FILE", &index)],
        )?;
    }
    let env = [("GIT_INDEX_FILE", index.as_path())];
    let result = (|| {
        git(&root, &["add", "-A", "--", "."], &env)?;
        let tree = git(&root, &["write-tree"], &env)?;
        let tree = tree.trim();
        let head = git(&root, &["rev-parse", "-q", "--verify", "HEAD"], &[]).ok();
        let mut args = vec!["commit-tree", tree, "-m", "deck checkpoint"];
        if let Some(h) = head.as_deref().map(str::trim) {
            args.extend(["-p", h]);
        }
        let commit = git(&root, &args, &[])?;
        let commit = commit.trim().to_string();
        git(&root, &["update-ref", refname, &commit], &[])?;
        Ok(commit)
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Put the files back as they were at `target`: every file that differs
/// from now is rewritten, and files made since are removed. Only files git
/// does not ignore are touched; the index and branch are left alone.
/// Returns how many files changed.
pub fn restore(dir: &Path, target: &str, now: &str) -> Result<usize, String> {
    let root = root(dir).ok_or("not in a git repository")?;
    let changed = git(
        &root,
        &["diff", "--name-only", "-z", "--no-renames", target, now],
        &[],
    )?;
    let changed: Vec<&str> = changed.split('\0').filter(|p| !p.is_empty()).collect();
    if changed.is_empty() {
        return Ok(0);
    }
    let mut ls = vec!["ls-tree", "-r", "-z", "--name-only", target, "--"];
    ls.extend(changed.iter().copied());
    let there = git(&root, &ls, &[])?;
    let there: std::collections::HashSet<&str> =
        there.split('\0').filter(|p| !p.is_empty()).collect();
    let (back, gone): (Vec<&str>, Vec<&str>) = changed.iter().partition(|p| there.contains(*p));
    for p in &gone {
        let path = root.join(p);
        let _ = std::fs::remove_file(&path);
        // Folders the turn made and left empty go too.
        let mut d = path.parent();
        while let Some(dir) = d.filter(|d| *d != root) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
            d = dir.parent();
        }
    }
    if !back.is_empty() {
        let mut args = vec!["restore", "--source", target, "--worktree", "--"];
        args.extend(back.iter().copied());
        git(&root, &args, &[])?;
    }
    Ok(changed.len())
}

/// Drop a thread's checkpoint refs (the commits go with git's next gc).
pub fn forget(dir: &Path, thread: &str) {
    let Some(root) = root(dir) else { return };
    let prefix = refname(thread, "x");
    let prefix = prefix.trim_end_matches('x');
    if let Ok(refs) = git(&root, &["for-each-ref", "--format=%(refname)", prefix], &[]) {
        for r in refs.lines() {
            let _ = git(&root, &["update-ref", "-d", r], &[]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("deck-ckpt-test-{stamp}"));
        std::fs::create_dir_all(&d).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "core.autocrlf", "false"],
        ] {
            git(&d, args, &[]).unwrap();
        }
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join(".gitignore"), "ignored/\n").unwrap();
        git(&d, &["add", "."], &[]).unwrap();
        git(&d, &["commit", "-qm", "init"], &[]).unwrap();
        d
    }

    #[test]
    fn a_turn_is_undone_without_touching_index_or_ignored_files() {
        let d = repo();
        std::fs::write(d.join("a.txt"), "edited by me\n").unwrap();
        std::fs::write(d.join("mine.txt"), "untracked, mine\n").unwrap();
        git(&d, &["add", "a.txt"], &[]).unwrap(); // staged by hand
        let before = save(&d, &refname("t1", "0")).unwrap();

        // The agent's turn: edits, a new file in a new folder, a deletion,
        // and something ignored.
        std::fs::write(d.join("a.txt"), "agent\n").unwrap();
        std::fs::create_dir_all(d.join("new/deep")).unwrap();
        std::fs::write(d.join("new/deep/b.txt"), "agent made\n").unwrap();
        std::fs::remove_file(d.join("mine.txt")).unwrap();
        std::fs::create_dir_all(d.join("ignored")).unwrap();
        std::fs::write(d.join("ignored/cache"), "x").unwrap();

        let now = save(&d, &refname("t1", "undo")).unwrap();
        assert_eq!(restore(&d, &before, &now).unwrap(), 3);
        let read = |p: &str| std::fs::read_to_string(d.join(p)).ok();
        assert_eq!(read("a.txt").as_deref(), Some("edited by me\n"));
        assert_eq!(read("mine.txt").as_deref(), Some("untracked, mine\n"));
        assert!(!d.join("new").exists());
        assert_eq!(read("ignored/cache").as_deref(), Some("x"));
        // The hand-staged change is still staged; HEAD did not move.
        let staged = git(&d, &["diff", "--cached", "--name-only"], &[]).unwrap();
        assert_eq!(staged.trim(), "a.txt");
        assert_eq!(
            git(&d, &["log", "--oneline"], &[]).unwrap().lines().count(),
            1
        );
        // And the restore can itself be undone.
        let after = save(&d, &refname("t1", "redo")).unwrap();
        assert_eq!(restore(&d, &now, &after).unwrap(), 3);
        assert_eq!(read("new/deep/b.txt").as_deref(), Some("agent made\n"));

        forget(&d, "t1");
        let refs = git(&d, &["for-each-ref", "refs/deck"], &[]).unwrap();
        assert!(refs.trim().is_empty(), "{refs}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
