//! Colours and metrics. The window wears one theme at a time, picked in
//! Settings and applied everywhere at once: sidebar, panes, chats, menus,
//! the terminal's own palette, and the appearance of native views (pages,
//! traffic lights, scrollbars).
//!
//! Every theme is quiet: hierarchy comes from lightness alone, and colour is
//! left to content (terminal output, web pages). A theme may carry one
//! accent, kept to the caret, focus and selection. Edges are drawn in
//! translucent text colour, so they sit right on any surface.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    /// Black: a #000 sidebar, near-black panes.
    #[default]
    Black,
    /// macOS's own dark greys.
    Graphite,
    /// Dark with a blue cast.
    Midnight,
    /// macOS light.
    Light,
    /// Black or Light, following the system's appearance.
    System,
    /// True black for OLED: #000 everywhere, cards drawn by their edges,
    /// white for the caret.
    Obsidian,
    /// A soft, warm black with one amber accent.
    Carbon,
    /// A deep blue-black with one cyan accent.
    Abyss,
    /// Dark and translucent: the desktop shows, blurred, through the
    /// sidebar and the window's edges; panes stay solid enough to read.
    Glass,
    /// Glass, light.
    Frost,
}

impl Theme {
    /// In the order Settings shows them: System, then dark to light.
    pub const ALL: [Theme; 10] = [
        Theme::System,
        Theme::Obsidian,
        Theme::Black,
        Theme::Carbon,
        Theme::Graphite,
        Theme::Abyss,
        Theme::Midnight,
        Theme::Glass,
        Theme::Frost,
        Theme::Light,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Theme::Black => "Black",
            Theme::Graphite => "Graphite",
            Theme::Midnight => "Midnight",
            Theme::Light => "Light",
            Theme::System => "System",
            Theme::Obsidian => "Obsidian",
            Theme::Carbon => "Carbon",
            Theme::Abyss => "Abyss",
            Theme::Glass => "Glass",
            Theme::Frost => "Frost",
        }
    }

    /// The colours it stands for, given whether the system is dark.
    pub fn colors(self, system_dark: bool) -> &'static Colors {
        &ALL[self.index(system_dark) as usize]
    }

    fn index(self, system_dark: bool) -> u8 {
        match self {
            Theme::Black => 0,
            Theme::Graphite => 1,
            Theme::Midnight => 2,
            Theme::Light => 3,
            Theme::System if system_dark => 0,
            Theme::System => 3,
            Theme::Obsidian => 4,
            Theme::Carbon => 5,
            Theme::Abyss => 6,
            Theme::Glass => 7,
            Theme::Frost => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub dark: bool,
    /// Translucent: the window is blurred and the chrome lets it through.
    pub glass: bool,
    pub sidebar: u32,
    pub pane: u32,
    /// Cards and the composer: a step up from the pane.
    pub raised: u32,
    /// Filled controls and the selected row.
    pub surface: u32,
    pub surface_hover: u32,
    pub row_hover: u32,
    /// The open sidebar row: lifted clear of the hover tone (white on
    /// Light, the surface on the dark themes), so a hovered row never
    /// reads as the open one.
    pub row_active: u32,
    pub border: u32,
    pub text: u32,
    pub text_2: u32,
    /// Secondary labels, kept at 4.5:1 or better on the pane.
    pub dim: u32,
    /// Focus, carets, the cursor: the one emphasis colour.
    pub accent: u32,
    pub selection: u32,
    /// Fill behind the terminal's find matches; the current one is the accent.
    pub search: u32,
    /// Structural edges (cards, panes), RGBA.
    pub hairline: u32,
    /// The edge of something you type into, RGBA.
    pub input_edge: u32,
    /// The card that has the keyboard, in a split, RGBA.
    pub focus_edge: u32,
    /// An edge that has to be seen (a failed card, a chosen option), RGBA.
    pub strong_edge: u32,
    /// Over the panes of a split that do not have the keyboard, RGBA.
    pub veil: u32,
    /// Floating menus.
    pub menu: u32,
    /// A faint fill behind inline code and quiet chips, RGBA.
    pub tint: u32,
    /// The terminal's sixteen colours under this theme.
    pub ansi: [u32; 16],
}

const DARK_ANSI: [u32; 16] = [
    0x2b2b33, 0xff7b7b, 0x6ddf9a, 0xffc069, 0x8b9dff, 0xc4a6ff, 0x8fd3f4, 0xa0a2ad, 0x565660,
    0xff9c9c, 0x92e9b4, 0xffd08e, 0xa8b5ff, 0xd4c1ff, 0xaee0f8, 0xecedf1,
];

/// Dark enough to read on white: the same hues, deepened.
const LIGHT_ANSI: [u32; 16] = [
    0x1d1d1f, 0xc4282e, 0x1a7f37, 0x9a6700, 0x0550ae, 0x8250df, 0x1b7c83, 0x6e7781, 0x57606a,
    0xd1242f, 0x1f883d, 0xb08800, 0x0969da, 0x8a63d2, 0x3192aa, 0x8c959f,
];

/// Clean, saturated hues that hold on pure black, every one 7:1 or better.
const OBSIDIAN_ANSI: [u32; 16] = [
    0x262626, 0xfe6b6b, 0x5ddb8b, 0xf4cf5a, 0x5ea3fd, 0xd28ae8, 0x5ed8e8, 0xc6c6c6, 0x636363,
    0xfe9b97, 0x9beeb3, 0xfbe194, 0x8dbdfe, 0xe2aef2, 0x9eedf9, 0xffffff,
];

/// Earthy and soft for the warm black: terracotta, sage, wheat, slate, rose,
/// sea glass.
const CARBON_ANSI: [u32; 16] = [
    0x35322f, 0xe97e6e, 0x9cc779, 0xeec46e, 0x7db1dd, 0xd891b9, 0x80cbc3, 0xc1bdb7, 0x746d67,
    0xf6998a, 0xb5da98, 0xffdb93, 0x99c5eb, 0xe7a9cc, 0x9fded7, 0xf6f3ef,
];

/// Cool and luminous for the blue-black: rose, mint, periwinkle, lavender, sky.
const ABYSS_ANSI: [u32; 16] = [
    0x232c38, 0xf37984, 0x67dba7, 0xf3cc75, 0x78a7fc, 0xc49cf1, 0x75d7f3, 0xb4bfca, 0x5e6d80,
    0xfc9aa1, 0x98edc3, 0xfedf9e, 0x9bbffe, 0xd6b8fb, 0xa7eafe, 0xf0f4f9,
];

static ALL: [Colors; 9] = [
    // Black
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x000000,
        pane: 0x0a0a0a,
        raised: 0x111111,
        surface: 0x1c1c1c,
        surface_hover: 0x262626,
        row_hover: 0x141414,
        row_active: 0x1c1c1c,
        border: 0x1a1a1a,
        text: 0xf5f5f5,
        text_2: 0xa3a3a3,
        dim: 0x737373,
        accent: 0xffffff,
        selection: 0x3a3a3a,
        search: 0x575757,
        hairline: 0xffffff0f,
        input_edge: 0xffffff1a,
        focus_edge: 0xffffff29,
        strong_edge: 0xffffff30,
        veil: 0x00000047,
        menu: 0x161616,
        tint: 0xffffff0d,
        ansi: DARK_ANSI,
    },
    // Graphite
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x1c1c1e,
        pane: 0x232325,
        raised: 0x2a2a2c,
        surface: 0x353538,
        surface_hover: 0x3f3f43,
        row_hover: 0x2c2c2f,
        row_active: 0x353538,
        border: 0x38383b,
        text: 0xf2f2f7,
        text_2: 0xb0b0b5,
        dim: 0x8e8e93,
        accent: 0xffffff,
        selection: 0x4c4c51,
        search: 0x575757,
        hairline: 0xffffff12,
        input_edge: 0xffffff1f,
        focus_edge: 0xffffff33,
        strong_edge: 0xffffff38,
        veil: 0x0000004d,
        menu: 0x2c2c2f,
        tint: 0xffffff0f,
        ansi: DARK_ANSI,
    },
    // Midnight
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x0b0e14,
        pane: 0x10141c,
        raised: 0x161b25,
        surface: 0x1e2431,
        surface_hover: 0x272e3e,
        row_hover: 0x181d28,
        row_active: 0x1e2431,
        border: 0x202634,
        text: 0xe8ebf2,
        text_2: 0xa6aec0,
        dim: 0x76809a,
        accent: 0xffffff,
        selection: 0x313b55,
        search: 0x575757,
        hairline: 0xffffff10,
        input_edge: 0xffffff1c,
        focus_edge: 0xffffff2e,
        strong_edge: 0xffffff33,
        veil: 0x0000004d,
        menu: 0x181d28,
        tint: 0xffffff0d,
        ansi: DARK_ANSI,
    },
    // Light
    Colors {
        dark: false,
        glass: false,
        sidebar: 0xf0f0f0,
        pane: 0xffffff,
        raised: 0xf7f7f7,
        surface: 0xe6e6e6,
        surface_hover: 0xdcdcdc,
        row_hover: 0xe8e8e8,
        row_active: 0xffffff,
        border: 0xe2e2e2,
        text: 0x1d1d1f,
        text_2: 0x55555a,
        dim: 0x76767b,
        accent: 0x000000,
        selection: 0xcfcfd4,
        search: 0xc8c8cc,
        hairline: 0x0000001a,
        input_edge: 0x00000026,
        focus_edge: 0x00000040,
        strong_edge: 0x00000033,
        veil: 0xffffff66,
        menu: 0xffffff,
        tint: 0x0000000a,
        ansi: LIGHT_ANSI,
    },
    // Obsidian
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x000000,
        pane: 0x000000,
        raised: 0x0a0a0a,
        surface: 0x1a1a1a,
        surface_hover: 0x242424,
        row_hover: 0x101010,
        row_active: 0x1a1a1a,
        border: 0x1c1c1c,
        text: 0xf0f0f0,
        text_2: 0xa1a1a1,
        dim: 0x777777,
        accent: 0xffffff,
        selection: 0x343434,
        search: 0x525252,
        hairline: 0xffffff17,
        input_edge: 0xffffff24,
        focus_edge: 0xffffff38,
        strong_edge: 0xffffff40,
        veil: 0x00000059,
        menu: 0x0f0f0f,
        tint: 0xffffff0f,
        ansi: OBSIDIAN_ANSI,
    },
    // Carbon
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x0f0e0c,
        pane: 0x161513,
        raised: 0x1c1b18,
        surface: 0x282624,
        surface_hover: 0x312f2c,
        row_hover: 0x1f1d1b,
        row_active: 0x282624,
        border: 0x262422,
        text: 0xece9e4,
        text_2: 0xb1aba4,
        dim: 0x857f78,
        accent: 0xf3ae5f,
        selection: 0x4d3c2b,
        search: 0x5b564f,
        hairline: 0xece9e412,
        input_edge: 0xece9e41f,
        focus_edge: 0xece9e433,
        strong_edge: 0xece9e438,
        veil: 0x0000004d,
        menu: 0x1f1d1b,
        tint: 0xece9e40f,
        ansi: CARBON_ANSI,
    },
    // Abyss
    Colors {
        dark: true,
        glass: false,
        sidebar: 0x04060b,
        pane: 0x080e16,
        raised: 0x0d141e,
        surface: 0x16202d,
        surface_hover: 0x1d2938,
        row_hover: 0x0e1620,
        row_active: 0x16202d,
        border: 0x151d29,
        text: 0xe7ecf1,
        text_2: 0xa4b1bf,
        dim: 0x768596,
        accent: 0x5acdf9,
        selection: 0x1e3b5a,
        search: 0x425163,
        hairline: 0xe7ecf112,
        input_edge: 0xe7ecf11f,
        focus_edge: 0xe7ecf133,
        strong_edge: 0xe7ecf138,
        veil: 0x00000052,
        menu: 0x101823,
        tint: 0xe7ecf10f,
        ansi: ABYSS_ANSI,
    },
    // Glass
    Colors {
        dark: true,
        glass: true,
        sidebar: 0x0b0c10,
        pane: 0x101116,
        raised: 0x17181e,
        surface: 0x23242b,
        surface_hover: 0x2c2d35,
        row_hover: 0x1a1b21,
        row_active: 0x23242b,
        border: 0x1f2027,
        text: 0xf3f4f7,
        text_2: 0xb0b2bb,
        dim: 0x82848f,
        accent: 0xa5b4fc,
        selection: 0x34365a,
        search: 0x4b4d5a,
        hairline: 0xffffff17,
        input_edge: 0xffffff24,
        focus_edge: 0xffffff38,
        strong_edge: 0xffffff3d,
        veil: 0x0000003d,
        menu: 0x17181f,
        tint: 0xffffff10,
        ansi: DARK_ANSI,
    },
    // Frost
    Colors {
        dark: false,
        glass: true,
        sidebar: 0xf3f4f7,
        pane: 0xffffff,
        raised: 0xf8f9fb,
        surface: 0xe6e8ee,
        surface_hover: 0xdcdfe6,
        row_hover: 0xe9ebf0,
        row_active: 0xffffff,
        border: 0xe1e3ea,
        text: 0x1b1c20,
        text_2: 0x52545c,
        dim: 0x73757d,
        accent: 0x4f46e5,
        selection: 0xcdd0e6,
        search: 0xc8cad4,
        hairline: 0x0000001a,
        input_edge: 0x00000026,
        focus_edge: 0x00000040,
        strong_edge: 0x00000033,
        veil: 0xffffff59,
        menu: 0xfbfbfd,
        tint: 0x0000000a,
        ansi: LIGHT_ANSI,
    },
];

/// The theme lets the blurred desktop through.
pub fn glass() -> bool {
    current().glass
}

/// The window's own ground (behind the sidebar and the status bar):
/// see-through on a glass theme, solid otherwise.
pub fn ground() -> gpui::Rgba {
    if glass() {
        gpui::rgba((sidebar() << 8) | 0x8c)
    } else {
        gpui::rgb(sidebar())
    }
}

/// A pane's card: on glass, a little of the blur shows through.
pub fn card() -> gpui::Rgba {
    if glass() {
        gpui::rgba((pane() << 8) | 0xec)
    } else {
        gpui::rgb(pane())
    }
}

/// Index into `ALL` of the theme the window wears now.
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Wear `theme` from the next frame on.
pub fn set(theme: Theme, system_dark: bool) {
    CURRENT.store(theme.index(system_dark), Ordering::Relaxed);
}

/// The colours in use now.
pub fn current() -> &'static Colors {
    &ALL[CURRENT.load(Ordering::Relaxed) as usize]
}

pub fn sidebar() -> u32 {
    current().sidebar
}
pub fn pane() -> u32 {
    current().pane
}
pub fn raised() -> u32 {
    current().raised
}
pub fn surface() -> u32 {
    current().surface
}
pub fn surface_hover() -> u32 {
    current().surface_hover
}
pub fn row_hover() -> u32 {
    current().row_hover
}
pub fn row_active() -> u32 {
    current().row_active
}
pub fn text() -> u32 {
    current().text
}
pub fn text_2() -> u32 {
    current().text_2
}
pub fn dim() -> u32 {
    current().dim
}
pub fn accent() -> u32 {
    current().accent
}

/// Text that reads on the accent: black on a light one (white, amber,
/// cyan), white on a dark one.
pub fn on_accent() -> u32 {
    let a = accent();
    let luma = 0.299 * ((a >> 16) & 0xff) as f32 + 0.587 * ((a >> 8) & 0xff) as f32 + 0.114 * (a & 0xff) as f32;
    if luma > 150. { 0x111111 } else { 0xffffff }
}
/// Red for what takes something away (Lock, Delete), readable on every
/// theme's menu.
pub const DANGER: u32 = 0xe5534b;

pub fn selection() -> u32 {
    current().selection
}
pub fn hairline() -> u32 {
    current().hairline
}
pub fn input_edge() -> u32 {
    current().input_edge
}
pub fn focus_edge() -> u32 {
    current().focus_edge
}
pub fn strong_edge() -> u32 {
    current().strong_edge
}
pub fn menu() -> u32 {
    current().menu
}
pub fn tint() -> u32 {
    current().tint
}
pub fn is_dark() -> bool {
    current().dark
}

/// A theme colour (RGB) at an alpha, for `rgba`.
pub fn alpha(rgb: u32, a: u8) -> u32 {
    (rgb << 8) | a as u32
}

/// The terminal's own face, shipped inside Deck, and the default.
pub const TERM_FONT: &str = "JetBrains Mono";

/// Nerd Font symbols (powerline arrows, file and git icons in prompts),
/// shipped inside Deck and tried after the chosen face for any glyph it
/// lacks, then Apple's emoji.
pub const SYMBOLS_FONT: &str = "Symbols Nerd Font Mono";

/// Make the shipped faces available, before anything draws.
pub fn add_fonts(cx: &gpui::App) {
    use std::borrow::Cow;
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf")),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("could not load the shipped fonts: {e}");
    }
    if !cfg!(target_os = "macos") {
        let installed = cx.text_system().all_font_names();
        let first = |names: &[&'static str]| names.iter().copied().find(|n| installed.iter().any(|i| i == n));
        let _ = UI_FONT.set(first(UI_FACES).unwrap_or("sans-serif"));
        let _ = EMOJI_FONT.set(first(EMOJI_FACES).unwrap_or("Noto Color Emoji"));
    }
}

/// Off macOS there is no one system face: the first of these installed.
/// Inter and the desktops' own (GNOME's Adwaita Sans and Cantarell, KDE's
/// Noto Sans, Ubuntu's), then Windows' Segoe UI.
const UI_FACES: &[&str] = &[
    "Inter",
    "Inter Variable",
    "Adwaita Sans",
    "Cantarell",
    "Noto Sans",
    "Ubuntu",
    "Segoe UI",
    "DejaVu Sans",
    "Liberation Sans",
];
const EMOJI_FACES: &[&str] = &["Noto Color Emoji", "Twemoji", "Segoe UI Emoji", "Apple Color Emoji"];

static UI_FONT: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
static EMOJI_FONT: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// The interface's face: the system's on macOS, else the one `add_fonts`
/// found.
pub fn ui() -> &'static str {
    if cfg!(target_os = "macos") {
        return ".SystemUIFont";
    }
    UI_FONT.get().copied().unwrap_or("sans-serif")
}

/// Code and paths: Menlo, which every Mac has; elsewhere the JetBrains Mono
/// Deck ships, so the grid never falls back to a proportional face.
pub fn mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        TERM_FONT
    }
}

/// Colour emoji, behind every face.
pub fn emoji() -> &'static str {
    if cfg!(target_os = "macos") {
        return "Apple Color Emoji";
    }
    EMOJI_FONT.get().copied().unwrap_or("Noto Color Emoji")
}

/// A terminal face with the symbol and emoji fallbacks behind it.
pub fn term_font(family: impl Into<gpui::SharedString>) -> gpui::Font {
    gpui::Font {
        fallbacks: Some(gpui::FontFallbacks::from_fonts(vec![
            SYMBOLS_FONT.into(),
            emoji().into(),
        ])),
        ..gpui::font(family)
    }
}

pub const FONT_PX: f32 = 13.;

// One type scale for the sidebar, chat, pages and settings. Anything
// between two steps reads as a mistake, so there is nothing between them.
/// Times, counts, hints, keycaps.
pub const TEXT_META: f32 = 11.;
/// Labels, notes, chips, section heads.
pub const TEXT_SMALL: f32 = 12.;
/// Sidebar rows, menus, buttons, popovers.
pub const TEXT_UI: f32 = 13.;
/// What you read and write: chat messages, the composer.
pub const TEXT_READ: f32 = 14.;
pub const TOOLBAR_PX: f32 = 40.;
pub const SIDEBAR_PX: f32 = 256.;
pub const INSPECTOR_PX: f32 = 320.;
/// Height of the transparent titlebar the traffic lights sit in.
pub const TITLEBAR_PX: f32 = 40.;

/// The one inset between the window edge and everything inside it.
pub const GUTTER: f32 = 8.;
/// Leading padding inside a sidebar row; section labels share this edge.
pub const ROW_PAD: f32 = 8.;
/// Concentric with the window's corner: card = window radius − GUTTER.
pub const CARD_RADIUS: f32 = 10.;
pub const ROW_RADIUS: f32 = 8.;
/// Terminal text inset inside its card; the title bar aligns to it.
pub const PANE_PAD_X: f32 = 12.;
pub const PANE_PAD_Y: f32 = 8.;

/// A UI length that follows the app zoom: `u(13.)` is 13 points at 100%.
/// Built on rems, so one `set_rem_size` per frame scales the whole UI.
pub fn u(v: f32) -> gpui::Rems {
    gpui::rems(v / REM_PX)
}

/// One line of text that ends in "…" where it stops fitting.
///
/// gpui 0.2.2's `truncate()` sizes nowrap text once and keeps that size for
/// the rest of the layout, so the "…" goes where the first, wider pass put
/// it and the box then clips the line mid-letter. A one-line clamp is
/// measured again at every width, so the ellipsis lands at the real edge.
pub trait OneLine: gpui::Styled {
    fn one_line(self) -> Self {
        self.whitespace_normal().line_clamp(1).text_ellipsis()
    }
}

impl<T: gpui::Styled> OneLine for T {}

/// The rem size at 100% zoom.
pub const REM_PX: f32 = 16.;

/// `a` moved toward `b` by `t` (0-1), channel by channel.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = ((a >> s & 0xff) as f32, (b >> s & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}

/// The terminal palette that matches the theme: its text on its pane.
pub fn palette() -> term::Palette {
    let c = current();
    term::Palette {
        fg: c.text,
        bg: c.pane,
        // A soft blue, as macOS marks selected text: it reads as a
        // selection at a glance (a grey read as a stain, or as the pane
        // itself going grey over a whole screen), and text stays legible.
        selection: if c.dark { mix(c.pane, 0x3d74d6, 0.36) } else { mix(c.pane, 0x9cc2ff, 0.62) },
        search: c.search,
        search_focus: c.accent,
        ansi: c.ansi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative luminance contrast, WCAG's formula.
    fn contrast(a: u32, b: u32) -> f64 {
        let lum = |c: u32| {
            let ch = |v: u32| {
                let v = v as f64 / 255.;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(c >> 16 & 0xff) + 0.7152 * ch(c >> 8 & 0xff) + 0.0722 * ch(c & 0xff)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_theme_keeps_its_text_readable() {
        for c in &ALL {
            for bg in [c.pane, c.sidebar, c.raised] {
                assert!(contrast(c.text, bg) >= 12., "{c:?}");
                assert!(
                    contrast(c.text_2, bg) >= 4.5,
                    "{:06x} on {bg:06x}",
                    c.text_2
                );
                assert!(contrast(c.dim, bg) >= 3.9, "{:06x} on {bg:06x}", c.dim);
            }
        }
    }

    /// `rgba` laid over `rgb`, as the GPU blends it.
    fn over(rgba: u32, rgb: u32) -> u32 {
        let a = (rgba & 0xff) as f64 / 255.;
        let ch = |s: u32| {
            let (f, b) = ((rgba >> (s + 8)) & 0xff, (rgb >> s) & 0xff);
            ((f as f64 * a + b as f64 * (1. - a)).round() as u32) << s
        };
        ch(16) | ch(8) | ch(0)
    }

    #[test]
    fn selection_matches_and_edges_show_without_shouting() {
        for c in &ALL {
            assert!(contrast(c.selection, c.pane) >= 1.5, "{:06x}", c.selection);
            assert!(contrast(c.text, c.selection) >= 7., "{:06x}", c.selection);
            assert_ne!(c.search, c.selection);
            assert!(contrast(c.pane, c.accent) >= 4.5, "{:06x}", c.accent);
            let edge = contrast(over(c.hairline, c.pane), c.pane);
            assert!((1.1..1.3).contains(&edge), "{:08x}: {edge:.2}", c.hairline);
        }
    }

    #[test]
    fn the_open_row_stands_apart_from_a_hovered_one() {
        for c in &ALL {
            let (open, hover) = (contrast(c.row_active, c.sidebar), contrast(c.row_hover, c.sidebar));
            assert!(open > hover, "{:06x} vs {:06x} on {:06x}", c.row_active, c.row_hover, c.sidebar);
            assert!(contrast(c.text, c.row_active) >= 7., "{:06x}", c.row_active);
        }
    }

    #[test]
    fn dark_terminal_colours_read_on_their_pane() {
        for c in ALL.iter().filter(|c| c.dark) {
            for (i, &a) in c.ansi.iter().enumerate() {
                if i % 8 != 0 {
                    assert!(contrast(a, c.pane) >= 4.5, "{a:06x} on {:06x}", c.pane);
                }
            }
        }
    }

    #[test]
    fn every_theme_is_listed_once_and_keeps_its_name() {
        for (i, t) in Theme::ALL.iter().enumerate() {
            assert!(!Theme::ALL[..i].contains(t), "{t:?} twice");
            let json = serde_json::to_string(t).unwrap();
            assert_eq!(json, format!("\"{}\"", t.name().to_lowercase()));
            assert_eq!(serde_json::from_str::<Theme>(&json).unwrap(), *t);
        }
        // Each named theme wears its own colours.
        let mut seen: Vec<u8> = Theme::ALL
            .iter()
            .filter(|t| **t != Theme::System)
            .map(|t| t.index(true))
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), ALL.len());
    }

    #[test]
    fn system_follows_the_appearance() {
        assert!(Theme::System.colors(true).dark);
        assert!(!Theme::System.colors(false).dark);
        assert_eq!(alpha(0xffffff, 0x30), 0xffffff30);
    }
}

/// Highlights gpui can lay out: inside `text`, on character boundaries,
/// in order and not overlapping (gpui assumes all of that, and a range
/// past the end or inside a character aborts the app while drawing).
pub fn safe_highlights(
    text: &str,
    mut hl: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)>,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let snap = |mut i: usize| {
        i = i.min(text.len());
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    for (r, _) in hl.iter_mut() {
        *r = snap(r.start)..snap(r.end);
    }
    hl.sort_by_key(|(r, _)| r.start);
    let mut out: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::with_capacity(hl.len());
    let mut at = 0;
    for (r, style) in hl {
        let start = r.start.max(at);
        if start < r.end {
            at = r.end;
            out.push((start..r.end, style));
        }
    }
    out
}

/// Runs gpui can lay out: lengths that end on character boundaries and
/// add up to exactly `text`'s length (too long or too short aborts).
pub fn safe_runs(text: &str, runs: Vec<gpui::TextRun>) -> Vec<gpui::TextRun> {
    let mut out: Vec<gpui::TextRun> = Vec::with_capacity(runs.len());
    let mut at = 0;
    for mut run in runs {
        if at >= text.len() {
            break;
        }
        let mut end = (at + run.len).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        if end > at {
            run.len = end - at;
            at = end;
            out.push(run);
        }
    }
    if at < text.len() {
        match out.last_mut() {
            Some(last) => last.len += text.len() - at,
            None => return Vec::new(),
        }
    }
    out
}

#[cfg(test)]
mod safe_text_tests {
    use super::*;

    #[test]
    fn highlights_and_runs_fit_the_text() {
        let text = "a—b😀c";
        let hl = vec![
            (0..9, gpui::HighlightStyle::default()),
            (2..3, gpui::HighlightStyle::default()),
            (5..50, gpui::HighlightStyle::default()),
        ];
        let got = safe_highlights(text, hl);
        for (r, _) in &got {
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end) && r.end <= text.len());
        }
        assert!(got.windows(2).all(|w| w[0].0.end <= w[1].0.start));
        let run = |len| gpui::TextRun {
            len,
            font: gpui::font("Menlo"),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = safe_runs(text, vec![run(2), run(100)]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
        let runs = safe_runs(text, vec![run(1)]);
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    }
}
