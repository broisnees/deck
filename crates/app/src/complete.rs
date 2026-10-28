//! Completions for the line at a zsh prompt, as a code editor offers them:
//! commands, their subcommands and flags, files, git branches, package
//! scripts and make targets. The shell reports its line (OSC 6973;line);
//! this works out what the word under the cursor could become. It runs off
//! the main thread: it reads folders, package.json and asks git.

mod learn;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Command,
    Sub,
    Flag,
    File,
    Dir,
    Branch,
    Script,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// What the menu shows.
    pub label: String,
    /// What replaces the word, quoted for the shell.
    pub insert: String,
    pub kind: Kind,
    pub detail: Option<String>,
    /// Characters of `label` that matched what was typed, for highlighting.
    pub matched: Vec<usize>,
    /// A space goes after it (not after a folder: its contents come next).
    pub space: bool,
    /// Added to its match score: what you ran lately and the commands
    /// people use rank above the hundreds of tools in /usr/bin.
    boost: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The word being completed, as typed (quotes and escapes included).
    pub word: String,
    pub items: Vec<Item>,
    /// Worth showing before anything is typed: "git " lists subcommands,
    /// "npm run " the scripts. Files wait for a first letter.
    pub eager: bool,
}

/// At most this many come back; the menu shows a window of them.
const MAX_ITEMS: usize = 40;

pub fn complete(line: &str, cursor: usize, cwd: &Path) -> Option<Found> {
    let before: String = line.chars().take(cursor).collect();
    let (args, word) = split(&before);
    let raw_word = word.raw.clone();
    let typed = word.text.clone();
    let (cands, eager) = candidates(&args, &typed, cwd);
    // A whole command or subcommand already typed: nothing to add, so no
    // menu. (A folder still offers what is inside it.)
    let command_word = args.is_empty() || (args.len() == 1 && cands.iter().any(|c| c.kind == Kind::Sub));
    if command_word && !typed.is_empty() && cands.iter().any(|c| c.insert == raw_word && matches!(c.kind, Kind::Command | Kind::Sub)) {
        return None;
    }
    let mut scored: Vec<(i32, usize, Item)> = cands
        .into_iter()
        .enumerate()
        .filter_map(|(order, mut it)| {
            let (score, matched) = score(&it.label, &typed)?;
            it.matched = matched;
            Some((score + it.boost, order, it))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut seen = std::collections::HashSet::new();
    let items: Vec<Item> = scored
        .into_iter()
        .map(|(.., it)| it)
        .filter(|it| seen.insert(it.insert.clone()))
        .take(MAX_ITEMS)
        .collect();
    // Nothing to offer, or only what is already there.
    if items.is_empty() || (items.len() == 1 && items[0].insert == raw_word) {
        return None;
    }
    Some(Found {
        word: raw_word,
        items,
        eager,
    })
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Word {
    /// As typed.
    raw: String,
    /// With quotes and escapes taken out.
    text: String,
}

/// The words of the command the cursor is in, up to it, and the one being
/// typed. `|`, `;`, `&&` and `(` start a new command.
fn split(before: &str) -> (Vec<String>, Word) {
    let mut args: Vec<String> = Vec::new();
    let mut cur = Word::default();
    let mut quote: Option<char> = None;
    let mut chars = before.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.raw.push(c);
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(n) = chars.next() {
                    cur.raw.push(n);
                    cur.text.push(n);
                }
            } else {
                cur.text.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                cur.raw.push(c);
                if let Some(n) = chars.next() {
                    cur.raw.push(n);
                    cur.text.push(n);
                }
            }
            '\'' | '"' => {
                quote = Some(c);
                cur.raw.push(c);
            }
            ' ' | '\t' | '\n' => {
                if !cur.raw.is_empty() {
                    args.push(std::mem::take(&mut cur).text);
                }
            }
            '|' | ';' | '&' | '(' | ')' => {
                cur = Word::default();
                args.clear();
            }
            _ => {
                cur.raw.push(c);
                cur.text.push(c);
            }
        }
    }
    // Words that only lead into the real command.
    while let Some(first) = args.first() {
        let lead = matches!(
            first.as_str(),
            "sudo" | "time" | "nohup" | "exec" | "command" | "builtin" | "noglob" | "env" | "watch"
        ) || (first.contains('=') && !first.starts_with('-') && !first.starts_with('='));
        if !lead {
            break;
        }
        args.remove(0);
    }
    (args, cur)
}

/// How well `label` matches `typed`: a prefix beats a case-insensitive
/// prefix beats letters in order; shorter wins a tie. `None`: no match.
fn score(label: &str, typed: &str) -> Option<(i32, Vec<usize>)> {
    if typed.is_empty() {
        return Some((0, Vec::new()));
    }
    let n = typed.chars().count();
    let len_penalty = label.chars().count().min(60) as i32;
    if label.starts_with(typed) {
        return Some((3000 - len_penalty, (0..n).collect()));
    }
    let lower = label.to_lowercase();
    let t = typed.to_lowercase();
    if lower.starts_with(&t) {
        return Some((2000 - len_penalty, (0..n).collect()));
    }
    // One letter has to be the start; past that, letters in order.
    if n < 2 {
        return None;
    }
    let lc: Vec<char> = lower.chars().collect();
    let mut matched = Vec::with_capacity(n);
    let mut at = 0;
    let mut gaps = 0;
    for tc in t.chars() {
        let found = (at..lc.len()).find(|&i| lc[i] == tc)?;
        gaps += (found - at) as i32;
        matched.push(found);
        at = found + 1;
    }
    let start_bonus = if matched.first() == Some(&0) { 200 } else { 0 };
    Some((1000 + start_bonus - gaps * 10 - len_penalty, matched))
}

fn item(label: impl Into<String>, kind: Kind, detail: Option<&str>) -> Item {
    let label = label.into();
    Item {
        insert: quote(&label),
        label,
        kind,
        detail: detail.map(str::to_string),
        matched: Vec::new(),
        space: true,
        boost: 0,
    }
}

/// A word as the shell needs it: specials escaped with a backslash.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if " \t'\"\\$`!&|;()<>*?[]{}#~".contains(c) && !(c == '~' && out.is_empty()) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn candidates(args: &[String], typed: &str, cwd: &Path) -> (Vec<Item>, bool) {
    let looks_like_path = typed.contains('/') || typed.starts_with('.') || typed.starts_with('~');
    let Some(cmd) = args.first().map(String::as_str) else {
        if looks_like_path {
            return (paths(typed, cwd, PathFilter::Runnable), false);
        }
        return (commands(typed), false);
    };
    let n = args.len();
    if typed.starts_with('-') {
        let sub = args.get(1).map(String::as_str).unwrap_or("");
        let mut out = flags(cmd, sub);
        if let Some(h) = learn::help(&command_path(args)) {
            out.extend(h.flags.iter().map(|(f, d)| item(f.as_str(), Kind::Flag, Some(d))));
        }
        return (out, false);
    }
    if looks_like_path {
        let filter = if matches!(cmd, "cd" | "pushd" | "rmdir") { PathFilter::Dirs } else { PathFilter::All };
        return (paths(typed, cwd, filter), false);
    }
    let subs = |list: &[(&str, &str)]| -> Vec<Item> {
        list.iter().map(|(s, d)| item(*s, Kind::Sub, Some(d))).collect()
    };
    match cmd {
        "cd" | "pushd" | "rmdir" => (paths(typed, cwd, PathFilter::Dirs), false),
        "git" => match (n, args.get(1).map(String::as_str)) {
            (1, _) => (subs(GIT), true),
            (_, Some("checkout" | "switch" | "merge" | "rebase" | "cherry-pick" | "log" | "show" | "branch" | "reset" | "diff")) => {
                let mut out = git_branches(cwd, matches!(args[1].as_str(), "checkout" | "switch" | "log" | "diff" | "reset" | "show"));
                if matches!(args[1].as_str(), "checkout" | "diff" | "reset") {
                    out.extend(git_changed(cwd));
                }
                (out, true)
            }
            (2, Some("push" | "pull" | "fetch")) => (git_remotes(cwd), true),
            (_, Some("push" | "pull")) => (git_branches(cwd, false), true),
            (_, Some("add" | "restore" | "rm" | "stage")) => {
                let mut out = git_changed(cwd);
                out.extend(paths(typed, cwd, PathFilter::All));
                (out, true)
            }
            (2, Some("stash")) => (subs(GIT_STASH), true),
            (2, Some("worktree")) => (subs(GIT_WORKTREE), true),
            (2, Some("remote")) => (subs(GIT_REMOTE), true),
            _ => (paths(typed, cwd, PathFilter::All), false),
        },
        "npm" => match (n, args.get(1).map(String::as_str)) {
            (1, _) => (subs(NPM), true),
            (2, Some("run" | "run-script")) => (scripts(cwd), true),
            _ => (paths(typed, cwd, PathFilter::All), false),
        },
        "pnpm" | "yarn" | "bun" => match (n, args.get(1).map(String::as_str)) {
            (1, _) => {
                let mut out = scripts(cwd);
                out.extend(subs(PNPM));
                (out, true)
            }
            (2, Some("run")) => (scripts(cwd), true),
            _ => (paths(typed, cwd, PathFilter::All), false),
        },
        "npx" | "bunx" if n == 1 => (node_bins(cwd), true),
        "cargo" if n == 1 => (subs(CARGO), true),
        "docker" => match (n, args.get(1).map(String::as_str)) {
            (1, _) => (subs(DOCKER), true),
            (2, Some("compose")) => (subs(COMPOSE), true),
            _ => (paths(typed, cwd, PathFilter::All), false),
        },
        "brew" if n == 1 => (subs(BREW), true),
        "deck" if n == 1 => (subs(DECK), true),
        "deck" if n == 2 && matches!(args[1].as_str(), "pane" | "task" | "browser" | "tab") => {
            let verbs: &[(&str, &str)] = match args[1].as_str() {
                "pane" => &[("list", "every pane"), ("read", "what a pane shows"), ("wait", "until it prints"), ("send", "type into it"), ("split", "a new pane beside"), ("focus", ""), ("close", "")],
                "task" => &[("new", "a sub-agent on its own branch"), ("list", ""), ("show", ""), ("wait", ""), ("diff", ""), ("send", ""), ("merge", "")],
                "browser" => &[("open", "a page"), ("eval", "run JavaScript")],
                _ => &[("new", "a tab")],
            };
            (subs(verbs), true)
        }
        "make" if n == 1 => (make_targets(cwd), true),
        "just" if n == 1 => (just_recipes(cwd), true),
        // Anything else: what its own --help says it takes next, the ones
        // you use most first, with files after them.
        _ => {
            let path = command_path(args);
            match learn::help(&path).filter(|h| !h.subs.is_empty() && path.len() == n) {
                Some(h) => {
                    let used = sub_counts(&path);
                    let mut out: Vec<Item> = h
                        .subs
                        .iter()
                        .map(|(s, d)| {
                            let boost = used.get(s).map_or(0, |c| 250 + 10 * (*c).min(20));
                            Item { boost, ..item(s.as_str(), Kind::Sub, Some(d)) }
                        })
                        .collect();
                    out.extend(paths(typed, cwd, PathFilter::All));
                    (out, true)
                }
                None => (paths(typed, cwd, PathFilter::All), false),
            }
        }
    }
}

/// The command and the subcommands after it that its help lists, up to
/// the first word that is none: `docker compose up web` → docker compose up.
fn command_path(args: &[String]) -> Vec<&str> {
    let mut path = vec![args[0].as_str()];
    for a in &args[1..] {
        let known = learn::help(&path).is_some_and(|h| h.subs.iter().any(|s| s.0 == *a));
        if !known || path.len() >= 3 {
            break;
        }
        path.push(a.as_str());
    }
    path
}

/// How often each word followed `path` in your history.
fn sub_counts(path: &[&str]) -> HashMap<String, i32> {
    static LINES: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let lines = {
        let mut cache = LINES.lock().unwrap_or_else(|e| e.into_inner());
        match cache.as_ref() {
            Some((at, v)) if at.elapsed() < Duration::from_secs(20) => v.clone(),
            _ => {
                let v: Vec<String> = crate::shell::zsh_history(2000).into_iter().map(|(c, _)| c).collect();
                *cache = Some((Instant::now(), v.clone()));
                v
            }
        }
    };
    let prefix = format!("{} ", path.join(" "));
    let mut out = HashMap::new();
    for line in &lines {
        if let Some(next) = line.strip_prefix(&prefix).and_then(|r| r.split_whitespace().next()) {
            *out.entry(next.to_string()).or_insert(0) += 1;
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum PathFilter {
    All,
    Dirs,
    /// Folders and programs: what can stand where a command goes.
    Runnable,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Entries of the folder the word points into whose names start the way the
/// rest of it does (scoring trims further).
fn paths(typed: &str, cwd: &Path, filter: PathFilter) -> Vec<Item> {
    let (dir_part, _) = match typed.rfind('/') {
        Some(i) => (&typed[..=i], &typed[i + 1..]),
        None => ("", typed),
    };
    let base = if let Some(rest) = dir_part.strip_prefix("~/") {
        home().map(|h| h.join(rest))
    } else if dir_part == "~" {
        home()
    } else if dir_part.starts_with('/') {
        Some(PathBuf::from(dir_part))
    } else {
        Some(cwd.join(dir_part))
    };
    let Some(base) = base else { return Vec::new() };
    let Ok(read) = std::fs::read_dir(&base) else { return Vec::new() };
    let leaf = typed.rsplit('/').next().unwrap_or("");
    let mut out = Vec::new();
    for e in read.flatten().take(3000) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !leaf.starts_with('.') {
            continue;
        }
        let ft = e.file_type().ok();
        let dir = ft.is_some_and(|t| t.is_dir())
            || (ft.is_some_and(|t| t.is_symlink()) && e.path().is_dir());
        if filter == PathFilter::Dirs && !dir {
            continue;
        }
        if filter == PathFilter::Runnable && !dir && !executable(&e.path()) {
            continue;
        }
        let full = format!("{dir_part}{name}");
        out.push(Item {
            insert: format!("{}{}", quote_path(dir_part), quote(&name)) + if dir { "/" } else { "" },
            label: if dir { format!("{full}/") } else { full },
            kind: if dir { Kind::Dir } else { Kind::File },
            detail: None,
            matched: Vec::new(),
            space: !dir,
            boost: 0,
        });
    }
    // Folders first, then by name, before scoring reorders.
    out.sort_by(|a, b| (a.kind != Kind::Dir).cmp(&(b.kind != Kind::Dir)).then(a.label.cmp(&b.label)));
    out
}

/// The folder part of a path word, quoted, but with a leading `~` kept.
fn quote_path(dir: &str) -> String {
    match dir.strip_prefix('~') {
        Some(rest) => format!("~{}", quote(rest)),
        None => quote(dir),
    }
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Programs on PATH and shell builtins, the ones run lately first.
fn commands(typed: &str) -> Vec<Item> {
    static PATH_CMDS: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let names = {
        let mut cache = PATH_CMDS.lock().unwrap_or_else(|e| e.into_inner());
        match cache.as_ref() {
            Some((at, names)) if at.elapsed() < Duration::from_secs(60) => names.clone(),
            _ => {
                let names = scan_path();
                *cache = Some((Instant::now(), names.clone()));
                names
            }
        }
    };
    let recent = recent_commands();
    let boosted = |label: &str, detail: Option<&str>, boost: i32| Item { boost, ..item(label, Kind::Command, detail) };
    let mut out: Vec<Item> = recent.iter().map(|c| boosted(c, Some("recent"), 400)).collect();
    out.extend(BUILTINS.iter().map(|(b, d)| boosted(b, Some(d), 300)));
    out.extend(COMMON.iter().filter(|c| names.binary_search_by(|n| n.as_str().cmp(c)).is_ok()).map(|c| boosted(c, None, 200)));
    // The rest of PATH (lskq, lsbom…) only once a word is specific enough
    // to be looking for one of them.
    if typed.chars().count() >= 3 {
        out.extend(names.iter().map(|n| item(n.as_str(), Kind::Command, None)));
    }
    out
}

/// Commands worth offering from the first letter.
const COMMON: &[&str] = &[
    "awk", "bat", "brew", "bun", "bunx", "cargo", "cat", "chmod", "claude", "clear", "code", "codex", "cp", "curl",
    "deck", "deno", "df", "diff", "docker", "du", "eza", "fd", "find", "fzf", "gemini", "gh", "git", "go", "grep",
    "head", "htop", "jq", "kill", "kubectl", "less", "ln", "ls", "make", "man", "mkdir", "mv", "node", "npm", "npx",
    "nvim", "open", "opencode", "pnpm", "ps", "python3", "rg", "rm", "rsync", "rustup", "scp", "sed", "sort", "ssh",
    "tail", "tar", "tmux", "top", "touch", "tree", "tsc", "uniq", "unzip", "vim", "wc", "wget", "yarn", "zip",
];

fn scan_path() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let path = std::env::var("PATH").unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !seen.insert(name.clone()) {
                continue;
            }
            if executable(&e.path()) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

/// First words of recent history, newest first, each once.
fn recent_commands() -> Vec<String> {
    static RECENT: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let mut cache = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, v)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(20) {
            return v.clone();
        }
    }
    let mut seen = std::collections::HashSet::new();
    let v: Vec<String> = crate::shell::zsh_history(2000)
        .into_iter()
        .filter_map(|(cmd, _)| {
            let (args, word) = split(&format!("{cmd} "));
            let first = args.into_iter().next().or(Some(word.text).filter(|w| !w.is_empty()))?;
            (!first.contains('/') && !first.contains('=')).then_some(first)
        })
        .filter(|c| seen.insert(c.clone()))
        .take(30)
        .collect();
    *cache = Some((Instant::now(), v.clone()));
    v
}

/// Run a quick git command in `cwd`, a few seconds of answers cached per
/// folder so each keystroke does not ask again.
type GitCache = HashMap<(PathBuf, String), (Instant, Option<String>)>;

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    static CACHE: Mutex<Option<GitCache>> = Mutex::new(None);
    let key = (cwd.to_path_buf(), args.join(" "));
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = cache.as_ref().and_then(|c| c.get(&key)) {
            if at.elapsed() < Duration::from_secs(4) {
                return v.clone();
            }
        }
    }
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    if map.len() > 64 {
        map.clear();
    }
    map.insert(key, (Instant::now(), out.clone()));
    out
}

fn git_branches(cwd: &Path, remotes: bool) -> Vec<Item> {
    let mut refs = vec!["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)\t%(subject)", "refs/heads"];
    if remotes {
        refs.push("refs/remotes");
    }
    let Some(out) = git(cwd, &refs) else { return Vec::new() };
    out.lines()
        .filter_map(|l| {
            let (name, subject) = l.split_once('\t').unwrap_or((l, ""));
            (!name.ends_with("/HEAD") && !name.is_empty()).then(|| {
                let subject: String = subject.chars().take(60).collect();
                item(name, Kind::Branch, Some(&subject))
            })
        })
        .collect()
}

fn git_remotes(cwd: &Path) -> Vec<Item> {
    git(cwd, &["remote"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|r| item(r, Kind::Branch, Some("remote")))
        .collect()
}

/// Files git sees changed, relative to `cwd`, with their status.
fn git_changed(cwd: &Path) -> Vec<Item> {
    let Some(out) = git(cwd, &["status", "--porcelain=v1", "--untracked-files=all", "--relative"]) else {
        return Vec::new();
    };
    // --relative does not apply to status; make the paths relative here.
    let top = git(cwd, &["rev-parse", "--show-toplevel"]).map(|t| PathBuf::from(t.trim()));
    out.lines()
        .filter(|l| l.len() > 3)
        .filter_map(|l| {
            let (code, path) = l.split_at(3);
            let path = path.rsplit(" -> ").next().unwrap_or(path).trim_matches('"');
            let full = top.as_ref().map(|t| t.join(path))?;
            let rel = full.strip_prefix(cwd).ok()?.to_string_lossy().into_owned();
            let what = match code.trim() {
                "??" => "untracked",
                c if c.contains('D') => "deleted",
                c if c.contains('A') => "added",
                c if c.contains('R') => "renamed",
                _ => "modified",
            };
            Some(item(rel, Kind::File, Some(what)))
        })
        .take(200)
        .collect()
}

/// The nearest package.json's scripts, in its order.
fn scripts(cwd: &Path) -> Vec<Item> {
    let Some(pkg) = cwd.ancestors().map(|d| d.join("package.json")).find(|p| p.is_file()) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(pkg) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let Some(map) = v.get("scripts").and_then(|s| s.as_object()) else { return Vec::new() };
    map.iter()
        .map(|(name, body)| {
            let body: String = body.as_str().unwrap_or("").chars().take(60).collect();
            item(name.as_str(), Kind::Script, Some(&body))
        })
        .collect()
}

fn node_bins(cwd: &Path) -> Vec<Item> {
    let Some(bin) = cwd.ancestors().map(|d| d.join("node_modules/.bin")).find(|p| p.is_dir()) else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(bin) else { return Vec::new() };
    let mut out: Vec<Item> = read
        .flatten()
        .map(|e| item(e.file_name().to_string_lossy().as_ref(), Kind::Command, Some("node_modules")))
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

fn make_targets(cwd: &Path) -> Vec<Item> {
    let Some(text) = ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .find_map(|f| std::fs::read_to_string(cwd.join(f)).ok())
    else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let ok = !name.is_empty()
                && !rest.starts_with('=')
                && !name.starts_with(['.', '\t', ' ', '#'])
                && name.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c));
            (ok && seen.insert(name.to_string())).then(|| item(name, Kind::Script, Some("make")))
        })
        .collect()
}

fn just_recipes(cwd: &Path) -> Vec<Item> {
    let Some(text) = ["justfile", "Justfile", ".justfile"]
        .iter()
        .find_map(|f| std::fs::read_to_string(cwd.join(f)).ok())
    else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            if l.starts_with([' ', '\t', '#', '@']) || l.contains(":=") {
                return None;
            }
            let (head, _) = l.split_once(':')?;
            let name = head.split_whitespace().next()?;
            name.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                .then(|| item(name, Kind::Script, Some("just")))
        })
        .collect()
}

fn flags(cmd: &str, sub: &str) -> Vec<Item> {
    let mut out = Vec::new();
    for (key, list) in FLAGS {
        let hit = match key.split_once(' ') {
            Some((c, s)) => c == cmd && s == sub,
            None => *key == cmd,
        };
        if hit {
            out.extend(list.iter().map(|(f, d)| item(*f, Kind::Flag, Some(d))));
        }
    }
    out.push(item("--help", Kind::Flag, Some("show help")));
    out
}

const BUILTINS: &[(&str, &str)] = &[
    ("cd", "change folder"),
    ("export", "set a variable"),
    ("source", "run a file here"),
    ("alias", "name a command"),
    ("echo", "print"),
    ("exit", "close the shell"),
    ("history", "past commands"),
    ("which", "where a command is"),
    ("pushd", "cd, remembering"),
    ("popd", "go back"),
    ("unset", "remove a variable"),
    ("type", "what a name is"),
];

const GIT: &[(&str, &str)] = &[
    ("status", "what changed"),
    ("add", "stage changes"),
    ("commit", "record staged changes"),
    ("push", "send to the remote"),
    ("pull", "fetch and merge"),
    ("checkout", "switch branch or restore files"),
    ("switch", "change branch"),
    ("branch", "list, make or delete branches"),
    ("diff", "show changes"),
    ("log", "history"),
    ("merge", "join a branch in"),
    ("rebase", "replay commits"),
    ("stash", "set changes aside"),
    ("fetch", "download from the remote"),
    ("restore", "undo file changes"),
    ("reset", "move HEAD"),
    ("clone", "copy a repository"),
    ("show", "a commit"),
    ("tag", "name a commit"),
    ("worktree", "more checkouts"),
    ("cherry-pick", "apply a commit"),
    ("remote", "manage remotes"),
    ("init", "start a repository"),
    ("blame", "who changed each line"),
    ("revert", "undo a commit"),
    ("rm", "remove files"),
    ("mv", "move files"),
    ("config", "settings"),
    ("bisect", "find a bad commit"),
];
const GIT_STASH: &[(&str, &str)] = &[
    ("push", "stash changes"),
    ("pop", "apply and drop"),
    ("list", ""),
    ("apply", "apply, keep"),
    ("drop", ""),
    ("show", ""),
    ("clear", "drop all"),
];
const GIT_WORKTREE: &[(&str, &str)] = &[("add", "a checkout"), ("list", ""), ("remove", ""), ("prune", "")];
const GIT_REMOTE: &[(&str, &str)] = &[("-v", "list with URLs"), ("add", ""), ("remove", ""), ("rename", ""), ("set-url", "")];
const NPM: &[(&str, &str)] = &[
    ("run", "a package.json script"),
    ("install", "dependencies"),
    ("test", ""),
    ("start", ""),
    ("ci", "clean install"),
    ("init", ""),
    ("uninstall", ""),
    ("update", ""),
    ("exec", ""),
    ("outdated", ""),
    ("ls", ""),
    ("audit", ""),
    ("publish", ""),
    ("version", ""),
    ("link", ""),
];
const PNPM: &[(&str, &str)] = &[
    ("install", "dependencies"),
    ("add", "a dependency"),
    ("remove", ""),
    ("run", "a script"),
    ("dlx", "run a package"),
    ("exec", ""),
    ("update", ""),
    ("test", ""),
    ("build", ""),
    ("dev", ""),
];
const CARGO: &[(&str, &str)] = &[
    ("build", ""),
    ("run", ""),
    ("test", ""),
    ("check", ""),
    ("clippy", "lints"),
    ("fmt", "format"),
    ("add", "a dependency"),
    ("remove", ""),
    ("new", ""),
    ("init", ""),
    ("doc", ""),
    ("bench", ""),
    ("update", ""),
    ("install", ""),
    ("tree", ""),
    ("clean", ""),
    ("fix", ""),
    ("publish", ""),
];
const DOCKER: &[(&str, &str)] = &[
    ("ps", "running containers"),
    ("run", ""),
    ("build", ""),
    ("compose", ""),
    ("exec", ""),
    ("logs", ""),
    ("images", ""),
    ("stop", ""),
    ("start", ""),
    ("rm", ""),
    ("rmi", ""),
    ("pull", ""),
    ("push", ""),
    ("inspect", ""),
    ("volume", ""),
    ("network", ""),
    ("system", ""),
    ("login", ""),
];
const COMPOSE: &[(&str, &str)] = &[
    ("up", ""),
    ("down", ""),
    ("logs", ""),
    ("ps", ""),
    ("build", ""),
    ("exec", ""),
    ("restart", ""),
    ("pull", ""),
    ("stop", ""),
];
const BREW: &[(&str, &str)] = &[
    ("install", ""),
    ("uninstall", ""),
    ("upgrade", ""),
    ("update", ""),
    ("list", ""),
    ("search", ""),
    ("info", ""),
    ("services", ""),
    ("outdated", ""),
    ("cleanup", ""),
    ("doctor", ""),
];
const DECK: &[(&str, &str)] = &[
    ("run", "a command in a pane beside you"),
    ("open", "a page beside you"),
    ("pane", "list, read, wait, send"),
    ("tab", "new tabs"),
    ("browser", "open, eval"),
    ("task", "sub-agents"),
    ("settings", "open Settings"),
    ("window", "shot"),
    ("doctor", "what deck can find"),
];

/// Flags by command, or by "command subcommand".
const FLAGS: &[(&str, &[(&str, &str)])] = &[
    ("git commit", &[("-m", "message"), ("--amend", "change the last commit"), ("--no-edit", "keep its message"), ("-a", "stage tracked changes"), ("--fixup", "for a commit"), ("--allow-empty", "")]),
    ("git push", &[("-u", "set upstream"), ("--force-with-lease", "force, safely"), ("--tags", ""), ("--set-upstream", "")]),
    ("git pull", &[("--rebase", ""), ("--ff-only", "")]),
    ("git log", &[("--oneline", ""), ("--graph", ""), ("--all", ""), ("-n", "how many"), ("-p", "with patches"), ("--stat", "")]),
    ("git checkout", &[("-b", "a new branch"), ("--", "files follow")]),
    ("git switch", &[("-c", "a new branch"), ("-", "the previous branch")]),
    ("git branch", &[("-d", "delete"), ("-D", "delete, unmerged too"), ("-a", "remote too"), ("-r", "remote"), ("-m", "rename"), ("--merged", "")]),
    ("git reset", &[("--hard", "discard changes"), ("--soft", "keep staged"), ("--mixed", "")]),
    ("git diff", &[("--staged", ""), ("--stat", ""), ("--name-only", "")]),
    ("git rebase", &[("-i", "interactive"), ("--continue", ""), ("--abort", ""), ("--onto", "")]),
    ("git merge", &[("--no-ff", ""), ("--squash", ""), ("--abort", "")]),
    ("git stash", &[("-u", "with untracked"), ("-m", "message")]),
    ("git add", &[("-A", "everything"), ("-p", "pick hunks"), ("-u", "tracked only")]),
    ("git clone", &[("--depth", ""), ("--branch", "")]),
    ("ls", &[("-la", "all, long"), ("-l", "long"), ("-a", "hidden too"), ("-h", "human sizes"), ("-t", "by time"), ("-R", "recursive")]),
    ("rm", &[("-rf", "folders, no questions"), ("-r", "folders"), ("-i", "ask")]),
    ("cp", &[("-r", "folders"), ("-i", "ask")]),
    ("mkdir", &[("-p", "parents too")]),
    ("grep", &[("-r", "recursive"), ("-i", "any case"), ("-n", "line numbers"), ("-v", "not matching"), ("-l", "file names"), ("-E", "extended regex")]),
    ("rg", &[("-i", "any case"), ("-l", "file names"), ("-t", "file type"), ("-g", "glob"), ("--hidden", "")]),
    ("cargo build", &[("--release", ""), ("-p", "package"), ("--workspace", "")]),
    ("cargo test", &[("--workspace", ""), ("-p", "package"), ("--release", ""), ("--", "args for the tests")]),
    ("cargo run", &[("--release", ""), ("--bin", ""), ("--example", ""), ("-p", "package")]),
    ("cargo clippy", &[("--all-targets", ""), ("--fix", ""), ("--workspace", "")]),
    ("cargo check", &[("--all-targets", ""), ("--workspace", "")]),
    ("npm install", &[("-D", "dev dependency"), ("-g", "global"), ("--save-dev", "")]),
    ("pnpm add", &[("-D", "dev dependency"), ("-g", "global")]),
    ("docker run", &[("-it", "interactive"), ("-d", "detached"), ("-p", "publish a port"), ("-v", "mount"), ("--rm", "remove on exit"), ("--name", ""), ("-e", "a variable")]),
    ("docker compose", &[("-f", "file"), ("-d", "detached")]),
    ("deck run", &[("--name", "a handle to reuse"), ("--dir", "right or down"), ("--cwd", "")]),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(f: &Option<Found>) -> Vec<&str> {
        f.as_ref().map(|f| f.items.iter().map(|i| i.label.as_str()).collect()).unwrap_or_default()
    }

    #[test]
    fn words_split_like_the_shell() {
        let (args, w) = split("FOO=1 sudo git commit -m 'a b' && npm r");
        assert_eq!(args, vec!["npm"]);
        assert_eq!(w.raw, "r");
        let (args, w) = split("ls my\\ fi");
        assert_eq!(args, vec!["ls"]);
        assert_eq!((w.raw.as_str(), w.text.as_str()), ("my\\ fi", "my fi"));
        let (args, w) = split("git ");
        assert_eq!((args, w.raw), (vec!["git".to_string()], String::new()));
    }

    #[test]
    fn prefixes_beat_letters_in_order() {
        assert!(score("checkout", "ch").unwrap().0 > score("cherry-pick", "chk").unwrap().0);
        assert!(score("status", "st").unwrap().0 > score("stash", "sts").unwrap().0 - 2000);
        assert_eq!(score("commit", "cmt").unwrap().1, vec![0, 2, 5]);
        assert!(score("commit", "x").is_none());
        assert!(score("commit", "o").is_none(), "one letter has to start the word");
    }

    #[test]
    fn git_subcommands_and_flags() {
        let tmp = std::env::temp_dir();
        let f = complete("git ch", 6, &tmp);
        assert_eq!(&labels(&f)[..2], &["checkout", "cherry-pick"]);
        let f = complete("git ", 4, &tmp).unwrap();
        assert!(f.eager && f.items.len() > 10);
        let f = complete("git commit --am", 15, &tmp);
        assert_eq!(labels(&f)[0], "--amend");
        // What is already whole is not offered back.
        assert!(complete("git status", 10, &tmp).is_none());
    }

    #[test]
    fn commands_rank_what_people_run_and_whole_words_close_the_menu() {
        let tmp = std::env::temp_dir();
        assert!(complete("ls", 2, &tmp).is_none(), "ls is already a whole command");
        assert!(complete("git", 3, &tmp).is_none());
        let found = complete("l", 1, &tmp);
        let short = labels(&found);
        assert!(!short.contains(&"lskq") && !short.contains(&"lsbom"), "{short:?}");
        if std::path::Path::new("/usr/bin/lskq").exists() {
            assert!(labels(&complete("lsk", 3, &tmp)).contains(&"lskq"));
        }
    }

    #[test]
    fn files_scripts_and_targets_from_the_folder() {
        let dir = std::env::temp_dir().join(format!("deck-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src dir")).unwrap();
        std::fs::write(dir.join("src.rs"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"vite","build":"vite build"}}"#).unwrap();
        std::fs::write(dir.join("Makefile"), "all: build\nbuild:\n\tcc x.c\nX := 1\n.PHONY: all\n").unwrap();

        let f = complete("cat sr", 6, &dir).unwrap();
        assert_eq!(labels(&Some(f.clone())), vec!["src.rs", "src dir/"]);
        assert_eq!(f.items[1].insert, "src\\ dir/");
        assert!(f.items[0].space && !f.items[1].space);
        assert_eq!(labels(&complete("cd s", 4, &dir)), vec!["src dir/"]);
        assert_eq!(labels(&complete("cat .h", 6, &dir)), vec![".hidden"]);
        assert_eq!(labels(&complete("npm run ", 8, &dir)), vec!["dev", "build"]);
        assert_eq!(labels(&complete("pnpm d", 6, &dir))[0], "dev");
        assert_eq!(labels(&complete("make ", 5, &dir)), vec!["all", "build"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoting_escapes_what_the_shell_would_split_on() {
        assert_eq!(quote("a b(1)"), "a\\ b\\(1\\)");
        assert_eq!(quote("~x"), "~x");
        assert_eq!(quote_path("~/My Files/"), "~/My\\ Files/");
    }
}
