//! Icons: real SVGs through gpui's `svg()`, tinted by `text_color`.
//!
//! gpui rasterises an SVG to an alpha mask, so one file serves every colour
//! and state. Files live in `assets/icons/` and are embedded at compile time.
//! The set (and its 1.8px round stroke) comes from yourcode; keep additions in
//! that style so nothing on a surface reads heavier than its neighbours.

use crate::theme::u;
use gpui::prelude::*;
use gpui::{div, rgb, rgba, svg, AnyElement, AssetSource, SharedString};
use std::borrow::Cow;

/// `Variant => "file.svg"`, once; the enum, paths and embeds derive from it.
macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Icon { $($variant),* }

        impl Icon {
            pub fn path(self) -> &'static str {
                match self { $(Icon::$variant => concat!("icons/", $file)),* }
            }
        }

        const FILES: &[(&str, &[u8])] = &[
            $(($file, include_bytes!(concat!("../assets/icons/", $file)) as &[u8])),*
        ];
    };
}

icons! {
    Sidebar => "sidebar.svg",
    PanelRight => "panel-right.svg",
    Split => "split.svg",
    Plus => "plus.svg",
    Close => "close.svg",
    ChevronDown => "chevron-down.svg",
    ChevronUp => "chevron-up.svg",
    ChevronRight => "chevron-right.svg",
    ArrowLeft => "arrow-left.svg",
    ArrowRight => "arrow-right.svg",
    ArrowUpRight => "arrow-up-right.svg",
    Refresh => "refresh.svg",
    Globe => "globe.svg",
    Terminal => "terminal.svg",
    Folder => "folder.svg",
    File => "file.svg",
    Files => "files.svg",
    Code => "code.svg",
    Git => "git.svg",
    Info => "info.svg",
    Copy => "copy.svg",
    Clipboard => "clipboard.svg",
    Scissors => "scissors.svg",
    SelectAll => "select-all.svg",
    Eraser => "eraser.svg",
    SplitDown => "split-down.svg",
    Link => "link.svg",
    CloseOthers => "close-others.svg",
    ArrowUp => "arrow-up.svg",
    Check => "check.svg",
    Spinner => "spinner.svg",
    ListCheck => "list-check.svg",
    List => "list.svg",
    Pencil => "pencil.svg",
    Search => "search.svg",
    Download => "download.svg",
    Clock => "clock.svg",
    Pin => "pin.svg",
    Bell => "bell.svg",
    Bolt => "bolt.svg",
    Shield => "shield.svg",
    Settings => "settings.svg",
    Snippet => "snippet.svg",
    User => "user.svg",
    Lock => "lock.svg",
    Theme => "theme.svg",
    Gauge => "gauge.svg",
    Cursor => "agent-cursor.svg",
    Amp => "agent-amp.svg",
    Aider => "agent-aider.svg",
    Stop => "stop.svg",
    Maximize => "maximize.svg",
    Package => "package.svg",
    Chart => "chart.svg",
    Import => "import.svg",
    Claude => "agent-claude.svg",
    Codex => "agent-codex.svg",
    Gemini => "agent-gemini.svg",
    Grok => "agent-grok.svg",
    OpenCode => "agent-opencode.svg",
    GenericAgent => "agent-generic.svg",
}

impl Icon {
    /// An agent's mark by its command name; unknown ones get the generic mark.
    pub fn agent(cmd: &str) -> Icon {
        match cmd {
            "claude" => Icon::Claude,
            "codex" => Icon::Codex,
            "gemini" => Icon::Gemini,
            "grok" => Icon::Grok,
            "opencode" => Icon::OpenCode,
            "cursor-agent" | "cursor" => Icon::Cursor,
            "amp" => Icon::Amp,
            "aider" => Icon::Aider,
            _ => Icon::GenericAgent,
        }
    }

    /// The colour an agent's mark takes while it works, so a busy row is
    /// told apart from idle ones at a glance. Readable on dark and light.
    pub fn brand(self) -> Option<u32> {
        Some(match self {
            Icon::Claude => 0xd97757,
            Icon::Codex => 0x10a37f,
            Icon::Gemini => 0x4f8ff7,
            Icon::Grok => 0x9b8cf7,
            Icon::OpenCode => 0xf0a030,
            Icon::Cursor => 0x8a8f98,
            Icon::Amp => 0xf35b5b,
            Icon::Aider => 0x4fb3a5,
            _ => return None,
        })
    }
}

/// An agent's mark: its brand icon, or for one without (Vibe, Crush) its
/// initial in a small rounded square, rather than a stock robot.
pub fn agent_mark(cmd: &str, name: &str, size: f32, color: u32) -> AnyElement {
    let glyph = Icon::agent(cmd);
    if glyph != Icon::GenericAgent {
        return icon(glyph, size, color);
    }
    let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(crate::theme::u(size))
        .rounded(crate::theme::u(size / 4.))
        .border_1()
        .border_color(rgba(crate::theme::alpha(color, 0x99)))
        .text_size(crate::theme::u(size * 0.62))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(color))
        .child(initial)
        .into_any_element()
}

pub fn icon(kind: Icon, size: f32, color: u32) -> AnyElement {
    svg()
        .path(kind.path())
        .size(u(size))
        .flex_none()
        .text_color(rgb(color))
        .into_any_element()
}

/// Animations here run on one clock at [`ANIM_FPS`] rather than at the
/// display's refresh rate. Every frame redraws the whole window, so a spinner
/// asking for 120 frames a second kept the main thread and the GPU busy for
/// as long as an agent worked; at 15 it looks the same. Building one records
/// the view drawing it, and the workspace's animation tick (see
/// [`take_animating`]) notifies those views alone: a spinner in the sidebar
/// redraws the sidebar, while every other pane (cached) keeps its last frame.
pub const ANIM_FPS: f32 = 15.;

/// What drew an animation since the last tick.
pub enum Animating {
    Nothing,
    /// These views: notify them.
    Views(Vec<gpui::EntityId>),
    /// Something outside any view: redraw the window.
    Window,
}

thread_local! {
    static ANIMATING: std::cell::RefCell<(Vec<gpui::EntityId>, bool)> = const { std::cell::RefCell::new((Vec::new(), false)) };
}

/// What animated since the last call.
pub fn take_animating() -> Animating {
    ANIMATING.with_borrow_mut(|(views, window)| {
        if std::mem::take(window) {
            views.clear();
            Animating::Window
        } else if views.is_empty() {
            Animating::Nothing
        } else {
            Animating::Views(std::mem::take(views))
        }
    })
}

/// Where in a cycle of `period_ms` the clock is, 0–1, in steps of a frame,
/// and a note that the window must come back for the next step.
pub fn phase(period_ms: u64) -> f32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    ANIMATING.with_borrow_mut(|(views, window)| match gpui::rendering_view() {
        Some(id) if !views.contains(&id) => views.push(id),
        Some(_) => {}
        None => *window = true,
    });
    let frame = (START.get_or_init(Instant::now).elapsed().as_secs_f32() * ANIM_FPS).floor();
    (frame / ANIM_FPS * 1000. / period_ms as f32).fract()
}

/// Work in progress: a comet, its head bright and its tail fading, turning
/// a little faster than once a second.
pub fn spinner(_id: impl Into<gpui::ElementId>, size: f32, color: u32) -> AnyElement {
    use gpui::{percentage, Transformation};
    svg()
        .path(Icon::Spinner.path())
        .size(u(size))
        .flex_none()
        .text_color(rgb(color))
        .with_transformation(Transformation::rotate(percentage(phase(850))))
        .into_any_element()
}

/// An agent at work: its own mark turns in eased quarter steps and breathes
/// in between, the way Claude Code's glyph does in a terminal. Still marks
/// read as idle; this one reads as thinking.
pub fn thinking(_id: impl Into<gpui::ElementId>, glyph: Icon, size: f32, color: u32) -> AnyElement {
    use gpui::{percentage, size as sz, Transformation};
    let t = phase(2400);
    let (step, within) = ((t * 4.).floor(), (t * 4.).fract());
    let turn = (step + ease_in_out(within)) / 4.;
    // Smallest mid-turn, full size at rest.
    let scale = 1. - 0.16 * (within * std::f32::consts::PI).sin();
    svg()
        .path(glyph.path())
        .size(u(size))
        .flex_none()
        .text_color(rgb(color))
        .with_transformation(Transformation::rotate(percentage(turn)).with_scaling(sz(scale, scale)))
        .into_any_element()
}

/// Text with a band of contrast sweeping across it: "Working…" while an
/// agent is busy. It runs from the theme's dim label tone to its text
/// colour, so on a light theme the band darkens instead of fading into the
/// background, and on the warm and blue themes it stays in their tint.
pub fn shimmer(_id: impl Into<gpui::ElementId>, text: SharedString) -> AnyElement {
    use gpui::{HighlightStyle, StyledText};
    let t = phase(1900);
    let n = text.chars().count() as f32;
    let band = 5.;
    let at = t * (n + band * 2.) - band;
    let (base, peak) = (crate::theme::dim(), crate::theme::text());
    let lights: Vec<_> = text
        .char_indices()
        .enumerate()
        .map(|(i, (byte, ch))| {
            let k = (1. - (i as f32 - at).abs() / band).max(0.);
            (
                byte..byte + ch.len_utf8(),
                HighlightStyle {
                    color: Some(rgb(mix(base, peak, k)).into()),
                    ..Default::default()
                },
            )
        })
        .collect();
    gpui::div()
        .flex_none()
        .child(StyledText::new(text.clone()).with_highlights(crate::theme::safe_highlights(&text, lights)))
        .into_any_element()
}

fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4. * t * t * t
    } else {
        1. - (-2. * t + 2.).powi(3) / 2.
    }
}

/// `a` → `b` by `k` in 0–1, per channel.
fn mix(a: u32, b: u32, k: f32) -> u32 {
    let ch = |shift: u32| {
        let (x, y) = ((a >> shift) & 0xff, (b >> shift) & 0xff);
        ((x as f32 + (y as f32 - x as f32) * k).round() as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// Serves the embedded icons to gpui.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let name = path.rsplit('/').next().unwrap_or(path);
        Ok(FILES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(FILES
            .iter()
            .map(|(n, _)| SharedString::from(format!("icons/{n}")))
            .collect())
    }
}
