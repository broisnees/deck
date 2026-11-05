//! Local models: which runtimes this Mac has (Ollama, LM Studio, a llama.cpp
//! server, MLX), what Ollama has pulled and loaded, and one answer from any
//! of them for Deck's own AI jobs.
//!
//! Every server here listens on loopback, so this speaks just enough
//! HTTP/1.1 over a plain socket: no TLS or proxies to honour, and a pull's
//! progress can be read line by line and stopped at any point, which the
//! one-shot NSURLSession tasks in `net` cannot do. Everything here blocks;
//! run it off the main thread.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long a server has to say it is there.
const PROBE: Duration = Duration::from_millis(300);
/// Listing, unloading, deleting.
const QUICK: Duration = Duration::from_secs(5);
/// Loading a model reads gigabytes from disk.
const LOAD: Duration = Duration::from_secs(120);
const CHAT: Duration = Duration::from_secs(60);
/// A pull goes quiet while it checks a big layer's digest.
const PULL_IDLE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Ollama,
    LmStudio,
    LlamaCpp,
    Mlx,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Ollama, Kind::LmStudio, Kind::LlamaCpp, Kind::Mlx];

    /// As a route writes it: `ollama:qwen2.5-coder:7b`.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Ollama => "ollama",
            Kind::LmStudio => "lmstudio",
            Kind::LlamaCpp => "llamacpp",
            Kind::Mlx => "mlx",
        }
    }

    fn from_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Ollama => "Ollama",
            Kind::LmStudio => "LM Studio",
            Kind::LlamaCpp => "llama.cpp server",
            Kind::Mlx => "MLX",
        }
    }

    /// Each one's default port. llama-server and mlx_lm.server share 8080.
    fn port(self) -> u16 {
        match self {
            Kind::Ollama => 11434,
            Kind::LmStudio => 1234,
            Kind::LlamaCpp | Kind::Mlx => 8080,
        }
    }

    pub fn url(self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    /// How to get it, for when it is missing.
    pub fn install_hint(self) -> &'static str {
        match self {
            Kind::Ollama => "brew install ollama, or the app from ollama.com",
            Kind::LmStudio => "brew install --cask lm-studio, or the app from lmstudio.ai",
            Kind::LlamaCpp => "brew install llama.cpp",
            Kind::Mlx => "pip install mlx-lm",
        }
    }

    /// How to start it, for when it is installed but stopped.
    pub fn start_hint(self) -> &'static str {
        match self {
            Kind::Ollama => "Start it here, or run ollama serve.",
            Kind::LmStudio => "Start its server in LM Studio, or run lms server start.",
            Kind::LlamaCpp => "Run llama-server -m with a .gguf model.",
            Kind::Mlx => "Run mlx_lm.server --model with a model.",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Kind::Ollama => "ollama",
            Kind::LmStudio => "lms",
            Kind::LlamaCpp => "llama-server",
            Kind::Mlx => "mlx_lm.server",
        }
    }

    fn app(self) -> Option<&'static str> {
        match self {
            Kind::Ollama => Some("Ollama.app"),
            Kind::LmStudio => Some("LM Studio.app"),
            Kind::LlamaCpp | Kind::Mlx => None,
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `bin` on PATH or where Homebrew, pip and LM Studio put theirs (an app
/// opened from the Finder has a short PATH).
fn find_binary(bin: &str) -> Option<PathBuf> {
    if let Some(p) = term::which(bin) {
        return Some(p);
    }
    let home = home();
    ["/opt/homebrew/bin", "/usr/local/bin"]
        .into_iter()
        .map(PathBuf::from)
        .chain([".local/bin", ".lmstudio/bin", ".cache/lm-studio/bin"].map(|d| home.join(d)))
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

fn find_app(kind: Kind) -> Option<PathBuf> {
    let app = kind.app()?;
    [PathBuf::from("/Applications"), home().join("Applications")]
        .into_iter()
        .map(|d| d.join(app))
        .find(|p| p.exists())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Runtime {
    pub kind: Kind,
    pub name: &'static str,
    pub installed: bool,
    pub running: bool,
    pub version: Option<String>,
    pub url: String,
}

impl Runtime {
    /// "Running · v0.5.7", "Installed, not running" or "Not installed".
    pub fn status(&self) -> String {
        match (self.running, &self.version) {
            (true, Some(v)) => format!("Running · v{}", v.trim_start_matches('v')),
            (true, None) => "Running".into(),
            (false, _) if self.installed => "Installed, not running".into(),
            _ => "Not installed".into(),
        }
    }
}

/// Every runtime Deck knows, installed or not, each probed at once.
pub fn runtimes() -> Vec<Runtime> {
    let found = |k: Kind| find_binary(k.binary()).is_some() || find_app(k).is_some();
    let found: Vec<(Kind, bool)> = Kind::ALL.into_iter().map(|k| (k, found(k))).collect();
    // 8080 is every dev server's port too: ask it only when one of the two
    // that use it is here, so a web app's log is not knocked every 3 s.
    let shared = found
        .iter()
        .any(|(k, f)| *f && matches!(k, Kind::LlamaCpp | Kind::Mlx));
    let (ollama, lmstudio, shared) = std::thread::scope(|s| {
        let o = s.spawn(|| json(Kind::Ollama.port(), "GET", "/api/version", None, PROBE).ok());
        let l = s.spawn(|| json(Kind::LmStudio.port(), "GET", "/v1/models", None, PROBE).ok());
        let p = s.spawn(move || if shared { probe_8080() } else { (None, false) });
        (
            o.join().ok().flatten(),
            l.join().ok().flatten(),
            p.join().unwrap_or((None, false)),
        )
    });
    found
        .into_iter()
        .map(|(kind, found)| {
            let (running, version) = match kind {
                Kind::Ollama => (
                    ollama.is_some(),
                    ollama
                        .as_ref()
                        .and_then(|v| v["version"].as_str().map(str::to_string)),
                ),
                Kind::LmStudio => (lmstudio.is_some(), None),
                Kind::LlamaCpp => (shared.0.is_some(), shared.0.clone().flatten()),
                Kind::Mlx => (shared.1, None),
            };
            Runtime {
                kind,
                name: kind.name(),
                installed: running || found,
                running,
                version,
                url: kind.url(),
            }
        })
        .collect()
}

/// Who answers on 8080: llama-server has `/props` (and says its build
/// there); mlx_lm.server only the OpenAI routes.
fn probe_8080() -> (Option<Option<String>>, bool) {
    let port = Kind::LlamaCpp.port();
    if let Ok(v) = json(port, "GET", "/props", None, PROBE) {
        if v.get("default_generation_settings").is_some() {
            return (Some(v["build_info"].as_str().map(str::to_string)), false);
        }
    }
    let mlx = json(port, "GET", "/v1/models", None, PROBE).is_ok_and(|v| v["data"].is_array());
    (None, mlx)
}

/// Start Ollama: its app when there is one (which keeps the server up and
/// itself updated), else `ollama serve` in its own process group, so it
/// outlives Deck. Waits until it answers.
pub fn start(kind: Kind) -> Result<(), String> {
    use std::process::{Command, Stdio};
    if kind != Kind::Ollama {
        return Err(format!("Deck can't start {} yet", kind.name()));
    }
    let spawned = match find_app(kind) {
        Some(app) => Command::new("/usr/bin/open").arg("-a").arg(app).spawn(),
        None => {
            let bin = find_binary(kind.binary()).ok_or("Ollama isn't installed")?;
            crate::os::detach(
                Command::new(bin)
                    .arg("serve")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
            )
            .spawn()
        }
    };
    spawned.map_err(|e| format!("couldn't start Ollama: {e}"))?;
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        if json(kind.port(), "GET", "/api/version", None, PROBE).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err("Ollama didn't start answering".into())
}

// -- Ollama's models ----------------------------------------------------------

/// A model Ollama has pulled.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// With its tag: `qwen2.5-coder:7b`.
    pub name: String,
    /// On disk, in bytes.
    pub size: u64,
    pub family: String,
    /// `7.6B`.
    pub params: String,
    /// `Q4_K_M`.
    pub quant: String,
    pub modified: String,
}

impl Model {
    /// "qwen2 · 7.6B · Q4_K_M · 4.7 GB".
    pub fn detail(&self) -> String {
        [
            self.family.clone(),
            self.params.clone(),
            self.quant.clone(),
            size(self.size),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
    }

    /// Whether it can answer a chat at all: embedding models (the BERT
    /// families, `nomic-embed-text`) cannot.
    pub fn chats(&self) -> bool {
        !self.family.contains("bert") && !self.name.contains("embed")
    }
}

/// A model Ollama has in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub name: String,
    /// What it takes in (GPU-shared) memory, in bytes.
    pub memory: u64,
    /// When Ollama lets it go, in seconds since the epoch.
    pub expires: Option<i64>,
}

impl Loaded {
    /// "Loaded · 5.1 GB · unloads in 4m".
    pub fn badge(&self, now: i64) -> String {
        let when = match self.expires.map(|e| e - now) {
            None => String::new(),
            Some(s) if s <= 0 => " · unloading".into(),
            Some(s) if s < 60 => format!(" · unloads in {s}s"),
            Some(s) if s < 3600 => format!(" · unloads in {}m", (s + 59) / 60),
            Some(s) if s < 86_400 => format!(" · unloads in {}h", (s + 3599) / 3600),
            Some(_) => " · stays loaded".into(),
        };
        format!("Loaded · {}{when}", size(self.memory))
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Details {
    family: String,
    parameter_size: String,
    quantization_level: String,
}

fn parse_tags(body: &[u8]) -> Option<Vec<Model>> {
    #[derive(Deserialize)]
    struct Tag {
        name: String,
        #[serde(default)]
        size: u64,
        #[serde(default)]
        modified_at: String,
        #[serde(default)]
        details: Details,
    }
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Tag>,
    }
    let t: Tags = serde_json::from_slice(body).ok()?;
    Some(
        t.models
            .into_iter()
            .map(|m| Model {
                name: m.name,
                size: m.size,
                family: m.details.family,
                params: m.details.parameter_size,
                quant: m.details.quantization_level,
                modified: m.modified_at,
            })
            .collect(),
    )
}

fn parse_ps(body: &[u8]) -> Option<Vec<Loaded>> {
    #[derive(Deserialize)]
    struct Running {
        name: String,
        #[serde(default)]
        size: u64,
        #[serde(default)]
        size_vram: u64,
        #[serde(default)]
        expires_at: String,
    }
    #[derive(Deserialize)]
    struct Ps {
        #[serde(default)]
        models: Vec<Running>,
    }
    let p: Ps = serde_json::from_slice(body).ok()?;
    Some(
        p.models
            .into_iter()
            .map(|m| Loaded {
                // On a Mac it is all VRAM; on CPU only `size` says.
                memory: if m.size_vram > 0 { m.size_vram } else { m.size },
                expires: unix_secs(&m.expires_at),
                name: m.name,
            })
            .collect(),
    )
}

/// What Ollama has pulled.
pub fn models() -> Result<Vec<Model>, String> {
    let (_, body) = ok(
        call(Kind::Ollama.port(), "GET", "/api/tags", None, QUICK),
        Kind::Ollama,
        None,
    )?;
    parse_tags(&body).ok_or_else(|| garbled(Kind::Ollama))
}

/// What Ollama has in memory now.
pub fn loaded() -> Result<Vec<Loaded>, String> {
    let (_, body) = ok(
        call(Kind::Ollama.port(), "GET", "/api/ps", None, QUICK),
        Kind::Ollama,
        None,
    )?;
    parse_ps(&body).ok_or_else(|| garbled(Kind::Ollama))
}

/// Put `name` in memory for ten minutes.
pub fn load(name: &str) -> Result<(), String> {
    let body = json!({ "model": name, "prompt": "", "keep_alive": "10m", "stream": false });
    ok(
        call(
            Kind::Ollama.port(),
            "POST",
            "/api/generate",
            Some(&body),
            LOAD,
        ),
        Kind::Ollama,
        Some(name),
    )
    .map(drop)
}

/// Take `name` out of memory now.
pub fn unload(name: &str) -> Result<(), String> {
    let body = json!({ "model": name, "keep_alive": 0, "stream": false });
    ok(
        call(
            Kind::Ollama.port(),
            "POST",
            "/api/generate",
            Some(&body),
            QUICK,
        ),
        Kind::Ollama,
        Some(name),
    )
    .map(drop)
}

/// Remove `name` and whatever of its files no other model shares.
pub fn delete(name: &str) -> Result<(), String> {
    let body = json!({ "model": name });
    ok(
        call(
            Kind::Ollama.port(),
            "DELETE",
            "/api/delete",
            Some(&body),
            QUICK,
        ),
        Kind::Ollama,
        Some(name),
    )
    .map(drop)
}

/// How far a pull has got, from Ollama's stream of progress lines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// Ollama's word for the step: "pulling manifest", "pulling 6a0746a1ec1a",
    /// "verifying sha256 digest", "success".
    pub status: String,
    /// Each layer's digest, size and bytes so far.
    layers: Vec<(String, u64, u64)>,
    pub done: bool,
}

impl Progress {
    /// One line of the stream. An `error` line is the pull failing.
    fn take(&mut self, line: &[u8]) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Line {
            #[serde(default)]
            status: String,
            #[serde(default)]
            digest: String,
            #[serde(default)]
            total: u64,
            #[serde(default)]
            completed: u64,
            #[serde(default)]
            error: Option<String>,
        }
        let line = line.trim_ascii();
        if line.is_empty() {
            return Ok(());
        }
        let l: Line = serde_json::from_slice(line)
            .map_err(|_| "Ollama sent a progress line Deck couldn't read".to_string())?;
        if let Some(e) = l.error {
            return Err(e);
        }
        if !l.digest.is_empty() && l.total > 0 {
            match self.layers.iter_mut().find(|(d, ..)| *d == l.digest) {
                Some(layer) => *layer = (l.digest, l.total, l.completed.max(layer.2)),
                None => self.layers.push((l.digest, l.total, l.completed)),
            }
        }
        if l.status == "success" {
            self.done = true;
            // The last lines can leave a layer a few bytes short.
            for layer in &mut self.layers {
                layer.2 = layer.1;
            }
        }
        if !l.status.is_empty() {
            self.status = l.status;
        }
        Ok(())
    }

    pub fn total(&self) -> u64 {
        self.layers.iter().map(|l| l.1).sum()
    }

    pub fn completed(&self) -> u64 {
        self.layers.iter().map(|l| l.2.min(l.1)).sum()
    }

    /// 0 to 1, once Ollama has said how big it is.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total();
        (total > 0).then(|| self.completed() as f32 / total as f32)
    }

    /// "1.2 of 4.7 GB · 26%", or the step in words while nothing downloads.
    pub fn describe(&self) -> String {
        let downloading = self.status.starts_with("pulling ") && self.status != "pulling manifest";
        match self.fraction() {
            Some(f) if downloading && !self.done => format!(
                "{} of {} · {:.0}%",
                size(self.completed()),
                size(self.total()),
                f * 100.
            ),
            _ if self.status.is_empty() => "Starting…".into(),
            _ => {
                let mut s = self.status.clone();
                if let Some(c) = s.get(..1) {
                    s.replace_range(..1, &c.to_uppercase());
                }
                format!("{s}…")
            }
        }
    }
}

/// Splits a byte stream into lines, whatever the pieces it arrives in.
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    fn push(&mut self, bytes: &[u8], mut each: impl FnMut(&[u8])) {
        for &b in bytes {
            if b == b'\n' {
                each(&self.partial);
                self.partial.clear();
            } else {
                self.partial.push(b);
            }
        }
    }
}

/// Pull `name` from ollama.com, telling `progress` as it goes, until it is
/// done, fails, or `cancel` is set. Ollama keeps what it got, so pulling the
/// same name again picks up where this stopped.
pub fn pull(
    name: &str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&Progress),
) -> Result<(), String> {
    let mut p = Progress::default();
    let mut lines = Lines::default();
    let mut failed: Option<String> = None;
    let body = json!({ "model": name, "stream": true });
    let mut take = |line: &[u8], p: &mut Progress| match p.take(line) {
        Ok(()) => progress(&*p),
        Err(e) => {
            failed.get_or_insert(e);
        }
    };
    let status = exchange(
        Kind::Ollama.port(),
        "POST",
        "/api/pull",
        Some(&body),
        Wait::Idle(PULL_IDLE),
        Some(cancel),
        |bytes| lines.push(bytes, |l| take(l, &mut p)),
    );
    // A last line without its newline (an error answer is one bare object).
    let rest = std::mem::take(&mut lines.partial);
    take(&rest, &mut p);
    match status {
        Err(Fail::Stopped) => Err("stopped".into()),
        Err(f) => Err(say(f, Kind::Ollama, Some(name))),
        Ok(_) if failed.is_some() => Err(failed.unwrap_or_default()),
        Ok(s) if !(200..300).contains(&s) => Err(format!("Ollama answered {s}")),
        Ok(_) if !p.done => Err("the pull ended before it finished".into()),
        Ok(_) => Ok(()),
    }
}

// -- other servers ------------------------------------------------------------

/// The models an OpenAI-style server offers (`GET /v1/models`).
pub fn served(kind: Kind) -> Result<Vec<String>, String> {
    let (_, body) = ok(
        call(kind.port(), "GET", "/v1/models", None, QUICK),
        kind,
        None,
    )?;
    parse_openai_models(&body).ok_or_else(|| garbled(kind))
}

fn parse_openai_models(body: &[u8]) -> Option<Vec<String>> {
    let v: Value = serde_json::from_slice(body).ok()?;
    Some(
        v["data"]
            .as_array()?
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect(),
    )
}

/// Everything Settings shows, in one go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Survey {
    pub runtimes: Vec<Runtime>,
    /// Ollama's, when it is running.
    pub models: Vec<Model>,
    pub loaded: Vec<Loaded>,
    /// What the other running servers offer.
    pub served: Vec<(Kind, String)>,
    /// Why Ollama's lists could not be read although it is running.
    pub error: Option<String>,
}

impl Survey {
    pub fn running(&self, kind: Kind) -> bool {
        self.runtimes.iter().any(|r| r.kind == kind && r.running)
    }

    /// Whether `route` names a model that is here now.
    pub fn has(&self, route: &Route) -> bool {
        match route {
            Route::Local(Kind::Ollama, m) => self.models.iter().any(|x| same_model(&x.name, m)),
            Route::Local(k, m) => self.served.iter().any(|(sk, x)| sk == k && x == m),
            _ => true,
        }
    }
}

/// `qwen2.5-coder` is `qwen2.5-coder:latest`.
pub fn same_model(a: &str, b: &str) -> bool {
    let full = |s: &str| {
        if s.contains(':') {
            s.to_string()
        } else {
            format!("{s}:latest")
        }
    };
    full(a) == full(b)
}

pub fn survey() -> Survey {
    let runtimes = runtimes();
    let mut s = Survey {
        runtimes,
        ..Default::default()
    };
    if s.running(Kind::Ollama) {
        let (models, loaded) = std::thread::scope(|sc| {
            let l = sc.spawn(loaded);
            (models(), l.join().unwrap_or_else(|_| Ok(Vec::new())))
        });
        match (models, loaded) {
            (Ok(m), Ok(l)) => (s.models, s.loaded) = (m, l),
            (Err(e), _) | (_, Err(e)) => s.error = Some(e),
        }
    }
    for kind in [Kind::LmStudio, Kind::LlamaCpp, Kind::Mlx] {
        if s.running(kind) {
            for m in served(kind).unwrap_or_default() {
                s.served.push((kind, m));
            }
        }
    }
    s
}

// -- routes and answers ---------------------------------------------------------

/// Where one of Deck's own AI jobs runs: `claude:haiku`, `ollama:<model>`,
/// `lmstudio:<model>` (or `llamacpp:`/`mlx:`), or `off`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `claude -p` with this model alias.
    Claude(String),
    Local(Kind, String),
    Off,
}

impl Route {
    /// A bare alias (`haiku`) is a Claude one, as `refine_model` writes it.
    pub fn parse(s: &str) -> Option<Route> {
        let s = s.trim();
        if s == "off" {
            return Some(Route::Off);
        }
        let ok = |m: &str| !m.is_empty() && !m.contains(char::is_whitespace);
        match s.split_once(':') {
            Some(("claude", m)) if ok(m) => Some(Route::Claude(m.to_string())),
            Some((k, m)) if ok(m) => Kind::from_id(k).map(|k| Route::Local(k, m.to_string())),
            Some(_) => None,
            None if ["haiku", "sonnet", "opus"].contains(&s) => Some(Route::Claude(s.to_string())),
            None => None,
        }
    }

    /// "Claude Haiku", "qwen2.5-coder:7b", "Off".
    pub fn label(&self) -> String {
        match self {
            Route::Claude(m) => {
                let name = crate::refine::MODELS
                    .iter()
                    .find(|(id, _)| id == m)
                    .map_or(m.as_str(), |(_, n)| n);
                format!("Claude {name}")
            }
            Route::Local(Kind::Ollama, m) => m.clone(),
            Route::Local(k, m) => format!("{m} · {}", k.name()),
            Route::Off => "Off".into(),
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Claude(m) => write!(f, "claude:{m}"),
            Route::Local(k, m) => write!(f, "{}:{m}", k.id()),
            Route::Off => f.write_str("off"),
        }
    }
}

/// One answer from a local model: Ollama's own chat API, or the
/// OpenAI-style one LM Studio, llama-server and mlx_lm.server share.
pub fn chat(model: &Route, system: &str, user: &str) -> Result<String, String> {
    let Route::Local(kind, name) = model else {
        return Err("that isn't a local model".into());
    };
    let messages = json!([
        { "role": "system", "content": system },
        { "role": "user", "content": user },
    ]);
    let (path, body, pointer) = match kind {
        // A thinking model would think first: slow, and not wanted here.
        Kind::Ollama => (
            "/api/chat",
            json!({ "model": name, "messages": messages, "stream": false, "think": false, "options": { "temperature": 0.2 } }),
            "/message/content",
        ),
        _ => (
            "/v1/chat/completions",
            json!({ "model": name, "messages": messages, "stream": false, "temperature": 0.2 }),
            "/choices/0/message/content",
        ),
    };
    let (_, reply) = ok(
        call(kind.port(), "POST", path, Some(&body), CHAT),
        *kind,
        Some(name),
    )?;
    let v: Value = serde_json::from_slice(&reply).map_err(|_| garbled(*kind))?;
    let text = strip_think(
        v.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if text.is_empty() {
        return Err(format!("{name} gave an empty answer"));
    }
    Ok(text)
}

/// A reply without the `<think>…</think>` some models open with.
fn strip_think(s: &str) -> String {
    let s = s.trim();
    match (s.strip_prefix("<think>"), s.find("</think>")) {
        (Some(_), Some(end)) => s[end + "</think>".len()..].trim().to_string(),
        _ => s.to_string(),
    }
}

// -- words ----------------------------------------------------------------------

/// Bytes as Ollama shows them: "4.7 GB", "46 MB".
pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if bytes > 0 {
        format!("{:.0} KB", (b / 1e3).max(1.))
    } else {
        String::new()
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Whole seconds since the epoch of an RFC 3339 time, as Ollama writes
/// `expires_at` (`2024-06-04T14:38:31.83753-07:00`).
fn unix_secs(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, sec) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = s.get(19..)?;
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(&sign @ (b'+' | b'-')) => {
            let oh = rest.get(1..3)?.parse::<i64>().ok()?;
            let om = rest.get(4..6)?.parse::<i64>().ok()?;
            (oh * 3600 + om * 60) * if sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    // Days since 1970-01-01 of a Gregorian date.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

// -- HTTP over loopback ---------------------------------------------------------

/// What went wrong talking to a server.
#[derive(Debug, PartialEq)]
enum Fail {
    /// Nothing listens there.
    Down,
    Slow,
    /// `cancel` was set.
    Stopped,
    /// It hung up before the answer was whole.
    Dropped,
    Garbled,
    /// A non-2xx answer and what it said.
    Status(u16, String),
}

/// The words for `f`, about `kind` and maybe a model.
fn say(f: Fail, kind: Kind, model: Option<&str>) -> String {
    let who = kind.name();
    match f {
        Fail::Down => format!("{who} isn't running"),
        Fail::Slow => format!("{who} took too long"),
        Fail::Stopped => "stopped".into(),
        Fail::Dropped => format!("{who} hung up"),
        Fail::Garbled => garbled(kind),
        Fail::Status(404, _) if model.is_some() => {
            format!("{who} doesn't have {}", model.unwrap_or_default())
        }
        Fail::Status(_, e) if !e.is_empty() => format!("{who}: {e}"),
        Fail::Status(s, _) => format!("{who} answered {s}"),
    }
}

fn garbled(kind: Kind) -> String {
    format!("{} sent an answer Deck couldn't read", kind.name())
}

/// A 2xx answer, or why not in words.
fn ok(
    got: Result<(u16, Vec<u8>), Fail>,
    kind: Kind,
    model: Option<&str>,
) -> Result<(u16, Vec<u8>), String> {
    match got {
        Ok((s, body)) if (200..300).contains(&s) => Ok((s, body)),
        Ok((s, body)) => Err(say(Fail::Status(s, error_text(&body)), kind, model)),
        Err(f) => Err(say(f, kind, model)),
    }
}

/// `{"error": "…"}` (Ollama) or `{"error": {"message": "…"}}` (OpenAI's).
fn error_text(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let e = &v["error"];
    let text = e
        .as_str()
        .or_else(|| e["message"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    if text.chars().count() > 120 {
        format!("{}…", text.chars().take(119).collect::<String>())
    } else {
        text
    }
}

/// The JSON of a 2xx answer.
fn json(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    limit: Duration,
) -> Result<Value, Fail> {
    let (status, body) = call(port, method, path, body, limit)?;
    if !(200..300).contains(&status) {
        return Err(Fail::Status(status, error_text(&body)));
    }
    serde_json::from_slice(&body).map_err(|_| Fail::Garbled)
}

/// One request and its whole answer, within `limit`.
fn call(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    limit: Duration,
) -> Result<(u16, Vec<u8>), Fail> {
    let mut out = Vec::new();
    let status = exchange(port, method, path, body, Wait::Total(limit), None, |b| {
        out.extend_from_slice(b)
    })?;
    Ok((status, out))
}

#[derive(Clone, Copy)]
enum Wait {
    /// The whole answer within this.
    Total(Duration),
    /// Never this long without a byte.
    Idle(Duration),
}

/// One request to 127.0.0.1:`port`, handing each piece of the body to
/// `sink` as it arrives. Returns the status once the answer is whole.
fn exchange(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    wait: Wait,
    cancel: Option<&AtomicBool>,
    mut sink: impl FnMut(&[u8]),
) -> Result<u16, Fail> {
    const TICK: Duration = Duration::from_millis(200);
    let limit = match wait {
        Wait::Total(t) | Wait::Idle(t) => t,
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s =
        TcpStream::connect_timeout(&addr, limit.min(Duration::from_secs(2))).map_err(|e| {
            if e.kind() == ErrorKind::TimedOut {
                Fail::Slow
            } else {
                Fail::Down
            }
        })?;
    let payload = body.map(|b| serde_json::to_vec(b).unwrap_or_default());
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    if let Some(p) = &payload {
        req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            p.len()
        ));
    }
    req.push_str("\r\n");
    let mut req = req.into_bytes();
    req.extend(payload.unwrap_or_default());
    let _ = s.set_write_timeout(Some(limit));
    s.write_all(&req).map_err(|_| Fail::Dropped)?;

    let started = Instant::now();
    let mut heard = started;
    let mut answer = Answer::default();
    let mut buf = vec![0u8; 16 * 1024];
    let mut out = Vec::new();
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Fail::Stopped);
        }
        let left = match wait {
            Wait::Total(t) => t.checked_sub(started.elapsed()),
            Wait::Idle(t) => t.checked_sub(heard.elapsed()),
        }
        .filter(|d| !d.is_zero())
        .ok_or(Fail::Slow)?;
        let _ = s.set_read_timeout(Some(left.min(TICK)));
        match s.read(&mut buf) {
            Ok(0) => return answer.end(),
            Ok(n) => {
                heard = Instant::now();
                out.clear();
                answer.feed(&buf[..n], &mut out)?;
                if !out.is_empty() {
                    sink(&out);
                }
                if answer.done {
                    return answer.status.ok_or(Fail::Garbled);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(Fail::Dropped),
        }
    }
}

/// An HTTP/1.1 answer read as it arrives: the head, then the body with any
/// chunked coding undone.
#[derive(Default)]
struct Answer {
    raw: Vec<u8>,
    status: Option<u16>,
    chunked: bool,
    length: Option<usize>,
    /// Body bytes handed on so far.
    seen: usize,
    done: bool,
}

impl Answer {
    /// Take `bytes` off the wire; what they add to the body goes to `out`.
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), Fail> {
        if self.done {
            return Ok(()); // trailers
        }
        self.raw.extend_from_slice(bytes);
        if self.status.is_none() {
            let Some(end) = self.raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                return if self.raw.len() > 64 * 1024 {
                    Err(Fail::Garbled)
                } else {
                    Ok(())
                };
            };
            let head = String::from_utf8_lossy(&self.raw[..end]).to_string();
            self.raw.drain(..end + 4);
            let mut lines = head.lines();
            let status = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or(Fail::Garbled)?;
            for l in lines {
                let Some((k, v)) = l.split_once(':') else {
                    continue;
                };
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
                if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                    self.chunked = true;
                } else if k == "content-length" {
                    self.length = v.parse().ok();
                }
            }
            self.status = Some(status);
            if status == 204 || status == 304 || (!self.chunked && self.length == Some(0)) {
                self.done = true;
                return Ok(());
            }
        }
        if self.chunked {
            // size in hex (maybe with ;extensions), CRLF, data, CRLF.
            while let Some(eol) = self.raw.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.raw[..eol]).to_string();
                let hex = line.split(';').next().unwrap_or("").trim();
                let n = usize::from_str_radix(hex, 16).map_err(|_| Fail::Garbled)?;
                if n == 0 {
                    self.done = true;
                    self.raw.clear();
                    break;
                }
                if self.raw.len() < eol + 2 + n + 2 {
                    break;
                }
                out.extend_from_slice(&self.raw[eol + 2..eol + 2 + n]);
                self.raw.drain(..eol + 2 + n + 2);
            }
        } else {
            let take = match self.length {
                Some(len) => self.raw.len().min(len - self.seen),
                None => self.raw.len(),
            };
            out.extend(self.raw.drain(..take));
            self.seen += take;
            if self.length == Some(self.seen) {
                self.done = true;
            }
        }
        Ok(())
    }

    /// The server closed the connection: fine when the body was to run
    /// until then, cut short otherwise.
    fn end(&self) -> Result<u16, Fail> {
        match self.status {
            Some(s) if self.done || (!self.chunked && self.length.is_none()) => Ok(s),
            Some(_) => Err(Fail::Dropped),
            None => Err(Fail::Dropped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /api/tags`, as Ollama 0.5 answers it.
    const TAGS: &str = r#"{"models":[{"name":"qwen2.5-coder:7b","model":"qwen2.5-coder:7b","modified_at":"2025-05-10T08:06:48.639712648-07:00","size":4683087332,"digest":"2b0496514337","details":{"parent_model":"","format":"gguf","family":"qwen2","families":["qwen2"],"parameter_size":"7.6B","quantization_level":"Q4_K_M"}},{"name":"all-minilm:latest","model":"all-minilm:latest","modified_at":"2025-05-04T17:37:44.706015396-07:00","size":45960996,"digest":"1b226e2802db","details":{"parent_model":"","format":"gguf","family":"bert","families":["bert"],"parameter_size":"23M","quantization_level":"F16"}}]}"#;

    /// `GET /api/ps`.
    const PS: &str = r#"{"models":[{"name":"qwen2.5-coder:7b","model":"qwen2.5-coder:7b","size":5137025024,"digest":"2b0496514337","details":{"family":"qwen2","parameter_size":"7.6B","quantization_level":"Q4_K_M"},"expires_at":"2024-06-04T14:38:31.83753-07:00","size_vram":5137025024}]}"#;

    #[test]
    fn ollama_lists_parse() {
        let models = parse_tags(TAGS.as_bytes()).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "qwen2.5-coder:7b");
        assert_eq!(models[0].detail(), "qwen2 · 7.6B · Q4_K_M · 4.7 GB");
        assert!(models[0].chats());
        assert_eq!(models[1].detail(), "bert · 23M · F16 · 46 MB");
        assert!(!models[1].chats(), "an embedding model can't chat");
        assert_eq!(parse_tags(br#"{"models":[]}"#), Some(vec![]));
        assert_eq!(parse_tags(b"<html>"), None);

        let loaded = parse_ps(PS.as_bytes()).unwrap();
        assert_eq!(loaded[0].memory, 5137025024);
        let expires = loaded[0].expires.unwrap();
        assert_eq!(expires, 1717537111, "14:38:31 at -07:00 is 21:38:31 UTC");
        assert_eq!(
            loaded[0].badge(expires - 230),
            "Loaded · 5.1 GB · unloads in 4m"
        );
        assert_eq!(
            loaded[0].badge(expires - 20),
            "Loaded · 5.1 GB · unloads in 20s"
        );
        assert_eq!(loaded[0].badge(expires + 1), "Loaded · 5.1 GB · unloading");
        // keep_alive -1 is "forever", which Ollama writes centuries out.
        let forever = Loaded {
            expires: unix_secs("2318-01-01T00:00:00Z"),
            ..loaded[0].clone()
        };
        assert_eq!(forever.badge(expires), "Loaded · 5.1 GB · stays loaded");
    }

    #[test]
    fn times_and_sizes_read_as_ollama_writes_them() {
        assert_eq!(unix_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_secs("2000-03-01T00:00:00+01:00"), Some(951_865_200));
        assert_eq!(unix_secs("2026-10-16T07:00:00.5Z"), Some(1_792_134_000));
        assert_eq!(unix_secs("0001-01-01T00:00:00Z").map(|s| s < 0), Some(true));
        assert_eq!(unix_secs("soon"), None);
        assert_eq!(size(4_683_087_332), "4.7 GB");
        assert_eq!(size(45_960_996), "46 MB");
        assert_eq!(size(0), "");
    }

    #[test]
    fn openai_style_model_lists_parse() {
        let body = br#"{"object":"list","data":[{"id":"qwen2.5-7b-instruct","object":"model"},{"id":"text-embedding-nomic","object":"model"}]}"#;
        assert_eq!(
            parse_openai_models(body),
            Some(vec![
                "qwen2.5-7b-instruct".to_string(),
                "text-embedding-nomic".to_string()
            ])
        );
        assert_eq!(parse_openai_models(b"{}"), None);
    }

    #[test]
    fn pull_progress_adds_up_its_layers_however_the_stream_is_cut() {
        let stream = concat!(
            "{\"status\":\"pulling manifest\"}\n",
            "{\"status\":\"pulling 6a0746a1ec1a\",\"digest\":\"sha256:6a07\",\"total\":4000}\n",
            "{\"status\":\"pulling 6a0746a1ec1a\",\"digest\":\"sha256:6a07\",\"total\":4000,\"completed\":1000}\n",
            "{\"status\":\"pulling 4fa551d4f938\",\"digest\":\"sha256:4fa5\",\"total\":1000,\"completed\":1000}\n",
        );
        // Every split point gives the same result.
        for cut in 0..stream.len() {
            let mut p = Progress::default();
            let mut lines = Lines::default();
            for piece in [&stream.as_bytes()[..cut], &stream.as_bytes()[cut..]] {
                lines.push(piece, |l| p.take(l).unwrap());
            }
            assert_eq!((p.completed(), p.total()), (2000, 5000), "cut at {cut}");
        }
        let mut p = Progress::default();
        let mut lines = Lines::default();
        lines.push(stream.as_bytes(), |l| p.take(l).unwrap());
        assert_eq!(p.fraction(), Some(0.4));
        assert_eq!(p.status, "pulling 4fa551d4f938");
        assert!(p.describe().ends_with("· 40%"), "{}", p.describe());
        p.take(br#"{"status":"verifying sha256 digest"}"#).unwrap();
        assert_eq!(p.describe(), "Verifying sha256 digest…");
        assert!(!p.done);
        p.take(br#"{"status":"success"}"#).unwrap();
        assert!(p.done);
        assert_eq!(p.fraction(), Some(1.));
        assert_eq!(
            Progress::default().take(br#"{"error":"pull model manifest: file does not exist"}"#),
            Err("pull model manifest: file does not exist".to_string())
        );
    }

    #[test]
    fn answers_come_off_the_wire_whole() {
        let chunked = b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"a\":1\r\n2;x=y\r\n}\n\r\n0\r\n\r\n";
        for cut in 0..chunked.len() {
            let mut a = Answer::default();
            let mut body = Vec::new();
            let mut out = Vec::new();
            a.feed(&chunked[..cut], &mut out).unwrap();
            body.extend_from_slice(&out);
            out.clear();
            a.feed(&chunked[cut..], &mut out).unwrap();
            body.extend_from_slice(&out);
            assert_eq!(body, b"{\"a\":1}\n", "cut at {cut}");
            assert!(a.done);
            assert_eq!(a.status, Some(200));
        }
        let mut a = Answer::default();
        let mut out = Vec::new();
        a.feed(
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 5\r\n\r\nhel",
            &mut out,
        )
        .unwrap();
        assert!(!a.done);
        assert_eq!(a.end(), Err(Fail::Dropped), "cut short");
        a.feed(b"lo", &mut out).unwrap();
        assert!(a.done);
        assert_eq!(out, b"hello");
        // No length: the body runs until the server hangs up.
        let mut a = Answer::default();
        a.feed(b"HTTP/1.0 200 OK\r\n\r\nall", &mut out).unwrap();
        assert_eq!(a.end(), Ok(200));
    }

    #[test]
    fn errors_say_who_and_what() {
        assert_eq!(
            say(Fail::Down, Kind::Ollama, Some("x")),
            "Ollama isn't running"
        );
        assert_eq!(
            say(
                Fail::Status(404, "model 'x' not found".into()),
                Kind::Ollama,
                Some("x")
            ),
            "Ollama doesn't have x"
        );
        assert_eq!(
            error_text(br#"{"error":{"message":"No models loaded"}}"#),
            "No models loaded"
        );
        assert_eq!(error_text(br#"{"error":"boom"}"#), "boom");
        assert_eq!(strip_think("<think>hmm</think>\n\nls -la"), "ls -la");
        assert_eq!(strip_think("  ls  "), "ls");
    }

    #[test]
    fn routes_parse_and_print_back() {
        for s in [
            "claude:haiku",
            "claude:sonnet",
            "ollama:qwen2.5-coder:7b",
            "lmstudio:qwen2.5-7b-instruct",
            "llamacpp:model.gguf",
            "mlx:mlx-community/Qwen3-4B",
            "off",
        ] {
            let r = Route::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!(r.to_string(), s);
        }
        assert_eq!(Route::parse("haiku"), Some(Route::Claude("haiku".into())));
        assert_eq!(
            Route::parse(" ollama:llama3.2:3b "),
            Some(Route::Local(Kind::Ollama, "llama3.2:3b".into()))
        );
        for bad in [
            "",
            "claude:",
            "ollama:",
            "gpt:4",
            "nonsense",
            "ollama:two words",
        ] {
            assert_eq!(Route::parse(bad), None, "{bad}");
        }
        assert_eq!(Route::Claude("haiku".into()).label(), "Claude Haiku");
        assert_eq!(
            Route::Local(Kind::Ollama, "gemma3:4b".into()).label(),
            "gemma3:4b"
        );
        assert_eq!(
            Route::Local(Kind::LmStudio, "qwen".into()).label(),
            "qwen · LM Studio"
        );
        assert!(same_model("qwen2.5-coder", "qwen2.5-coder:latest"));
        assert!(!same_model("qwen2.5-coder:7b", "qwen2.5-coder:latest"));
    }

    /// Talks to the Ollama on this Mac; read-only. `cargo test -p app
    /// local::tests::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_survey() {
        let s = survey();
        for r in &s.runtimes {
            println!("{}: {} ({})", r.name, r.status(), r.url);
        }
        println!(
            "models {:?}\nloaded {:?}\nserved {:?}\nerror {:?}",
            s.models, s.loaded, s.served, s.error
        );
    }
}
