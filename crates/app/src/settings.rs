//! Preferences that outlive a launch: one small JSON file beside the control
//! socket. Unknown or missing fields fall back to defaults, so an older file
//! never stops the app from starting, and one bad value only resets itself.

use crate::theme;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;

/// The zoom steps ⌘+ and ⌘− walk through (1.0 = 100%).
pub const ZOOMS: [f32; 10] = [0.75, 0.85, 0.9, 1.0, 1.1, 1.2, 1.35, 1.5, 1.75, 2.0];
pub const DEFAULT_ZOOM: f32 = 1.0;
pub const FONT_PX_RANGE: (f32, f32) = (9., 32.);
/// Terminal line height as a multiple of the font size.
pub const LINE_HEIGHT_RANGE: (f32, f32) = (1.0, 2.0);
pub const DEFAULT_LINE_HEIGHT: f32 = 1.4;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    /// The window's theme: every surface, the chrome and native views.
    pub theme: theme::Theme,
    /// Terminal font size at 100% zoom.
    pub font_px: f32,
    /// One zoom for everything: UI, terminals, web pages.
    pub zoom: f32,
    /// Terminal font family.
    pub font_family: String,
    pub line_height: f32,
    pub scheme: Scheme,
    pub cursor: Cursor,
    pub cursor_blink: bool,
    /// The emulator new terminals run on. One this build lacks falls back
    /// to Alacritty.
    pub engine: Engine,
    /// ⌥ sends Esc+key (Meta) instead of typing ⌥-characters like ∂ or ƒ.
    pub option_as_meta: bool,
    pub search: Search,
    /// Registry id of the agent new chats start with; `None` picks Claude if
    /// it is ready, else the first agent that is.
    pub default_agent: Option<String>,
    pub access: Access,
    /// A notification when an agent asks for permission or a question.
    pub notify_attention: bool,
    /// A notification when an agent finishes a long piece of work.
    pub notify_done: bool,
    /// The sound for an agent that needs you, one of `notify::SOUNDS`.
    pub sound_attention: String,
    /// The sound for an agent that finished.
    pub sound_done: String,
    /// The sound for a background job that failed.
    pub sound_failed: String,
    /// 0–1.
    pub sound_volume: f32,
    /// Rewrite chat messages into clear prompts before they are sent.
    pub refine: crate::refine::Mode,
    /// The model that rewrites them, by Claude Code's alias.
    pub refine_model: String,
    /// Say "continue" when a turn dies on an API or network error.
    pub auto_retry: bool,
    /// The first launch offered to make deck the default terminal; it asks
    /// once.
    pub asked_default_terminal: bool,
    /// The first launch offered to give Deck and its background service
    /// Full Disk Access; it asks once.
    pub asked_permissions: bool,
    /// Look for a newer deck now and then.
    pub check_updates: bool,
    /// Download a newer deck in the background and put it in place at quit.
    pub install_updates: bool,
    /// Programs' coloured backgrounds in terminals drawn as a tint.
    pub soften_backgrounds: bool,
    /// A menu of completions under the cursor at a zsh prompt, as you type.
    pub complete_menu: bool,
    /// Deck's skill in every coding agent's skills folder, so agents know
    /// how to run commands and drive the browser where you can see them.
    pub agent_skill: bool,
    /// The strip along the bottom: running commands, pages and dev servers.
    pub status_bar: bool,
    /// Background jobs (`deck job start`) start again at launch.
    pub restore_jobs: bool,
    /// Terminal tabs run in the background service, so quitting Deck
    /// leaves their shells and agents running.
    pub keep_terminals: bool,
    /// Changing the theme also sets coding agents' own (Claude Code's).
    pub match_agent_themes: bool,
    /// `deck do ACTION` may run any ⌘K action.
    pub cli_actions: bool,
    /// The offer to import from another browser was answered.
    pub import_offered: bool,
    /// Pages load without ads and trackers.
    pub block_ads: bool,
    /// Agents may read pages: their console, network, storage and text
    /// (`deck browser console|network|storage|eval|read`). Off, they can
    /// still open pages for you to look at.
    pub agents_read_pages: bool,
    /// Your shortcuts, by `workspace::SHORTCUTS` id: a key in gpui's form
    /// ("cmd-shift-k"), or "" for none. Absent ones keep Deck's.
    pub keys: std::collections::BTreeMap<String, String>,
    /// Where Deck's own AI jobs run, by `refine::JOBS` id: `claude:haiku`,
    /// `ollama:qwen2.5-coder:7b`, `lmstudio:…` or `off`. A job not listed
    /// runs on Claude with `refine_model`, as it always has.
    pub routes: std::collections::BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: theme::Theme::default(),
            font_px: theme::FONT_PX,
            zoom: DEFAULT_ZOOM,
            font_family: theme::TERM_FONT.into(),
            line_height: DEFAULT_LINE_HEIGHT,
            scheme: Scheme::default(),
            cursor: Cursor::default(),
            cursor_blink: false,
            engine: Engine::default(),
            option_as_meta: false,
            search: Search::default(),
            default_agent: None,
            notify_attention: true,
            notify_done: true,
            sound_attention: "Glass".into(),
            sound_done: "Pop".into(),
            sound_failed: "Basso".into(),
            sound_volume: 0.6,
            refine: crate::refine::Mode::default(),
            refine_model: "haiku".into(),
            auto_retry: true,
            asked_default_terminal: false,
            asked_permissions: false,
            check_updates: true,
            install_updates: true,
            soften_backgrounds: true,
            complete_menu: true,
            agent_skill: true,
            status_bar: true,
            restore_jobs: true,
            keep_terminals: true,
            match_agent_themes: false,
            cli_actions: true,
            import_offered: false,
            block_ads: true,
            agents_read_pages: true,
            keys: Default::default(),
            routes: Default::default(),
            access: Access::default(),
        }
    }
}

/// Terminal colour schemes. Deck's own is the monochrome default; the rest
/// are the classics people already have muscle memory for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scheme {
    #[default]
    Deck,
    TomorrowNight,
    CatppuccinMocha,
    TokyoNight,
    GruvboxDark,
    SolarizedDark,
    OneDark,
}

impl Scheme {
    pub const ALL: [Scheme; 7] = [
        Scheme::Deck,
        Scheme::TomorrowNight,
        Scheme::CatppuccinMocha,
        Scheme::TokyoNight,
        Scheme::GruvboxDark,
        Scheme::SolarizedDark,
        Scheme::OneDark,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scheme::Deck => "Match window",
            Scheme::TomorrowNight => "Tomorrow Night",
            Scheme::CatppuccinMocha => "Catppuccin Mocha",
            Scheme::TokyoNight => "Tokyo Night",
            Scheme::GruvboxDark => "Gruvbox Dark",
            Scheme::SolarizedDark => "Solarized Dark",
            Scheme::OneDark => "One Dark",
        }
    }

    pub fn palette(self) -> term::Palette {
        // (background, foreground, selection, the 16 ANSI colours)
        let (bg, fg, selection, ansi): (u32, u32, u32, [u32; 16]) = match self {
            Scheme::Deck => return theme::palette(),
            Scheme::TomorrowNight => (
                0x1d1f21,
                0xc5c8c6,
                0x373b41,
                [
                    0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6,
                    0x969896, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xffffff,
                ],
            ),
            Scheme::CatppuccinMocha => (
                0x1e1e2e,
                0xcdd6f4,
                0x45475a,
                [
                    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                    0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
                ],
            ),
            Scheme::TokyoNight => (
                0x1a1b26,
                0xc0caf5,
                0x283457,
                [
                    0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                    0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
                ],
            ),
            Scheme::GruvboxDark => (
                0x282828,
                0xebdbb2,
                0x504945,
                [
                    0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                    0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
                ],
            ),
            Scheme::SolarizedDark => (
                0x002b36,
                0x839496,
                0x073642,
                [
                    0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                    0x586e75, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
                ],
            ),
            Scheme::OneDark => (
                0x282c34,
                0xabb2bf,
                0x3e4451,
                [
                    0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                    0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
                ],
            ),
        };
        term::Palette {
            fg,
            bg,
            selection,
            // Other matches sit a step above the background; the current one
            // is the scheme's yellow, with its text drawn in the background.
            search: mix(bg, fg, 0.3),
            search_focus: ansi[3],
            ansi,
        }
    }

    /// The cursor: white on Deck's black, the scheme's foreground elsewhere.
    pub fn cursor_color(self) -> u32 {
        match self {
            Scheme::Deck => theme::accent(),
            s => s.palette().fg,
        }
    }
}

/// `a` moved `t` of the way to `b`, per channel.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |c: u32, shift: u32| ((c >> shift) & 0xff) as f32;
    [16, 8, 0].iter().fold(0, |acc, &shift| {
        let v = ch(a, shift) + (ch(b, shift) - ch(a, shift)) * t;
        acc | ((v.round() as u32) << shift)
    })
}

/// Which emulator a new terminal runs on (see `term::Engine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    #[default]
    Alacritty,
    Ghostty,
}

impl Engine {
    pub const ALL: [Engine; 2] = [Engine::Alacritty, Engine::Ghostty];

    pub fn name(self) -> &'static str {
        match self {
            Engine::Alacritty => "Alacritty",
            Engine::Ghostty => "Ghostty",
        }
    }

    pub fn term(self) -> term::Engine {
        match self {
            Engine::Alacritty => term::Engine::Alacritty,
            Engine::Ghostty => term::Engine::Ghostty,
        }
    }
}

/// The cursor a program gets until it asks for its own shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cursor {
    #[default]
    Block,
    Bar,
    Underline,
}

impl Cursor {
    pub const ALL: [Cursor; 3] = [Cursor::Block, Cursor::Bar, Cursor::Underline];

    pub fn name(self) -> &'static str {
        match self {
            Cursor::Block => "Block",
            Cursor::Bar => "Bar",
            Cursor::Underline => "Underline",
        }
    }

    pub fn shape(self) -> term::CursorShape {
        match self {
            Cursor::Block => term::CursorShape::Block,
            Cursor::Bar => term::CursorShape::Beam,
            Cursor::Underline => term::CursorShape::Underline,
        }
    }
}

/// Where the address bar sends anything that is not an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Search {
    #[default]
    Google,
    DuckDuckGo,
    Bing,
    Brave,
    Kagi,
}

impl Search {
    pub const ALL: [Search; 5] = [
        Search::Google,
        Search::DuckDuckGo,
        Search::Bing,
        Search::Brave,
        Search::Kagi,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Search::Google => "Google",
            Search::DuckDuckGo => "DuckDuckGo",
            Search::Bing => "Bing",
            Search::Brave => "Brave",
            Search::Kagi => "Kagi",
        }
    }

    /// The results page for a query that is already URL-encoded.
    pub fn url(self, query_encoded: &str) -> String {
        let base = match self {
            Search::Google => "https://www.google.com/search?q=",
            Search::DuckDuckGo => "https://duckduckgo.com/?q=",
            Search::Bing => "https://www.bing.com/search?q=",
            Search::Brave => "https://search.brave.com/search?q=",
            Search::Kagi => "https://kagi.com/search?q=",
        };
        format!("{base}{query_encoded}")
    }
}

/// What a new chat may do without asking. The chat's own chip changes it
/// per thread; this is only where new threads start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    /// Ask before edits.
    Ask,
    /// Auto-accept edits; commands still ask (except `deck …`).
    #[default]
    Edits,
    /// Never ask.
    Full,
}

impl Access {
    pub const ALL: [Access; 3] = [Access::Ask, Access::Edits, Access::Full];

    pub fn name(self) -> &'static str {
        match self {
            Access::Ask => "Ask first",
            Access::Edits => "Accept edits",
            Access::Full => "Full access",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Access::Ask => "Asks before every edit and command",
            Access::Edits => "Edits files on its own, asks before commands",
            Access::Full => "Edits and runs commands without asking",
        }
    }

    pub fn of(mode: agent_core::PermissionMode) -> Access {
        match mode {
            agent_core::PermissionMode::Yolo => Access::Full,
            agent_core::PermissionMode::Edit => Access::Edits,
            _ => Access::Ask,
        }
    }

    pub fn mode(self) -> agent_core::PermissionMode {
        match self {
            Access::Ask => agent_core::PermissionMode::Ask,
            Access::Edits => agent_core::PermissionMode::Edit,
            Access::Full => agent_core::PermissionMode::Yolo,
        }
    }
}

impl Settings {
    pub fn terminal_font(&self) -> f32 {
        self.font_px * self.zoom
    }

    /// What every terminal pane draws and types with.
    pub fn term_look(&self) -> crate::terminal::Look {
        crate::terminal::Look {
            family: self.font_family.clone().into(),
            font_px: self.terminal_font(),
            line: self.line_height,
            palette: self.scheme.palette(),
            cursor_color: self.scheme.cursor_color(),
            cursor: self.cursor.shape(),
            blink: self.cursor_blink,
            option_meta: self.option_as_meta,
            soften: self.soften_backgrounds,
            complete: self.complete_menu,
        }
    }

    /// The results page for `query_encoded` on the chosen search engine.
    pub fn search_url(&self, query_encoded: &str) -> String {
        self.search.url(query_encoded)
    }

    /// The next zoom step in `dir` (+1 / −1), or 100% for 0.
    pub fn stepped(&self, dir: i32) -> f32 {
        let at = ZOOMS
            .iter()
            .position(|z| (z - self.zoom).abs() < 0.01)
            // Off the ladder (an old file): start from the nearest step.
            .unwrap_or_else(|| {
                ZOOMS
                    .iter()
                    .enumerate()
                    .min_by(|a, b| (a.1 - self.zoom).abs().total_cmp(&(b.1 - self.zoom).abs()))
                    .map_or(3, |(i, _)| i)
            }) as i32;
        match dir {
            0 => DEFAULT_ZOOM,
            d => ZOOMS[(at + d).clamp(0, ZOOMS.len() as i32 - 1) as usize],
        }
    }
}

/// The on-disk shape, tolerant of files written by older versions (which had
/// only `font_px`, then the terminal's own zoom) and of any single field that
/// no longer parses: that one field takes its default, the rest are kept.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Stored {
    #[serde(deserialize_with = "lenient")]
    theme: Option<theme::Theme>,
    #[serde(deserialize_with = "lenient")]
    font_px: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    zoom: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    font_family: Option<String>,
    #[serde(deserialize_with = "lenient")]
    line_height: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    scheme: Option<Scheme>,
    #[serde(deserialize_with = "lenient")]
    cursor: Option<Cursor>,
    #[serde(deserialize_with = "lenient")]
    cursor_blink: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    engine: Option<Engine>,
    #[serde(deserialize_with = "lenient")]
    option_as_meta: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    search: Option<Search>,
    #[serde(deserialize_with = "lenient")]
    default_agent: Option<String>,
    #[serde(deserialize_with = "lenient")]
    access: Option<Access>,
    #[serde(deserialize_with = "lenient")]
    notify_attention: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    notify_done: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    sound_attention: Option<String>,
    #[serde(deserialize_with = "lenient")]
    sound_done: Option<String>,
    #[serde(deserialize_with = "lenient")]
    sound_failed: Option<String>,
    #[serde(deserialize_with = "lenient")]
    sound_volume: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    refine: Option<crate::refine::Mode>,
    #[serde(deserialize_with = "lenient")]
    refine_model: Option<String>,
    #[serde(deserialize_with = "lenient")]
    auto_retry: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    asked_default_terminal: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    asked_permissions: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    check_updates: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    install_updates: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    soften_backgrounds: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    complete_menu: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    agent_skill: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    status_bar: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    restore_jobs: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    keep_terminals: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    match_agent_themes: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    cli_actions: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    import_offered: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    block_ads: Option<bool>,
    agents_read_pages: Option<bool>,
    #[serde(deserialize_with = "lenient")]
    keys: Option<std::collections::BTreeMap<String, String>>,
    #[serde(deserialize_with = "lenient")]
    routes: Option<std::collections::BTreeMap<String, String>>,
}

/// A value that does not parse (a scheme a newer build added, a string
/// where a number belongs) reads as absent instead of failing the file.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

pub fn path() -> PathBuf {
    crate::control::socket_dir().join("settings.json")
}

fn from_stored(s: Stored) -> Settings {
    let base = theme::FONT_PX;
    let zoom_range = |z: f32| z.clamp(ZOOMS[0], ZOOMS[ZOOMS.len() - 1]);
    let (font_px, zoom) = match (s.font_px, s.zoom) {
        (Some(f), Some(z)) => (f.clamp(FONT_PX_RANGE.0, FONT_PX_RANGE.1), zoom_range(z)),
        // An older file zoomed by growing the terminal font: keep how big
        // things looked by turning that into the app zoom.
        (Some(f), None) => (base, zoom_range(f / base)),
        (None, z) => (base, zoom_range(z.unwrap_or(DEFAULT_ZOOM))),
    };
    let d = Settings::default();
    Settings {
        theme: s.theme.unwrap_or(d.theme),
        font_px,
        zoom,
        font_family: s
            .font_family
            .filter(|f| !f.trim().is_empty())
            .unwrap_or(d.font_family),
        line_height: s.line_height.map_or(d.line_height, |l| {
            l.clamp(LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1)
        }),
        scheme: s.scheme.unwrap_or(d.scheme),
        cursor: s.cursor.unwrap_or(d.cursor),
        cursor_blink: s.cursor_blink.unwrap_or(d.cursor_blink),
        engine: s.engine.unwrap_or(d.engine),
        option_as_meta: s.option_as_meta.unwrap_or(d.option_as_meta),
        search: s.search.unwrap_or(d.search),
        default_agent: s.default_agent.filter(|a| !a.is_empty()),
        access: s.access.unwrap_or(d.access),
        notify_attention: s.notify_attention.unwrap_or(d.notify_attention),
        notify_done: s.notify_done.unwrap_or(d.notify_done),
        sound_attention: s.sound_attention.unwrap_or(d.sound_attention),
        sound_done: s.sound_done.unwrap_or(d.sound_done),
        sound_failed: s.sound_failed.unwrap_or(d.sound_failed),
        sound_volume: s.sound_volume.map_or(d.sound_volume, |v| v.clamp(0., 1.)),
        refine: s.refine.unwrap_or(d.refine),
        refine_model: s
            .refine_model
            .filter(|m| !m.trim().is_empty())
            .unwrap_or(d.refine_model),
        auto_retry: s.auto_retry.unwrap_or(d.auto_retry),
        asked_default_terminal: s.asked_default_terminal.unwrap_or(d.asked_default_terminal),
        asked_permissions: s.asked_permissions.unwrap_or(d.asked_permissions),
        check_updates: s.check_updates.unwrap_or(d.check_updates),
        install_updates: s.install_updates.unwrap_or(d.install_updates),
        soften_backgrounds: s.soften_backgrounds.unwrap_or(d.soften_backgrounds),
        complete_menu: s.complete_menu.unwrap_or(d.complete_menu),
        agent_skill: s.agent_skill.unwrap_or(d.agent_skill),
        status_bar: s.status_bar.unwrap_or(d.status_bar),
        restore_jobs: s.restore_jobs.unwrap_or(d.restore_jobs),
        keep_terminals: s.keep_terminals.unwrap_or(d.keep_terminals),
        match_agent_themes: s.match_agent_themes.unwrap_or(d.match_agent_themes),
        cli_actions: s.cli_actions.unwrap_or(d.cli_actions),
        import_offered: s.import_offered.unwrap_or(d.import_offered),
        block_ads: s.block_ads.unwrap_or(d.block_ads),
        agents_read_pages: s.agents_read_pages.unwrap_or(d.agents_read_pages),
        // A key that does not parse is dropped, so Deck's comes back.
        keys: s
            .keys
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, k)| k.is_empty() || gpui::Keystroke::parse(k).is_ok())
            .collect(),
        // So does a route that does not parse: the job goes back to Claude.
        routes: s
            .routes
            .unwrap_or_default()
            .into_iter()
            .filter(|(job, r)| !job.is_empty() && crate::local::Route::parse(r).is_some())
            .collect(),
    }
}

/// Loading or saving also hands the routes to `refine`, so Deck's AI jobs
/// run where Settings says from launch on.
pub fn load() -> Settings {
    let stored: Stored = std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let s = from_stored(stored);
    crate::refine::set_routes(&s.routes);
    s
}

/// Written to a temp file and renamed, so a crash mid-write cannot leave a
/// half file that resets everything on the next launch.
pub fn save(s: &Settings) {
    crate::refine::set_routes(&s.routes);
    let target = path();
    let tmp = target.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
        std::fs::rename(&tmp, &target)
    };
    if let Err(e) = write() {
        tracing::warn!("could not save settings: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Settings {
        from_stored(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn old_files_keep_their_size_as_app_zoom() {
        assert_eq!(parse("{}"), Settings::default());
        let s = parse(r#"{"font_px": 19.5}"#);
        assert_eq!(s.font_px, theme::FONT_PX);
        assert!((s.zoom - 1.5).abs() < 1e-6);
        assert_eq!(parse(r#"{"font_px": 13, "zoom": 1.2}"#).zoom, 1.2);
        assert_eq!(parse(r#"{"zoom": 9}"#).zoom, 2.0);
    }

    #[test]
    fn a_file_from_before_the_settings_page_gets_every_new_default() {
        let s = parse(r#"{"font_px": 14.0, "zoom": 1.1}"#);
        assert_eq!(
            s,
            Settings {
                font_px: 14.,
                zoom: 1.1,
                ..Settings::default()
            }
        );
        assert_eq!(s.search, Search::Google);
        assert_eq!(s.font_family, crate::theme::TERM_FONT);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let s = Settings {
            theme: theme::Theme::Graphite,
            font_px: 15.,
            zoom: 1.2,
            font_family: "JetBrains Mono".into(),
            line_height: 1.6,
            scheme: Scheme::CatppuccinMocha,
            cursor: Cursor::Bar,
            cursor_blink: true,
            engine: Engine::Ghostty,
            option_as_meta: true,
            search: Search::Kagi,
            default_agent: Some("codex".into()),
            access: Access::Full,
            notify_attention: false,
            notify_done: true,
            sound_attention: "Ping".into(),
            sound_done: "None".into(),
            sound_failed: "Basso".into(),
            sound_volume: 0.4,
            refine: crate::refine::Mode::Review,
            refine_model: "sonnet".into(),
            auto_retry: false,
            asked_default_terminal: true,
            asked_permissions: true,
            check_updates: false,
            install_updates: false,
            soften_backgrounds: false,
            complete_menu: false,
            agent_skill: false,
            status_bar: false,
            restore_jobs: false,
            keep_terminals: false,
            match_agent_themes: false,
            cli_actions: false,
            import_offered: true,
            block_ads: false,
            agents_read_pages: true,
            keys: [("palette".to_string(), "cmd-p".to_string())].into(),
            routes: [
                ("rewrite".to_string(), "ollama:qwen2.5-coder:7b".to_string()),
                ("shell".to_string(), "off".to_string()),
            ]
            .into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(parse(&json), s);
        assert!(json.contains(r#""scheme":"catppuccin-mocha""#));
        assert!(json.contains(r#""theme":"graphite""#));
    }

    #[test]
    fn one_bad_value_resets_only_itself() {
        let s = parse(
            r#"{"zoom": 1.2, "scheme": "neon-future", "line_height": "tall", "cursor": "bar", "font_family": ""}"#,
        );
        assert_eq!(s.zoom, 1.2);
        assert_eq!(s.scheme, Scheme::Deck);
        assert_eq!(s.line_height, DEFAULT_LINE_HEIGHT);
        assert_eq!(s.cursor, Cursor::Bar);
        assert_eq!(s.font_family, crate::theme::TERM_FONT);
        assert_eq!(parse(r#"{"line_height": 9}"#).line_height, 2.0);
    }

    #[test]
    fn routes_keep_only_what_parses() {
        let s =
            parse(r#"{"routes": {"rewrite": "ollama:llama3.2:3b", "shell": "gpt:4", "": "off"}}"#);
        assert_eq!(
            s.routes,
            [("rewrite".to_string(), "ollama:llama3.2:3b".to_string())].into()
        );
        assert!(parse(r#"{"routes": "all local"}"#).routes.is_empty());
        assert!(
            Settings::default().routes.is_empty(),
            "every job starts on Claude"
        );
    }

    #[test]
    fn search_engines_build_their_results_pages() {
        let s = Settings::default();
        assert_eq!(
            s.search_url("rust+gpui"),
            "https://www.google.com/search?q=rust+gpui"
        );
        assert_eq!(
            Search::DuckDuckGo.url("a%26b"),
            "https://duckduckgo.com/?q=a%26b"
        );
    }

    #[test]
    fn schemes_have_distinct_backgrounds_and_deck_is_the_theme() {
        assert_eq!(Scheme::Deck.palette(), theme::palette());
        let mut bgs: Vec<u32> = Scheme::ALL.iter().map(|s| s.palette().bg).collect();
        bgs.dedup();
        assert_eq!(bgs.len(), Scheme::ALL.len());
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
        assert_eq!(mix(0x102030, 0x102030, 0.3), 0x102030);
    }

    #[test]
    fn zoom_walks_the_ladder_and_stops_at_the_ends() {
        let mut s = Settings::default();
        s.zoom = s.stepped(1);
        assert_eq!(s.zoom, 1.1);
        s.zoom = s.stepped(-1);
        s.zoom = s.stepped(-1);
        assert_eq!(s.zoom, 0.9);
        s.zoom = 2.0;
        assert_eq!(s.stepped(1), 2.0);
        s.zoom = 1.31; // off the ladder: nearest is 1.35
        assert_eq!(s.stepped(1), 1.5);
        assert_eq!(s.stepped(0), 1.0);
    }
}
