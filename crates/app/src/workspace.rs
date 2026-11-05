//! The window: vertical tabs on the left, the active tab in the middle (a
//! split tree of terminal and browser panes), the inspector on the right, and
//! the control-plane dispatcher that lets anything in a pane drive the rest.

use crate::browser::{BrowserEvent, BrowserPane};
use crate::chat::{ChatEnv, ChatEvent, ChatPane, ChatStatus};
use crate::control::{self, Reply};
use crate::icon::{icon, shimmer, spinner, thinking, Icon};
use crate::inspect;
use crate::inspector::{self, Inspector};
use crate::launcher::{Choice, LauncherEvent, LauncherPane, Pick};
use crate::palette::{Item, Kind, Palette, PaletteEvent};
use crate::review::{ReviewEvent, ReviewPane};
use crate::settings_pane::{SettingsEvent, SettingsPane};
use crate::terminal::{TerminalEvent, TerminalPane};
use crate::theme::{self, u, OneLine};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    actions, canvas, div, px, Animation, AnimationExt, rgb, rgba, AnyElement, AnyView, App, Context, ElementId, Entity,
    FontWeight, KeyBinding, Menu, MenuItem, MouseButton, OsAction, SharedString, StyleRefinement,
    Subscription, Task, WeakEntity, Window,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

actions!(
    deck,
    [
        OpenUsage,
        OpenPackages,
        NextNeedsYou,
        ZoomPane,
        FilterSidebar,
        ToggleDeck,
        AskCommand,
        CloseIdleTabs,
        ImportBrowserData,
        NewTab,
        NewTerminal,
        NewBrowser,
        NewChat,
        QuickOpen,
        FocusLeft,
        FocusRight,
        FocusUp,
        FocusDown,
        OpenFiles,
        OpenChanges,
        OpenSettings,
        ToggleAgents,
        SplitRight,
        SplitDown,
        ClosePane,
        NextTab,
        PrevTab,
        RecentTab,
        RecentTabBack,
        MoveToTab,
        ConnectHost,
        ShowTurnDiff,
        NextPane,
        PrevPane,
        FocusAddress,
        Reload,
        ToggleDevTools,
        CommandPalette,
        ActionPalette,
        TabSwitcher,
        InsertSnippet,
        SaveSnippet,
        EditSnippets,
        LockDeck,
        RewritePrompt,
        MyWords,
        NewControl,
        RestartControl,
        ToggleSidebar,
        ToggleSidebarView,
        ToggleInspector,
        Quit,
        Copy,
        Cut,
        Paste,
        SelectAll,
        Undo,
        Redo,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ClearTerminal,
        Find,
        FindNext,
        FindPrevious,
        Tab1,
        Tab2,
        Tab3,
        Tab4,
        Tab5,
        Tab6,
        Tab7,
        Tab8,
        Tab9
    ]
);

/// A shortcut you can change in Settings › Keyboard.
pub struct Shortcut {
    /// Its key in settings.json's `keys`.
    pub id: &'static str,
    pub name: &'static str,
    /// The section Settings lists it under.
    pub group: &'static str,
    /// What Deck ships with; the first is the one shown.
    pub defaults: &'static [&'static str],
    pub context: Option<&'static str>,
    bind: fn(&str, Option<&str>) -> KeyBinding,
}

macro_rules! shortcut {
    ($id:literal, $name:literal, $group:literal, [$($k:literal),*], $action:expr) => {
        shortcut!($id, $name, $group, [$($k),*], $action, None)
    };
    ($id:literal, $name:literal, $group:literal, [$($k:literal),*], $action:expr, $ctx:expr) => {
        Shortcut {
            id: $id,
            name: $name,
            group: $group,
            defaults: &[$($k),*],
            context: $ctx,
            bind: |keys, ctx| KeyBinding::new(keys, $action, ctx),
        }
    };
}

/// ⌘N the new tab page, ⌘T a shell, ⌘L the browser: only ⌘ by default,
/// since in a terminal ⌃L clears the screen and ⌃N / ⌃T are editing keys.
pub static SHORTCUTS: &[Shortcut] = &[
    shortcut!("new-tab", "New tab page", "Tabs", ["cmd-n"], NewTab),
    shortcut!(
        "new-terminal",
        "New terminal",
        "Tabs",
        ["cmd-t"],
        NewTerminal
    ),
    shortcut!("new-chat", "New chat", "Tabs", ["cmd-shift-n"], NewChat),
    shortcut!(
        "new-browser",
        "New browser tab",
        "Tabs",
        ["cmd-shift-t"],
        NewBrowser
    ),
    shortcut!("agents", "Agents", "Tabs", ["cmd-shift-a"], ToggleAgents),
    shortcut!("files", "Files", "Code", ["cmd-shift-e"], OpenFiles),
    shortcut!("changes", "Git changes", "Code", ["cmd-shift-g"], OpenChanges),
    shortcut!("close", "Close pane", "Tabs", ["cmd-w"], ClosePane),
    shortcut!(
        "next-tab",
        "Next tab",
        "Tabs",
        ["cmd-shift-]"],
        NextTab
    ),
    shortcut!("recent-tab", "Last used tab (again for older)", "Tabs", ["ctrl-tab"], RecentTab),
    shortcut!("move-to-tab", "Move pane to a new tab", "Panes", ["alt-cmd-t"], MoveToTab),
    shortcut!("connect-host", "Connect to a host over SSH", "Window", ["shift-cmd-h"], ConnectHost),
    shortcut!("recent-tab-back", "Last used tab, the other way", "Tabs", ["ctrl-shift-tab"], RecentTabBack),
    shortcut!(
        "prev-tab",
        "Previous tab",
        "Tabs",
        ["cmd-shift-["],
        PrevTab
    ),
    shortcut!("split-right", "Split right", "Panes", ["cmd-d"], SplitRight),
    shortcut!(
        "split-down",
        "Split down",
        "Panes",
        ["cmd-shift-d"],
        SplitDown
    ),
    shortcut!("next-pane", "Next pane", "Panes", ["cmd-]"], NextPane),
    shortcut!("prev-pane", "Previous pane", "Panes", ["cmd-["], PrevPane),
    shortcut!("quick-open", "Switch tabs (in the editor: open a file)", "Window", ["cmd-p"], QuickOpen),
    // ⌃P and ⌃K stay the shell's in a terminal (previous command, kill to
    // the end of the line).
    shortcut!("switch-tab", "Switch tabs", "Window", ["ctrl-p"], TabSwitcher, Some("!Terminal")),
    shortcut!("actions", "Actions", "Window", ["ctrl-k"], ActionPalette, Some("!Terminal")),
    shortcut!("focus-left", "Pane to the left", "Panes", ["alt-cmd-left"], FocusLeft),
    shortcut!("focus-right", "Pane to the right", "Panes", ["alt-cmd-right"], FocusRight),
    shortcut!("focus-up", "Pane above", "Panes", ["alt-cmd-up"], FocusUp),
    shortcut!("focus-down", "Pane below", "Panes", ["alt-cmd-down"], FocusDown),
    shortcut!(
        "palette",
        "Command palette",
        "Window",
        ["cmd-k"],
        CommandPalette
    ),
    shortcut!(
        "snippet",
        "Insert snippet",
        "Window",
        ["cmd-;"],
        InsertSnippet
    ),
    shortcut!("settings", "Settings", "Window", ["cmd-,"], OpenSettings),
    shortcut!("usage", "Usage and cost", "Window", ["alt-cmd-u"], OpenUsage),
    shortcut!("next-needs-you", "Next one that needs you", "Tabs", ["alt-cmd-j"], NextNeedsYou),
    shortcut!("zoom-pane", "Zoom the pane to fill its tab", "Panes", ["cmd-shift-enter"], ZoomPane),
    shortcut!("filter-sidebar", "Filter the sidebar", "Window", ["alt-cmd-f"], FilterSidebar),
    // From any app. The context matches nothing inside Deck: these are
    // registered with macOS instead (see global.rs).
    shortcut!("global-toggle", "Show or hide Deck", "System-wide", ["ctrl-alt-space"], ToggleDeck, Some(GLOBAL)),
    shortcut!("global-chat", "New chat, from any app", "System-wide", ["ctrl-alt-n"], NewChat, Some(GLOBAL)),
    shortcut!("global-terminal", "New terminal, from any app", "System-wide", ["ctrl-alt-t"], NewTerminal, Some(GLOBAL)),
    shortcut!("ask-command", "Write a command with AI", "Terminal", ["cmd-j"], AskCommand, Some("Terminal")),
    shortcut!("packages", "Packages", "Window", ["alt-cmd-p"], OpenPackages),
    shortcut!(
        "sidebar",
        "Show or hide the sidebar",
        "Window",
        ["cmd-b"],
        ToggleSidebar
    ),
    shortcut!(
        "sidebar-view",
        "List the sidebar by project or by time",
        "Window",
        ["cmd-shift-b"],
        ToggleSidebarView
    ),
    shortcut!(
        "inspector",
        "Show or hide the inspector",
        "Window",
        ["cmd-i"],
        ToggleInspector
    ),
    shortcut!("zoom-in", "Zoom in", "Window", ["cmd-=", "cmd-+"], ZoomIn),
    shortcut!("zoom-out", "Zoom out", "Window", ["cmd--"], ZoomOut),
    shortcut!("zoom-reset", "Actual size", "Window", ["cmd-0"], ZoomReset),
    shortcut!("lock", "Lock Deck", "Window", ["cmd-shift-l"], LockDeck),
    shortcut!("address", "Address bar", "Browser", ["cmd-l"], FocusAddress),
    shortcut!("reload", "Reload page", "Browser", ["cmd-r"], Reload),
    shortcut!(
        "devtools",
        "Web inspector",
        "Browser",
        ["alt-cmd-i"],
        ToggleDevTools
    ),
    shortcut!(
        "clear",
        "Clear terminal",
        "Terminal",
        ["cmd-shift-k"],
        ClearTerminal,
        Some("Terminal")
    ),
    shortcut!(
        "rewrite",
        "Rewrite the prompt",
        "Chat",
        ["ctrl-o"],
        RewritePrompt,
        Some("Composer")
    ),
    shortcut!(
        "my-words",
        "Bring back my words",
        "Chat",
        ["ctrl-z"],
        MyWords,
        Some("Composer")
    ),
];

impl Shortcut {
    /// The keys it answers to: yours from Settings (empty: none), else
    /// Deck's.
    pub fn keys<'a>(
        &'a self,
        keys: &'a std::collections::BTreeMap<String, String>,
    ) -> Vec<&'a str> {
        match keys.get(self.id) {
            Some(k) if k.is_empty() => Vec::new(),
            Some(k) => vec![k.as_str()],
            None => self.defaults.to_vec(),
        }
    }
}

/// The shortcut for `id` as the UI shows it ("⇧⌘N"), or "" when it has none.
pub fn hint(id: &str, keys: &std::collections::BTreeMap<String, String>) -> SharedString {
    SHORTCUTS
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.keys(keys).first().map(|k| crate::keys::pretty(k)))
        .unwrap_or_default()
        .into()
}

pub fn install(cx: &mut App) {
    rebind(cx, &crate::settings::load().keys);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

/// Put the keymap in place with your shortcuts from Settings, and the menus
/// that show them. At launch, and whenever a shortcut changes.
/// The key context of system-wide shortcuts: no element has it.
const GLOBAL: &str = "SystemWide";

pub fn rebind(cx: &mut App, mine: &std::collections::BTreeMap<String, String>) {
    // System-wide ones go to macOS, as your keys or Deck's.
    let global: Vec<(&'static str, String)> = SHORTCUTS
        .iter()
        .filter(|s| s.context == Some(GLOBAL))
        .flat_map(|s| s.keys(mine).into_iter().map(move |k| (s.id, k.to_string())))
        .collect();
    crate::global::register(&global);
    // Pages hand Deck its own shortcuts (not text editing: ⌘C, ⌘V, ⌘X,
    // ⌘A and ⌘Z stay the page's).
    let page_keys: Vec<String> = SHORTCUTS
        .iter()
        .filter(|s| s.context.is_none())
        .flat_map(|s| s.keys(mine).into_iter().map(str::to_string))
        .filter(|k| !matches!(k.as_str(), "cmd-c" | "cmd-v" | "cmd-x" | "cmd-a" | "cmd-z" | "cmd-shift-z"))
        .collect();
    crate::browser::set_forward_keys(page_keys);
    cx.clear_key_bindings();
    let mut keys = Vec::new();
    for s in SHORTCUTS {
        for k in s.keys(mine) {
            // A key settings.json holds that does not parse is skipped, not
            // a crash.
            if gpui::Keystroke::parse(k).is_ok() {
                keys.push((s.bind)(k, s.context));
            }
        }
    }
    keys.extend([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-f", Find, Some("Terminal")),
        KeyBinding::new("cmd-g", FindNext, Some("Terminal")),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some("Terminal")),
        // Anywhere else, find goes to the focused page.
        KeyBinding::new("cmd-f", Find, None),
        KeyBinding::new("cmd-g", FindNext, None),
        KeyBinding::new("cmd-shift-g", FindPrevious, None),
    ]);
    for ctx in ["Terminal", "Field"] {
        keys.push(KeyBinding::new("cmd-c", Copy, Some(ctx)));
        keys.push(KeyBinding::new("cmd-v", Paste, Some(ctx)));
    }
    keys.push(KeyBinding::new("cmd-x", Cut, Some("Field")));
    keys.push(KeyBinding::new("cmd-a", SelectAll, Some("Field")));
    keys.push(KeyBinding::new("cmd-a", SelectAll, Some("Terminal")));
    keys.push(KeyBinding::new("cmd-z", Undo, Some("Field")));
    keys.push(KeyBinding::new("cmd-shift-z", Redo, Some("Field")));
    keys.extend([
        KeyBinding::new("cmd-1", Tab1, None),
        KeyBinding::new("cmd-2", Tab2, None),
        KeyBinding::new("cmd-3", Tab3, None),
        KeyBinding::new("cmd-4", Tab4, None),
        KeyBinding::new("cmd-5", Tab5, None),
        KeyBinding::new("cmd-6", Tab6, None),
        KeyBinding::new("cmd-7", Tab7, None),
        KeyBinding::new("cmd-8", Tab8, None),
        KeyBinding::new("cmd-9", Tab9, None),
    ]);
    cx.bind_keys(keys);
    // Menus are not decoration here: AppKit routes ⌘ shortcuts through them,
    // so they keep working while a webview holds the keyboard.
    cx.set_menus(vec![
        Menu {
            name: "Deck".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Lock Deck", LockDeck),
                MenuItem::separator(),
                MenuItem::action("Quit Deck", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Tab…", NewTab),
                MenuItem::action("New Terminal Tab", NewTerminal),
                MenuItem::action("New Chat", NewChat),
                MenuItem::action("New Browser Tab", NewBrowser),
                MenuItem::action("New Agent…", ToggleAgents),
                MenuItem::separator(),
                MenuItem::action("Files", OpenFiles),
                MenuItem::action("Git Changes", OpenChanges),
                MenuItem::separator(),
                MenuItem::action("Close", ClosePane),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", Undo, OsAction::Undo),
                MenuItem::os_action("Redo", Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
                MenuItem::separator(),
                MenuItem::action("Find…", Find),
                MenuItem::action("Find Next", FindNext),
                MenuItem::action("Find Previous", FindPrevious),
                MenuItem::separator(),
                MenuItem::action("Insert Snippet…", InsertSnippet),
                MenuItem::action("Save as Snippet", SaveSnippet),
                MenuItem::action("Edit Snippets…", EditSnippets),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Sidebar by Project or Time", ToggleSidebarView),
                MenuItem::action("Toggle Inspector", ToggleInspector),
                MenuItem::separator(),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Open Location…", FocusAddress),
                MenuItem::action("Reload Page", Reload),
                MenuItem::action("Web Inspector", ToggleDevTools),
                MenuItem::separator(),
                MenuItem::action("Command Palette…", CommandPalette),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PrevTab),
                MenuItem::action("Next Pane", NextPane),
                MenuItem::action("Previous Pane", PrevPane),
                // Every global shortcut needs a menu item: a focused web page
                // takes ⌘-keys first and hands back only what the menu claims.
                MenuItem::separator(),
                MenuItem::action("Control", Tab1),
                MenuItem::action("Tab 2", Tab2),
                MenuItem::action("Tab 3", Tab3),
                MenuItem::action("Tab 4", Tab4),
                MenuItem::action("Tab 5", Tab5),
                MenuItem::action("Tab 6", Tab6),
                MenuItem::action("Tab 7", Tab7),
                MenuItem::action("Tab 8", Tab8),
                MenuItem::action("Last Tab", Tab9),
            ],
        },
    ]);
}

/// Coding agents we know how to start. Only the installed ones are offered.
pub struct Agent {
    pub name: &'static str,
    pub cmd: &'static str,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        name: "Claude Code",
        cmd: "claude",
    },
    Agent {
        name: "Codex",
        cmd: "codex",
    },
    Agent {
        name: "Gemini CLI",
        cmd: "gemini",
    },
    Agent {
        name: "OpenCode",
        cmd: "opencode",
    },
    Agent {
        name: "Cursor Agent",
        cmd: "cursor-agent",
    },
    Agent {
        name: "Grok",
        cmd: "grok",
    },
    Agent {
        name: "Amp",
        cmd: "amp",
    },
    Agent {
        name: "Aider",
        cmd: "aider",
    },
];

pub type PaneId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Row,
    Column,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Leaf(PaneId),
    /// Children side by side along the axis, each given its share of the
    /// space by weight (dragging a divider moves weight between two).
    Split(Axis, Vec<Node>, Vec<f32>),
}

impl Node {
    /// Put `new` beside `target`. Joins the parent run when it already flows
    /// the same way, as i3 does, so three splits right are three equal columns
    /// rather than a staircase.
    fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split(
                    axis,
                    vec![Node::Leaf(target), Node::Leaf(new)],
                    vec![1., 1.],
                );
                true
            }
            Node::Leaf(_) => false,
            Node::Split(a, children, sizes) => {
                if *a == axis {
                    if let Some(i) = children.iter().position(|c| *c == Node::Leaf(target)) {
                        // The new pane takes half of the one it splits.
                        let half = sizes[i] / 2.;
                        sizes[i] = half;
                        sizes.insert(i + 1, half);
                        children.insert(i + 1, Node::Leaf(new));
                        return true;
                    }
                }
                children.iter_mut().any(|c| c.split(target, new, axis))
            }
        }
    }

    /// Remove a leaf from a split, collapsing any split left with one child.
    fn remove(&mut self, target: PaneId) -> bool {
        let Node::Split(_, children, sizes) = self else {
            return false;
        };
        let removed = match children.iter().position(|c| *c == Node::Leaf(target)) {
            Some(i) => {
                children.remove(i);
                sizes.remove(i);
                true
            }
            None => children.iter_mut().any(|c| c.remove(target)),
        };
        if children.len() == 1 {
            *self = children.pop().expect("one child");
        }
        removed
    }

    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split(_, children, _) => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split(_, children, _) => children.iter().any(|c| c.contains(id)),
        }
    }

    /// The pane beside `target` along `axis` (forward: right or down).
    /// `None` when `target` is not in this tree, `Some(None)` when it is but
    /// nothing lies that way: up the tree to the nearest split along the
    /// axis where its branch has a neighbour, then that neighbour's edge
    /// facing it.
    fn neighbor(&self, target: PaneId, axis: Axis, forward: bool) -> Option<Option<PaneId>> {
        match self {
            Node::Leaf(id) => (*id == target).then_some(None),
            Node::Split(a, children, _) => {
                for (i, child) in children.iter().enumerate() {
                    let Some(found) = child.neighbor(target, axis, forward) else {
                        continue;
                    };
                    if found.is_some() || *a != axis {
                        return Some(found);
                    }
                    let j = if forward { Some(i + 1).filter(|j| *j < children.len()) } else { i.checked_sub(1) };
                    return Some(j.map(|j| children[j].edge(axis, !forward)));
                }
                None
            }
        }
    }

    /// The leaf at one end of this node along `axis` (`last`: the right or
    /// bottom end); across the other axis, the first.
    fn edge(&self, axis: Axis, last: bool) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split(a, children, _) => {
                let child = if *a == axis && last { children.last() } else { children.first() };
                child.expect("a split has children").edge(axis, last)
            }
        }
    }

    /// The pane a subtree starts with; with its axis, it names a split's
    /// child for as long as the drag on its divider lasts.
    fn first(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split(_, children, _) => children[0].first(),
        }
    }

    /// Set the weights of the two neighbours whose subtrees start with `a`
    /// and `b`, wherever that split is.
    fn resize(&mut self, a: PaneId, b: PaneId, wa: f32, wb: f32) -> bool {
        let Node::Split(_, children, sizes) = self else {
            return false;
        };
        if let Some(i) = (0..children.len().saturating_sub(1))
            .find(|&i| children[i].first() == a && children[i + 1].first() == b)
        {
            sizes[i] = wa;
            sizes[i + 1] = wb;
            return true;
        }
        children.iter_mut().any(|c| c.resize(a, b, wa, wb))
    }

    /// The current weights of those neighbours.
    fn weights(&self, a: PaneId, b: PaneId) -> Option<(f32, f32)> {
        let Node::Split(_, children, sizes) = self else {
            return None;
        };
        (0..children.len().saturating_sub(1))
            .find(|&i| children[i].first() == a && children[i + 1].first() == b)
            .map(|i| (sizes[i], sizes[i + 1]))
            .or_else(|| children.iter().find_map(|c| c.weights(a, b)))
    }

    /// Even out a split's children again (double-click on a divider).
    fn even(&mut self, a: PaneId, b: PaneId) -> bool {
        let Node::Split(_, children, sizes) = self else {
            return false;
        };
        if (0..children.len().saturating_sub(1))
            .any(|i| children[i].first() == a && children[i + 1].first() == b)
        {
            sizes.iter_mut().for_each(|w| *w = 1.);
            return true;
        }
        children.iter_mut().any(|c| c.even(a, b))
    }

    /// Put `new` where `old` is, keeping the layout around it.
    fn replace(&mut self, old: PaneId, new: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == old => {
                *id = new;
                true
            }
            Node::Leaf(_) => false,
            Node::Split(_, children, _) => children.iter_mut().any(|c| c.replace(old, new)),
        }
    }
}

enum PaneKind {
    Terminal(Entity<TerminalPane>),
    Browser(Entity<BrowserPane>),
    Chat(Entity<ChatPane>),
    /// The one Settings tab (⌘,).
    Settings(Entity<SettingsPane>),
    /// A new tab that has not been told what to be yet (⌘N).
    Launcher(Entity<LauncherPane>),
    /// A task's changes, to merge, open as a PR or discard.
    Review(Entity<ReviewPane>),
    /// A page of Deck's own: Usage, Packages.
    Page(gpui::AnyView, PageKind),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageKind {
    Usage,
    Packages,
}

impl PageKind {
    fn title(self) -> &'static str {
        match self {
            PageKind::Usage => "Usage",
            PageKind::Packages => "Packages",
        }
    }
}

struct Pane {
    kind: PaneKind,
    /// Terminals and chats get one: they are where callers run.
    token: Option<String>,
    agent: Option<&'static Agent>,
    _sub: Subscription,
}

struct Tab {
    root: Node,
    focused: PaneId,
    /// The control chat: first in the sidebar, never closed by accident.
    pinned: bool,
    /// Pinned by you to the top of the sidebar, above every project.
    on_top: bool,
}

impl Tab {
    fn leaves(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.root.leaves(&mut v);
        v
    }
}

pub enum Open {
    Terminal {
        cmd: Option<String>,
        cwd: Option<PathBuf>,
    },
    Browser {
        url: String,
    },
    /// A structured chat with an agent (by registry id, e.g. "claude"); `None`
    /// picks the default one.
    Chat {
        agent: Option<String>,
        cwd: Option<PathBuf>,
    },
    /// A chat thread saved by an earlier launch.
    Thread(Box<crate::threads::Saved>),
    Settings,
    Launcher,
    /// The review of the task with this id.
    Review(String),
    /// The code editor on a project (a browser pane showing `code.rs`'s app).
    Editor(crate::code::Editor),
    /// One of Deck's own pages.
    Page(PageKind),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Working,
    Idle,
    Attention,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Chats and agent TUIs: things doing work for you.
    Threads,
    /// Terminals and pages.
    Workspace,
}

/// One project in the sidebar: its tabs, by tab index.
/// A Claude Code conversation: its folder, and its id when known.
type Conversation = (PathBuf, Option<String>);

/// What the usage meter and panel show, gathered off the main thread.
#[derive(Default)]
struct UsageView {
    claude: Option<Result<crate::usage::Limits, String>>,
    codex: Option<crate::usage::Limits>,
    /// The focused Claude Code conversation: its title and its numbers.
    session: Option<(SharedString, crate::usage::SessionUse)>,
    today: Option<crate::usage::DayUse>,
    /// Agents with no plan limits to read (OpenCode, Gemini CLI): their
    /// day so far.
    others: Vec<crate::usage::AgentDay>,
    busy: bool,
    at: Option<Instant>,
}

/// A chat's numbers as its agent reported them, for the usage panel.
struct ChatUse {
    pane: PaneId,
    title: SharedString,
    mark: Icon,
    /// "Claude", "OpenCode".
    agent: SharedString,
    /// The model it runs, as the product names it ("Opus 4.1").
    model: Option<String>,
    usage: agent_core::Usage,
}

struct ProjectGroup {
    /// Its folder's name; empty for the tabs in no project ("Web").
    name: String,
    root: Option<PathBuf>,
    /// Chats and agents, with when each last did something.
    threads: Vec<(usize, i64)>,
    others: Vec<usize>,
}

impl ProjectGroup {
    fn tabs(&self) -> impl Iterator<Item = usize> + '_ {
        self.threads.iter().map(|t| t.0).chain(self.others.iter().copied())
    }
}

pub struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    panes: HashMap<PaneId, Pane>,
    next_id: PaneId,
    cwd: PathBuf,
    installed: Vec<&'static Agent>,
    /// Project groups folded shut in the sidebar, by name.
    folded: std::collections::HashSet<String>,
    sidebar: bool,
    /// The sidebar by time (Now, Today, …) instead of by project.
    recent: bool,
    /// When each pane last did something the sidebar sorts by, in seconds
    /// since the epoch: began or stopped work, wanted you, finished a
    /// command, or was opened. Never mere output, which an idle agent's
    /// status line makes all the time. Wall-clock, so it holds across
    /// sleep and restarts.
    touched: HashMap<PaneId, i64>,
    /// Each tab's place in the sidebar's Recent view, by its first pane:
    /// the time it had when the list first showed it (see `settle`).
    settled: HashMap<PaneId, i64>,
    pub inspector: Inspector,
    refreshing: bool,
    /// Agent runtime for chats; probed in the background at startup.
    chats: Option<Arc<ChatEnv>>,
    /// Set once that probe has finished, whatever it found.
    probed: bool,
    settings: crate::settings::Settings,
    /// Last agent statuses drawn, so the 1s tick only repaints on a change.
    statuses: Vec<(PaneId, Status)>,
    /// Threads that finished while you were looking elsewhere: "Done".
    unseen: std::collections::HashSet<PaneId>,
    ticks: u64,
    /// Set by whoever launched the app (tests, scripts): a token with a
    /// pane's rights, acting as the focused pane.
    automation: Option<String>,
    control: Option<control::Server>,
    /// The right-click menu of a sidebar row, while open.
    menu: Option<crate::menu::Open>,
    /// The sidebar and the inspector, each its own cached view.
    islands: Option<(Entity<Island>, Entity<Island>)>,
    /// Where each terminal and chat is working, refreshed by the tick.
    places: HashMap<PaneId, Place>,
    /// Terminals running a command that is printing right now.
    busy: std::collections::HashSet<PaneId>,
    /// A divider being dragged.
    resizing: Option<Resize>,
    /// The side panels' widths (unzoomed px), dragged at their inner edges.
    sidebar_px: f32,
    inspector_px: f32,
    /// A side panel's edge is being dragged: which one, where the drag
    /// started and the width then.
    side_drag: Option<(bool, f32, f32)>,
    /// Claude Code terminals deck has unstuck from an API wait: how many
    /// times in a row, and when last.
    stalls: HashMap<PaneId, (u32, Instant)>,
    /// When each agent started its current stretch of work, so finishing a
    /// long one can say so.
    working_since: HashMap<PaneId, Instant>,
    /// Where each split cell was last laid out, by the pane it starts with.
    cells: std::rc::Rc<std::cell::RefCell<HashMap<PaneId, gpui::Bounds<gpui::Pixels>>>>,
    /// The last notification each pane posted, so one prompt is one banner.
    alerted: HashMap<PaneId, (Instant, String)>,
    /// What the Dock badge says: how many panes want you.
    badge: usize,
    /// Set once the last session's tabs are back; until then nothing is
    /// saved, so a slow start cannot overwrite the layout with an empty one.
    restored: bool,
    /// The layout as last written, so an unchanged one is not written again.
    saved_session: Option<crate::session::Session>,
    /// What each restored pane was saved as, and when the restore happened:
    /// an agent takes a moment to start, and until it is seen running the
    /// saved state stands in for it, so an early save cannot drop it.
    restored_as: HashMap<PaneId, crate::session::PaneState>,
    restored_at: Option<Instant>,
    /// Every task, open or finished, as kept in tasks.json.
    tasks: Vec<crate::tasks::Task>,
    /// What each open task has changed so far, refreshed by the tick.
    task_changes: HashMap<String, crate::tasks::Changes>,
    /// The command palette while it is open, and the pane that had the
    /// keyboard before it.
    palette: Option<(Entity<Palette>, Option<PaneId>, Subscription)>,
    /// What the open palette offers, so its own key closes it and another
    /// palette key switches it.
    palette_scope: Option<crate::palette::Scope>,
    /// A one-line message at the foot of the window, and when it goes.
    toast: Option<(SharedString, Instant)>,
    /// A card in the corner that asks something (make deck the default
    /// terminal, restart for an update) until it is answered or closed.
    notice: Option<Notice>,
    /// Locked (⇧⌘L): only the lock screen is drawn until you unlock.
    locked: bool,
    /// The browser is open on the sign-in page.
    signing_in: bool,
    /// The last account problem, shown on the sign-in screen.
    account_note: Option<SharedString>,
    /// Pages are hidden for the lock or sign-in screen.
    screen_cover: bool,
    _requests: Option<Task<()>>,
    /// A `refresh_places` scan is out on a background thread.
    placing: bool,
    /// Each terminal's scrollback key (see `session::scrollback_file`), and
    /// the one the next terminal made takes over (a restored pane's).
    scroll_keys: HashMap<PaneId, String>,
    /// Terminals `deck run --name` made, by name.
    named: HashMap<String, PaneId>,
    /// Terminals that run in the background, outside every tab: dev
    /// servers and watchers an agent started with `deck job start`. Each is
    /// a tab in the status bar, and opens in the drawer above it.
    jobs: Vec<PaneId>,
    /// The job whose terminal the drawer shows.
    job_open: Option<PaneId>,
    /// What each job was started with, until its shell reports it.
    job_cmds: HashMap<PaneId, String>,
    /// The background service's jobs, as last asked (every two seconds).
    daemon_jobs: Vec<crate::daemon::JobInfo>,
    /// A list request is out.
    daemon_asking: bool,
    /// Sidebar rows picked with ⌘-click / ⇧-click, by their tab's first
    /// pane, and the row a ⇧-click range starts from.
    picked: std::collections::HashSet<PaneId>,
    pick_anchor: Option<PaneId>,
    /// Terminal panes whose shell lives in the background service: their
    /// session's name.
    sessions: HashMap<PaneId, String>,
    /// Terminal agents whose screen shows they wait on you (a permission
    /// prompt, a choice), as last read.
    blocked: std::collections::HashSet<PaneId>,
    /// Deck's own pages' focus handles, so keys reach the window's keymap.
    page_focus: HashMap<PaneId, gpui::FocusHandle>,
    /// The branch picker, while open.
    branch_picker: Option<BranchPicker>,
    /// "Connect to Host": the folder field and the hosts.
    remote_picker: Option<RemotePicker>,
    /// The pane zoomed to fill its tab, if any.
    zoomed: Option<PaneId>,
    /// The sidebar's filter, while open: rows whose title or project
    /// contain it show.
    filter: Option<(Entity<crate::field::Field>, Subscription)>,
    /// Tabs by when they were last looked at (their first pane), newest
    /// first, and a ⌃Tab walk in progress: where it is and when it moved.
    mru: Vec<PaneId>,
    /// Each terminal agent's last turn: the project's root and the
    /// checkpoint commit made when the turn began.
    turn_marks: HashMap<PaneId, (PathBuf, String)>,
    mru_walk: Option<(usize, Instant)>,
    /// The pointer is on the sidebar's gauge: its card shows.
    gauge_hover: bool,
    /// Whether the window was last told to blur what is behind it.
    glass_on: Option<bool>,
    /// At restore: the service's tab sessions not yet attached.
    restore_sessions: Vec<crate::daemon::JobInfo>,
    /// The next terminal made attaches to this session (pid of its shell)
    /// with this token, instead of starting a shell: a restore.
    reattach: Option<(String, u32, String)>,
    /// `deck://job/NAME` arrived before its pane: show it once attached.
    show_when_attached: Option<String>,
    /// The status bar's chats list, and whether the pointer is on the item
    /// or the list.
    bar_chats_open: bool,
    bar_chats_over_item: bool,
    bar_chats_over_list: bool,
    /// The same for the status bar's "+N" list of background jobs and pages.
    bar_more_open: bool,
    bar_more_over_item: bool,
    bar_more_over_list: bool,
    /// Dev servers under our terminals, for the status bar: (port, process).
    bar_ports: Vec<(u16, String)>,
    /// Whether each listening (port, process) answers HTTP: only web
    /// servers belong in the status bar. `None` while being asked.
    port_web: HashMap<(u16, String), Option<bool>>,
    /// The focused pane's project and its git branch, for the status bar.
    bar_git: Option<(String, Option<String>)>,
    /// Claude Code conversations' first asks by session id, read once: a
    /// title of their own when Claude Code titles several terminals alike.
    conv_titles: HashMap<String, Option<crate::session::ConvTitles>>,
    /// AI usage for the status bar's meter, the sidebar's Usage row and
    /// the panel either opens.
    usage: UsageView,
    usage_open: bool,
    /// The usage panel's tab (Limits, This chat, Chats, Today), the one
    /// before it (the indicator slides from there) and a count of switches
    /// (so each switch animates afresh).
    usage_tab: usize,
    usage_tab_prev: usize,
    usage_tab_moves: u64,
    /// The panel was opened from the sidebar's Usage row, so it hangs over
    /// the sidebar instead of over the status bar's meter.
    usage_by_sidebar: bool,
    pending_scroll: Option<String>,
    _tick: Task<()>,
    _animate: Task<()>,
}

impl Workspace {
    pub fn new(cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tick = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this
                .update_in(cx, |ws, window, cx| ws.tick(window, cx))
                .is_err()
            {
                return;
            }
        });
        // At quit, each shell's scrollback is kept for the next launch.
        cx.on_app_quit(|ws, cx| {
            ws.save_scrollback(cx);
            // A downloaded update goes in now, for the next launch.
            if ws.settings.install_updates {
                if let crate::updates::State::Ready(_, path) = crate::updates::state() {
                    if let Err(e) = crate::updates::install_on_quit(&path, false) {
                        tracing::warn!("could not install the update: {e}");
                    }
                }
            }
            async {}
        })
        .detach();
        // Spinners and the like step on their own clock (icon::phase): after
        // a frame that drew one, draw the next a fifteenth of a second later.
        // Nothing animating, nothing drawn.
        let animate = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs_f32(1. / crate::icon::ANIM_FPS))
                .await;
            if crate::icon::take_animating()
                && this.update_in(cx, |_, window, _| window.refresh()).is_err()
            {
                return;
            }
        });
        let mut ws = Self {
            tabs: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 1,
            cwd,
            installed: AGENTS
                .iter()
                .filter(|a| term::which(a.cmd).is_some())
                .collect(),
            folded: std::collections::HashSet::new(),
            sidebar: true,
            recent: false,
            touched: HashMap::new(),
            settled: HashMap::new(),
            inspector: Inspector {
                // A narrow (tiled) window cannot afford a third column.
                open: f32::from(window.viewport_size().width) >= 1200.,
                ..Default::default()
            },
            refreshing: false,
            chats: None,
            probed: false,
            settings: {
                let s = crate::settings::load();
                crate::browser::set_search_engine(s.search);
                crate::refine::set(s.refine, &s.refine_model);
                crate::retry::set_enabled(s.auto_retry);
                wear(s.theme, window);
                s
            },
            statuses: Vec::new(),
            unseen: Default::default(),
            islands: None,
            places: HashMap::new(),
            busy: Default::default(),
            resizing: None,
            sidebar_px: theme::SIDEBAR_PX,
            inspector_px: theme::INSPECTOR_PX,
            side_drag: None,
            working_since: HashMap::new(),
            stalls: HashMap::new(),
            cells: Default::default(),
            alerted: HashMap::new(),
            badge: 0,
            restored: false,
            saved_session: None,
            restored_as: HashMap::new(),
            tasks: crate::tasks::load_all(),
            task_changes: HashMap::new(),
            palette: None,
            palette_scope: None,
            toast: None,
            restored_at: None,
            ticks: 0,
            automation: std::env::var("DECK_AUTOMATION_TOKEN")
                .ok()
                .filter(|t| t.len() >= 16),
            control: None,
            menu: None,
            notice: None,
            locked: false,
            signing_in: false,
            account_note: None,
            screen_cover: false,
            _requests: None,
            placing: false,
            scroll_keys: HashMap::new(),
            named: HashMap::new(),
            jobs: Vec::new(),
            job_open: None,
            job_cmds: HashMap::new(),
            daemon_jobs: Vec::new(),
            daemon_asking: false,
            show_when_attached: None,
            sessions: HashMap::new(),
            picked: Default::default(),
            pick_anchor: None,
            restore_sessions: Vec::new(),
            glass_on: None,
            gauge_hover: false,
            mru: Vec::new(),
            turn_marks: HashMap::new(),
            mru_walk: None,
            zoomed: None,
            filter: None,
            branch_picker: None,
            remote_picker: None,
            page_focus: HashMap::new(),
            blocked: Default::default(),
            reattach: None,
            bar_chats_open: false,
            bar_chats_over_item: false,
            bar_chats_over_list: false,
            bar_more_open: false,
            bar_more_over_item: false,
            bar_more_over_list: false,
            bar_ports: Vec::new(),
            port_web: HashMap::new(),
            bar_git: None,
            conv_titles: HashMap::new(),
            usage: UsageView::default(),
            usage_open: false,
            usage_tab: 0,
            usage_tab_prev: 0,
            usage_tab_moves: 0,
            usage_by_sidebar: false,
            pending_scroll: None,
            _tick: tick,
            _animate: animate,
        };
        // Scripts and links macOS opened deck with, now and later.
        if let Some(mut opened) = crate::handler::receiver() {
            cx.spawn_in(window, async move |this, cx| {
                use futures::StreamExt;
                while let Some(url) = opened.next().await {
                    let ok = this.update_in(cx, |ws, window, cx| ws.open_handled(&url, window, cx));
                    if ok.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        // Probing every agent CLI takes a moment; the window does not wait.
        // When it is ready, the control chat opens as the first tab.
        cx.spawn_in(window, async move |this, cx| {
            let (env, saved) = cx
                .background_executor()
                .spawn(async {
                    let loaded = (ChatEnv::start(), crate::threads::load_all());
                    crate::threads::prune_attachments();
                    loaded
                })
                .await;
            let _ = this.update_in(cx, |ws, window, cx| {
                match env {
                    Ok(env) => ws.chats = Some(Arc::new(env)),
                    Err(e) => tracing::error!("agent runtime unavailable: {e}"),
                }
                ws.probed = true;
                let agents = ws.ready_agents();
                let settings = ws.settings.clone();
                for pane in ws.panes.values() {
                    if let PaneKind::Settings(s) = &pane.kind {
                        let agents = agents.clone();
                        s.update(cx, |s, cx| s.sync(&settings, Some(agents), cx));
                    }
                }
                ws.restore(saved, window, cx);
                ws.offer_default_terminal(cx);
                ws.offer_permissions(cx);
                ws.start_account(cx);
                ws.start_update_checks(cx);
            });
        })
        .detach();
        // The System theme follows macOS from light to dark and back.
        cx.observe_window_appearance(window, |ws, window, cx| {
            if ws.settings.theme == crate::theme::Theme::System {
                wear(ws.settings.theme, window);
                ws.restyle(window, cx);
                for pane in ws.panes.values() {
                    if let PaneKind::Browser(b) = &pane.kind {
                        b.read(cx).editor_restyle();
                    }
                }
            }
        })
        .detach();
        // Deck's shortcuts pressed while a page had the keyboard.
        {
            let mut keys = crate::browser::watch_page_keys();
            cx.spawn_in(window, async move |_, cx| {
                while let Some(k) = keys.next().await {
                    let _ = cx.update(|window, cx| {
                        if let Ok(ks) = gpui::Keystroke::parse(&k) {
                            window.dispatch_keystroke(ks, cx);
                        }
                    });
                }
            })
            .detach();
        }
        // System-wide shortcuts, pressed in any app.
        {
            let mut pressed = crate::global::start();
            cx.spawn_in(window, async move |this, cx| {
                while let Some(id) = pressed.next().await {
                    let _ = this.update_in(cx, |_, window, cx| {
                        let front = window.is_window_active();
                        match id {
                            "global-toggle" if front => cx.hide(),
                            "global-toggle" => {
                                cx.activate(true);
                                window.activate_window();
                            }
                            other => {
                                cx.activate(true);
                                window.activate_window();
                                match other {
                                    "global-chat" => window.dispatch_action(Box::new(NewChat), cx),
                                    "global-terminal" => window.dispatch_action(Box::new(NewTerminal), cx),
                                    _ => {}
                                }
                            }
                        }
                    });
                }
            })
            .detach();
        }
        // A click on one of deck's notifications brings its pane forward.
        if let Some(mut clicks) = crate::notify::start() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(ev) = clicks.next().await {
                    let _ = this.update_in(cx, |ws, window, cx| ws.notification_action(ev, window, cx));
                }
            })
            .detach();
        }
        match control::listen() {
            Ok((server, mut rx)) => {
                tracing::info!(socket = %server.path.display(), "control plane listening");
                ws.control = Some(server);
                ws._requests = Some(cx.spawn_in(window, async move |this, cx| {
                    while let Some((req, reply)) = rx.next().await {
                        let res =
                            this.update_in(cx, |ws, window, cx| ws.handle(req, reply, window, cx));
                        if res.is_err() {
                            return;
                        }
                    }
                }));
            }
            Err(e) => tracing::error!("control plane unavailable: {e}"),
        }
        ws
    }

    // -- panes and tabs -----------------------------------------------------

    /// Create a pane (not yet placed in any tab).
    fn make_pane(
        &mut self,
        what: Open,
        agent: Option<&'static Agent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let id = self.next_id;
        // Where a terminal starts, so its row is in its project from the
        // first frame rather than moving there after the first scan.
        let mut seed: Option<PathBuf> = None;
        // The fallible half (PTY, webview) happens before `cx.new`, which
        // cannot return an error.
        let (kind, token, sub) = match what {
            Open::Terminal { cmd, cwd } => {
                let reattach = self.reattach.take();
                let token = reattach.as_ref().map_or_else(new_token, |r| r.2.clone());
                let mut env = self.pane_env(id, &token);
                // zsh marks its prompts and commands, for blocks.
                env.extend(crate::shell::env());
                // A restored shell prints what this pane showed before the
                // quit, then starts fresh beneath it.
                let key = self.pending_scroll.take().unwrap_or_else(|| new_token()[..16].to_string());
                let saved = crate::session::scrollback_file(&key);
                if cmd.is_none() && saved.exists() && env.contains_key("ZDOTDIR") && reattach.is_none() {
                    env.insert("DECK_RESTORE".into(), saved.display().to_string());
                }
                self.scroll_keys.insert(id, key);
                let cwd = cwd.unwrap_or_else(|| self.cwd.clone());
                seed = Some(cwd.clone());
                // In the background service when it can be: the shell (and
                // any agent in it) outlives Deck, and this pane attaches.
                let attach_cmd = std::env::current_exe().ok().map(|exe| {
                    move |name: &str| {
                        let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
                        format!("exec {} job attach {}", q(&exe.to_string_lossy()), q(name))
                    }
                });
                let mut session: Option<(String, u32)> = None;
                if let Some((name, pid, _)) = reattach {
                    session = Some((name, pid));
                } else if self.settings.keep_terminals && attach_cmd.is_some() && crate::daemon::ask(&json!({ "op": "ping" })).is_ok() {
                    let name = format!("tab-{}", &token[..12.min(token.len())]);
                    let mut job_env = crate::daemon::job_env();
                    job_env.extend(env.clone());
                    // The shell finds whichever Deck is running now.
                    job_env.insert("DECK_SOCKET".into(), control::stable_socket().display().to_string());
                    // The `-c` shell that runs this reads Deck's .zshenv,
                    // which hands ZDOTDIR back to the user's own: point it
                    // at Deck's integration again for the login shell, or
                    // it starts without suggestions, blocks or folder
                    // tracking.
                    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
                    let mut again = String::new();
                    if let Some(z) = env.get("ZDOTDIR") {
                        again.push_str(&format!("export ZDOTDIR={}; ", q(z)));
                        match env.get("DECK_ZDOTDIR") {
                            Some(own) => again.push_str(&format!("export DECK_ZDOTDIR={}; ", q(own))),
                            None => again.push_str("unset DECK_ZDOTDIR; "),
                        }
                    }
                    let line = match &cmd {
                        Some(c) => format!("{c}; {again}exec \"$SHELL\" -l"),
                        None => format!("{again}exec \"$SHELL\" -l"),
                    };
                    match crate::daemon::start_tab(&name, &line, &cwd, job_env) {
                        Ok(job) => session = Some((name, job.pid as u32)),
                        Err(e) => tracing::warn!("terminal stays in Deck: {e}"),
                    }
                }
                let engine = self.settings.engine.term();
                let pty = match (&session, &attach_cmd) {
                    (Some((name, _)), Some(attach)) => TerminalPane::spawn(&cwd, Some(&attach(name)), HashMap::new(), engine)?,
                    _ => TerminalPane::spawn(&cwd, cmd.as_deref(), env, engine)?,
                };
                let title = agent
                    .map(|a| a.name.to_string())
                    .or(cmd)
                    .or_else(|| {
                        std::env::var("SHELL")
                            .ok()
                            .map(|s| s.rsplit('/').next().unwrap_or("shell").to_string())
                    })
                    .unwrap_or_else(|| "shell".into());
                let look = self.settings.term_look();
                crate::code::set_font(&look.family, self.settings.font_px);
                let t = cx.new(|cx| TerminalPane::new(pty, title, look, window, cx));
                if let Some((name, pid)) = session {
                    t.update(cx, |t, _| t.set_pid(pid));
                    self.sessions.insert(id, name);
                }
                let sub =
                    cx.subscribe_in(&t, window, move |ws, _, ev: &TerminalEvent, window, cx| {
                        match ev {
                            TerminalEvent::Focused => ws.mark_focused(id, cx),
                            // The sidebar draws only when the workspace
                            // notifies, so a new title has to say so.
                            TerminalEvent::Retitled => cx.notify(),
                            TerminalEvent::Alert(said) => {
                                ws.touched.insert(id, crate::local::now());
                                let said = said.clone();
                                ws.alert(id, said, window, cx);
                            }
                            TerminalEvent::Exited => ws.close_pane(id, window, cx),
                            TerminalEvent::OpenUrl(url) => {
                                let url = url.clone();
                                ws.open_url_near(id, &url, window, cx);
                            }
                            TerminalEvent::CommandDone { command, exit, took } => {
                                ws.touched.insert(id, crate::local::now());
                                ws.command_done(id, command.clone(), *exit, *took, window, cx);
                            }
                            TerminalEvent::History => ws.open_history(id, window, cx),
                            TerminalEvent::Ask { request, cwd } => {
                                let (request, cwd) = (request.clone(), cwd.clone());
                                ws.write_command(id, request, cwd, cx);
                            }
                            TerminalEvent::AskAgent { prompt, cwd } => {
                                let (prompt, cwd) = (prompt.clone(), cwd.clone());
                                ws.ask_agent_beside(id, prompt, cwd, window, cx);
                            }
                        }
                    });
                (PaneKind::Terminal(t), Some(token), sub)
            }
            Open::Chat { .. } | Open::Thread(_) => {
                let env = self.chats.clone().ok_or_else(|| {
                    anyhow::anyhow!("agents are still being detected; try again in a moment")
                })?;
                let token = new_token();
                let agent_env: Vec<(String, String)> =
                    self.pane_env(id, &token).into_iter().collect();
                let c = match what {
                    Open::Thread(saved) => {
                        cx.new(|cx| ChatPane::restore(env, *saved, agent_env, cx))
                    }
                    Open::Chat { agent, cwd } => {
                        let agent = match agent {
                            Some(a) => agent_core::AgentId::new(a),
                            None => self.default_agent().ok_or_else(|| {
                                anyhow::anyhow!("no agent CLI is installed and signed in")
                            })?,
                        };
                        let cwd = cwd.unwrap_or_else(|| self.cwd.clone());
                        let c = cx.new(|cx| ChatPane::new(env, agent, cwd, agent_env, cx));
                        let mode = self.settings.access.mode();
                        c.update(cx, |c, _| c.set_mode(mode));
                        c
                    }
                    _ => unreachable!(),
                };
                let sub =
                    cx.subscribe_in(
                        &c,
                        window,
                        move |ws, _, ev: &ChatEvent, window, cx| match ev {
                            ChatEvent::Focused => ws.mark_focused(id, cx),
                            ChatEvent::StartTask { brief, agent, cwd } => {
                                let (brief, agent, cwd) = (brief.clone(), agent.clone(), cwd.clone());
                                ws.spawn_task(brief, cwd, Some(agent), None, None, window, cx);
                                // The empty thread made way for the task's.
                                ws.close_pane(id, window, cx);
                            }
                            ChatEvent::Moved => {
                                ws.refresh_places(cx);
                                ws.save_session(cx);
                            }
                            ChatEvent::OpenUrl(url) => {
                                let url = url.clone();
                                ws.open_url_near(id, &url, window, cx);
                            }
                            ChatEvent::OpenFile(path) => {
                                let root = crate::inspect::project_root(
                                    path.parent().unwrap_or(std::path::Path::new("/")),
                                );
                                let rel = path
                                    .strip_prefix(&root)
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_else(|_| path.to_string_lossy().into_owned());
                                let file = Some((rel, None, true));
                                if let Err(e) = ws.open_editor("changes", Some(root), file, window, cx) {
                                    tracing::warn!("could not open the editor: {e}");
                                }
                            }
                        },
                    );
                (PaneKind::Chat(c), Some(token), sub)
            }
            Open::Browser { .. } | Open::Editor(_) => {
                let (url, editor) = match what {
                    Open::Editor(e) => (String::new(), Some(e)),
                    Open::Browser { url } => (url, None),
                    _ => unreachable!(),
                };
                let (webview, events, url, code) = BrowserPane::build(&url, editor, window)?;
                let b = cx.new(|cx| BrowserPane::new(webview, events, url, code, window, cx));
                let zoom = self.settings.zoom as f64;
                let offer = (!self.settings.import_offered)
                    .then(|| crate::browser_import::sources().first().map(|s| SharedString::from(s.name)))
                    .flatten();
                b.update(cx, |b, _| {
                    b.set_zoom(zoom);
                    b.import_offer = offer;
                });
                let sub =
                    cx.subscribe_in(&b, window, move |ws, _, ev: &BrowserEvent, window, cx| {
                        match ev {
                            BrowserEvent::Focused => {
                                // The page has the keyboard now; drop gpui's focus so
                                // no field or terminal still looks focused.
                                window.blur();
                                ws.mark_focused(id, cx);
                            }
                            BrowserEvent::OpenTab(url) => {
                                let url = url.clone();
                                let _ = ws.new_tab(Open::Browser { url }, None, window, cx);
                            }
                            BrowserEvent::Retitled => cx.notify(),
                            BrowserEvent::Close => ws.close_pane(id, window, cx),
                            BrowserEvent::DismissImport => ws.answer_import_offer(window, cx),
                            BrowserEvent::Key(k) => {
                                // As if pressed with Deck focused: the
                                // window's keymap runs it.
                                // After this update, not inside it: the
                                // keystroke redraws the window, and the
                                // workspace is still leased here (⌘W on a
                                // page crashed Deck with a double lease).
                                if let Ok(ks) = gpui::Keystroke::parse(k) {
                                    window.defer(cx, move |window, cx| {
                                        window.dispatch_keystroke(ks, cx);
                                    });
                                }
                            }
                            BrowserEvent::Import => ws.import_from_browser(window, cx),
                            BrowserEvent::OpenFolder { path, terminal } => {
                                let path = path.clone();
                                let opened = if *terminal {
                                    ws.new_tab(Open::Terminal { cmd: None, cwd: Some(path) }, None, window, cx)
                                } else {
                                    ws.open_editor("files", Some(path), None, window, cx)
                                };
                                if let Err(e) = opened {
                                    tracing::warn!("could not open the folder: {e}");
                                }
                            }
                        }
                    });
                (PaneKind::Browser(b), None, sub)
            }
            Open::Settings => self.make_settings(window, cx),
            Open::Launcher => self.make_launcher(id, window, cx),
            Open::Page(kind) => {
                let mut return_sub = None;
                let view: gpui::AnyView = match kind {
                    PageKind::Usage => {
                        let page = cx.new(crate::usage_page::UsagePage::new);
                        self.page_focus.insert(id, page.read(cx).focus.clone());
                        page.into()
                    }
                    PageKind::Packages => {
                        let cwd = self.focused_cwd(cx).unwrap_or_else(|| self.cwd.clone());
                        let page = cx.new(|cx| crate::packages::PackagesPage::new(cwd, cx));
                        self.page_focus.insert(id, page.read(cx).focus.clone());
                        let sub = cx.subscribe_in(&page, window, move |ws, _, ev: &crate::packages::PackagesEvent, window, cx| match ev {
                            crate::packages::PackagesEvent::Run { name, cmd, cwd } => {
                                match ws.start_job(name.clone(), cmd.clone(), Some(cwd.clone()), window, cx) {
                                    Ok(job) => ws.show_job(job, window, cx),
                                    Err(e) => ws.flash(format!("Could not run {cmd}: {e}"), cx),
                                }
                            }
                            crate::packages::PackagesEvent::OpenUrl(url) => {
                                let url = url.clone();
                                ws.open_url_near(id, &url, window, cx);
                            }
                        });
                        return_sub = Some(sub);
                        page.into()
                    }
                };
                (PaneKind::Page(view, kind), None, return_sub.unwrap_or_else(|| Subscription::new(|| {})))
            }
            Open::Review(task) => self.make_review(id, &task, window, cx)?,
        };
        self.next_id += 1;
        if let PaneKind::Chat(c) = &kind {
            seed = Some(c.read(cx).cwd().to_path_buf());
        }
        if let Some(dir) = seed {
            self.places.entry(id).or_insert_with(|| place_of(&dir));
        }
        self.panes.insert(
            id,
            Pane {
                kind,
                token,
                agent,
                _sub: sub,
            },
        );
        Ok(id)
    }

    /// Put the control chat first. Without any agent there is nothing to talk
    /// to, so the window opens on a terminal instead of an empty screen.
    /// Open the pinned control chat: the one saved last time if there is
    /// one, else a new one. Other saved threads come back as tabs after it.
    fn open_control(
        &mut self,
        mut saved: Vec<crate::threads::Saved>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A restored layout may already hold the control chat; then the
        // other threads still come back as tabs of their own.
        let pinned = self.tabs.iter().any(|t| t.pinned);
        let control = saved
            .iter()
            .rposition(|s| s.control && !pinned)
            .map(|i| saved.remove(i));
        let agent = self.default_agent();
        let what = match control {
            Some(s) => Some(Open::Thread(Box::new(s))),
            None if pinned => None,
            None => agent.map(|a| Open::Chat {
                agent: Some(a.as_str().into()),
                cwd: None,
            }),
        };
        let made = what.and_then(|what| {
            self.make_pane(what, None, window, cx)
                .map_err(|e| tracing::error!("control chat: {e}"))
                .ok()
        });
        match made {
            Some(id) => {
                if let Some(Pane {
                    kind: PaneKind::Chat(c),
                    ..
                }) = self.panes.get(&id)
                {
                    c.update(cx, |c, _| c.control = true);
                }
                let fresh = self.tabs.is_empty();
                self.tabs.insert(
                    0,
                    Tab {
                        root: Node::Leaf(id),
                        focused: id,
                        pinned: true,
                        on_top: false,
                    },
                );
                // Don't yank focus from something the user already opened.
                if fresh {
                    self.focus_pane(id, window, cx);
                } else {
                    self.active += 1;
                }
            }
            None if self.tabs.is_empty() && !pinned => {
                if let Err(e) = self.new_tab(
                    Open::Terminal {
                        cmd: None,
                        cwd: None,
                    },
                    None,
                    window,
                    cx,
                ) {
                    tracing::error!("could not start a shell: {e}");
                }
            }
            None => {}
        }
        let cutoff = (crate::local::now() - RESTORE_SECS) * 1000;
        for mut s in saved {
            if s.updated < cutoff {
                continue;
            }
            // Only one control chat; an older one is an ordinary thread now.
            s.control = false;
            match self.make_pane(Open::Thread(Box::new(s)), None, window, cx) {
                Ok(id) => self.tabs.push(Tab {
                    root: Node::Leaf(id),
                    focused: id,
                    pinned: false,
                    on_top: false,
                }),
                Err(e) => tracing::warn!("could not restore a thread: {e}"),
            }
        }
        cx.notify();
    }

    /// Bring back the last session's tabs, splits and focus, then the
    /// threads it did not mention (and the control chat, if it had none).
    fn restore(
        &mut self,
        saved: Vec<crate::threads::Saved>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::session;
        // Tab sessions the service kept running while Deck was closed.
        self.restore_sessions = crate::daemon::list().unwrap_or_default().into_iter().filter(|j| j.tab).collect();
        let Some(last) = session::load() else {
            self.open_control(saved, window, cx);
            self.adopt_sessions(window, cx);
            self.restored = true;
            return;
        };
        let mut threads: HashMap<String, crate::threads::Saved> =
            saved.into_iter().map(|s| (s.id.clone(), s)).collect();
        let mut back: Vec<Tab> = Vec::new();
        // The tab to land on, by its focused pane: indices move as the
        // control chat is put first.
        let mut active = None;
        let now = crate::local::now();
        for (ix, tab) in last.tabs.iter().enumerate() {
            // Only the last few days come back; pinned rows always do.
            // Older threads stay on disk, out of the sidebar.
            if !tab.pinned && !tab.on_top && tab.touched.is_some_and(|at| now - at > RESTORE_SECS) {
                continue;
            }
            let mut leaves = Vec::new();
            let Some(root) = self.rebuild(&tab.root, &mut threads, &mut leaves, window, cx) else {
                continue;
            };
            // The control chat is the pinned tab only if its thread is.
            let pinned = tab.pinned
                && leaves
                    .iter()
                    .any(|id| match self.panes.get(id).map(|p| &p.kind) {
                        Some(PaneKind::Chat(c)) => c.read(cx).control,
                        _ => false,
                    });
            let focused = leaves
                .get(tab.focused)
                .or(leaves.first())
                .copied()
                .unwrap_or(0);
            if let Some(at) = tab.touched {
                for &id in &leaves {
                    self.touched.insert(id, at);
                }
            }
            if ix <= last.active {
                active = Some(focused);
            }
            back.push(Tab {
                focused,
                root,
                pinned,
                on_top: tab.on_top,
            });
        }
        // Tabs opened before the restore finished (`deck open`) stay, after.
        let opened = std::mem::take(&mut self.tabs);
        let had_any = !back.is_empty();
        self.tabs = back;
        self.tabs.extend(opened);
        self.sidebar = last.sidebar;
        self.recent = last.recent;
        self.inspector.open = last.inspector;
        self.sidebar_px = last.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.inspector_px = last.inspector_width.clamp(INSPECTOR_MIN, INSPECTOR_MAX);
        self.adopt_sessions(window, cx);
        let mut rest: Vec<_> = threads.into_values().collect();
        rest.sort_by_key(|s| s.updated);
        self.open_control(rest, window, cx);
        // The pinned control chat goes first, wherever it was saved.
        if let Some(i) = self.tabs.iter().position(|t| t.pinned) {
            if i != 0 {
                let t = self.tabs.remove(i);
                self.tabs.insert(0, t);
            }
        }
        if let Some(ix) = active.filter(|_| had_any).and_then(|id| self.tab_of(id)) {
            self.activate(ix, window, cx);
        }
        self.restored = true;
        self.restored_at = Some(Instant::now());
        cx.notify();
    }

    /// Sessions running in the service that no saved tab named (Deck
    /// crashed before saving): each comes back as a tab of its own.
    fn adopt_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for j in std::mem::take(&mut self.restore_sessions).into_iter().filter(|j| j.running) {
            self.reattach = Some((j.name.clone(), j.pid as u32, new_token()));
            match self.make_pane(Open::Terminal { cmd: None, cwd: Some(j.cwd.clone()) }, None, window, cx) {
                Ok(id) => self.tabs.push(Tab {
                    root: Node::Leaf(id),
                    focused: id,
                    pinned: false,
                    on_top: false,
                }),
                Err(e) => tracing::warn!("could not attach {}: {e}", j.name),
            }
        }
    }

    /// One saved node back as panes; a pane that cannot come back (its
    /// thread was archived, a page with no address) is left out, and a split
    /// left with one child becomes that child.
    fn rebuild(
        &mut self,
        node: &crate::session::NodeState,
        threads: &mut HashMap<String, crate::threads::Saved>,
        leaves: &mut Vec<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Node> {
        use crate::session::{NodeState, PaneState};
        match node {
            NodeState::Split {
                row,
                children,
                sizes,
            } => {
                let mut kids: Vec<Node> = Vec::new();
                let mut weights: Vec<f32> = Vec::new();
                for (i, c) in children.iter().enumerate() {
                    if let Some(k) = self.rebuild(c, threads, leaves, window, cx) {
                        kids.push(k);
                        weights.push(
                            sizes
                                .get(i)
                                .copied()
                                .filter(|w| w.is_finite() && *w > 0.)
                                .unwrap_or(1.),
                        );
                    }
                }
                match kids.len() {
                    0 => None,
                    1 => kids.pop(),
                    _ => Some(Node::Split(
                        if *row { Axis::Row } else { Axis::Column },
                        kids,
                        weights,
                    )),
                }
            }
            NodeState::Pane(p) => {
                let (what, agent) = match p {
                    PaneState::Terminal { cwd, agent, scrollback, session, token } => {
                        self.pending_scroll = scrollback.clone();
                        let cwd = cwd.clone().filter(|c| c.is_dir());
                        // Still running in the service: attach, nothing
                        // is started or resumed.
                        let live = session.as_ref().and_then(|name| {
                            self.restore_sessions.iter().find(|j| &j.name == name && j.running).map(|j| (name.clone(), j.pid as u32))
                        });
                        if let Some((name, pid)) = live {
                            self.reattach = Some((name.clone(), pid, token.clone().unwrap_or_else(new_token)));
                            self.restore_sessions.retain(|j| j.name != name);
                            let id = self
                                .make_pane(Open::Terminal { cmd: None, cwd }, None, window, cx)
                                .map_err(|e| tracing::warn!("could not attach a session: {e}"))
                                .ok()?;
                            self.restored_as.insert(id, p.clone());
                            leaves.push(id);
                            return Some(Node::Leaf(id));
                        }
                        let tab = agent
                            .as_ref()
                            .filter(|a| a.tab)
                            .and_then(|a| AGENTS.iter().find(|x| x.cmd == a.cmd));
                        (
                            Open::Terminal {
                                cmd: agent.as_ref().map(crate::session::resume_line),
                                cwd,
                            },
                            tab,
                        )
                    }
                    PaneState::Browser { url } => (Open::Browser { url: url.clone() }, None),
                    PaneState::Chat { thread } => {
                        let saved = threads.remove(thread)?;
                        (Open::Thread(Box::new(saved)), None)
                    }
                    PaneState::Settings => (Open::Settings, None),
                };
                let control = matches!(&what, Open::Thread(s) if s.control);
                let id = self
                    .make_pane(what, agent, window, cx)
                    .map_err(|e| tracing::warn!("could not restore a pane: {e}"))
                    .ok()?;
                self.restored_as.insert(id, p.clone());
                if control {
                    if let Some(PaneKind::Chat(c)) = self.panes.get(&id).map(|p| &p.kind) {
                        c.update(cx, |c, _| c.control = true);
                    }
                }
                leaves.push(id);
                Some(Node::Leaf(id))
            }
        }
    }

    /// The layout as it stands, in the form it is saved.
    fn snapshot(&self, cx: &App) -> crate::session::Session {
        use crate::session::{AgentRun, NodeState, PaneState, Session, TabState};
        // Several Claude Code tabs in one folder: the one that printed last
        // is the conversation written last.
        let mut claude: HashMap<PathBuf, Vec<(Instant, PaneId)>> = HashMap::new();
        let mut conversation: HashMap<PaneId, String> = HashMap::new();
        for (&id, pane) in &self.panes {
            if let (PaneKind::Terminal(t), Some(place)) = (&pane.kind, self.places.get(&id)) {
                if let Some(s) = &place.session {
                    conversation.insert(id, s.clone());
                } else if matches!(place.agent, Some(("claude", _))) {
                    claude
                        .entry(place.cwd.clone())
                        .or_default()
                        .push((t.read(cx).last_output, id));
                }
            }
        }
        // deck's own chats run Claude Code too, and write their
        // conversations to the same folder; those are not a terminal's.
        let chats: std::collections::HashSet<String> = self
            .panes
            .values()
            .filter_map(|p| match &p.kind {
                PaneKind::Chat(c) => c.read(cx).native_id().map(str::to_string),
                _ => None,
            })
            .collect();
        let known: std::collections::HashSet<String> = conversation.values().cloned().collect();
        for (cwd, mut panes) in claude {
            panes.sort_by_key(|p| std::cmp::Reverse(p.0));
            let sessions = crate::session::claude_sessions(&cwd)
                .into_iter()
                .filter(|s| !chats.contains(s) && !known.contains(s));
            for ((_, id), session) in panes.into_iter().zip(sessions) {
                conversation.insert(id, session);
            }
        }

        // Right after a restore, a terminal not yet seen with its agent
        // running is still what it was saved as.
        let settling = self
            .restored_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(20));
        let pane_state = |id: PaneId| -> Option<PaneState> {
            let pane = self.panes.get(&id)?;
            Some(match &pane.kind {
                PaneKind::Terminal(_) => {
                    let place = self.places.get(&id);
                    let saved = self.restored_as.get(&id);
                    let waiting = matches!(saved, Some(PaneState::Terminal { agent: Some(_), .. }))
                        && settling
                        && place.is_none_or(|p| p.agent.is_none());
                    if let (true, Some(saved)) = (place.is_none() || waiting, saved) {
                        return Some(saved.clone());
                    }
                    let agent = place
                        .and_then(|p| p.agent.as_ref())
                        .map(|(cmd, argv)| AgentRun {
                            cmd: cmd.to_string(),
                            flags: crate::session::keep_flags(cmd, argv),
                            session: conversation.get(&id).cloned(),
                            tab: pane.agent.is_some(),
                        });
                    PaneState::Terminal {
                        cwd: place.map(|p| p.cwd.clone()),
                        scrollback: agent.is_none().then(|| self.scroll_keys.get(&id).cloned()).flatten(),
                        agent,
                        session: self.sessions.get(&id).cloned(),
                        token: self.sessions.contains_key(&id).then(|| pane.token.clone()).flatten(),
                    }
                }
                PaneKind::Browser(b) => PaneState::Browser {
                    url: b.read(cx).url.to_string(),
                },
                PaneKind::Chat(c) => PaneState::Chat {
                    thread: c.read(cx).thread_id().to_string(),
                },
                PaneKind::Settings(_) => PaneState::Settings,
                // An empty new tab is not worth bringing back.
                PaneKind::Launcher(_) | PaneKind::Review(_) | PaneKind::Page(..) => return None,
            })
        };
        fn node_state(
            node: &Node,
            pane: &dyn Fn(PaneId) -> Option<PaneState>,
            kept: &mut Vec<PaneId>,
        ) -> Option<NodeState> {
            match node {
                Node::Leaf(id) => {
                    let p = pane(*id)?;
                    kept.push(*id);
                    Some(NodeState::Pane(p))
                }
                Node::Split(axis, children, weights) => {
                    let mut kids: Vec<NodeState> = Vec::new();
                    let mut sizes: Vec<f32> = Vec::new();
                    for (c, w) in children.iter().zip(weights) {
                        if let Some(k) = node_state(c, pane, kept) {
                            kids.push(k);
                            sizes.push(*w);
                        }
                    }
                    match kids.len() {
                        0 => None,
                        1 => kids.pop(),
                        _ => Some(NodeState::Split {
                            row: *axis == Axis::Row,
                            children: kids,
                            sizes,
                        }),
                    }
                }
            }
        }

        let mut tabs = Vec::new();
        let mut active = 0;
        for (ix, tab) in self.tabs.iter().enumerate() {
            let mut kept = Vec::new();
            let Some(root) = node_state(&tab.root, &pane_state, &mut kept) else {
                continue;
            };
            if ix <= self.active {
                active = tabs.len();
            }
            tabs.push(TabState {
                pinned: tab.pinned,
                focused: kept.iter().position(|&p| p == tab.focused).unwrap_or(0),
                root,
                on_top: tab.on_top,
                touched: kept.iter().filter_map(|id| self.touched.get(id)).max().copied(),
            });
        }
        // Background jobs are the service's to keep, not the session's.
        let jobs = Vec::new();
        Session {
            jobs,
            tabs,
            active,
            sidebar: self.sidebar,
            inspector: self.inspector.open,
            sidebar_width: self.sidebar_px,
            inspector_width: self.inspector_px,
            recent: self.recent,
        }
    }

    /// Write the layout when it has changed (checked once a second).
    /// Write every plain shell's scrollback (an agent's TUI redraws itself),
    /// and drop files no pane will take back.
    fn save_scrollback(&mut self, cx: &App) {
        let dir = crate::session::scrollback_file("x").parent().map(PathBuf::from);
        let Some(dir) = dir else { return };
        let _ = std::fs::create_dir_all(&dir);
        let mut keep = std::collections::HashSet::new();
        let stamp = "\x1b[2m── restored ──\x1b[0m\r\n";
        for (id, pane) in &self.panes {
            let PaneKind::Terminal(t) = &pane.kind else { continue };
            let agent = self.places.get(id).and_then(|p| p.agent.as_ref()).is_some() || pane.agent.is_some();
            let Some(key) = self.scroll_keys.get(id).filter(|_| !agent) else { continue };
            let Some(text) = t.read(cx).history_ansi().filter(|t| !t.trim().is_empty()) else { continue };
            let path = crate::session::scrollback_file(key);
            if std::fs::write(&path, format!("{text}{stamp}")).is_ok() {
                keep.insert(path);
            }
        }
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !keep.contains(&e.path()) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        // The session must name these keys even if nothing else changed.
        self.saved_session = None;
        self.save_session(cx);
    }

    fn save_session(&mut self, cx: &App) {
        if !self.restored {
            return;
        }
        let now = self.snapshot(cx);
        if self.saved_session.as_ref() != Some(&now) {
            self.saved_session = Some(now.clone());
            std::thread::spawn(move || crate::session::save(&now));
        }
    }

    /// Start the control chat over: its conversation is filed away like a
    /// closed thread, and a fresh one opens in its place.
    fn new_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.control_tab() {
            self.close_tab(ix, window, cx);
        }
        self.open_control(Vec::new(), window, cx);
        if let Some(ix) = self.control_tab() {
            self.activate(ix, window, cx);
        }
    }

    /// Restart the control chat's agent: the same conversation, a new
    /// process (Claude resumes its own session).
    fn restart_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.control_tab() else {
            return self.open_control(Vec::new(), window, cx);
        };
        let old = self.tabs[ix].root.first();
        let Some(PaneKind::Chat(c)) = self.panes.get(&old).map(|p| &p.kind) else {
            return;
        };
        let mut saved = c.read(cx).saved(cx);
        saved.control = true;
        let new = match self.make_pane(Open::Thread(Box::new(saved)), None, window, cx) {
            Ok(id) => id,
            Err(e) => return self.flash(format!("Could not restart the agent: {e}"), cx),
        };
        if let Some(PaneKind::Chat(c)) = self.panes.get(&new).map(|p| &p.kind) {
            c.update(cx, |c, _| c.control = true);
        }
        let tab = &mut self.tabs[ix];
        tab.root.replace(old, new);
        if tab.focused == old {
            tab.focused = new;
        }
        // Not archived: the new pane carries the same thread on.
        self.panes.remove(&old);
        self.focus_pane(new, window, cx);
        self.flash("Agent restarted", cx);
    }

    fn control_tab(&self) -> Option<usize> {
        self.tabs.iter().position(|t| t.pinned)
    }

    fn new_tab(
        &mut self,
        what: Open,
        agent: Option<&'static Agent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let id = self.make_pane(what, agent, window, cx)?;
        self.tabs.push(Tab {
            root: Node::Leaf(id),
            focused: id,
            pinned: false,
            on_top: false,
        });
        self.focus_pane(id, window, cx);
        Ok(id)
    }

    fn split(
        &mut self,
        beside: PaneId,
        what: Open,
        axis: Axis,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let Some(ix) = self.tab_of(beside) else {
            return self.new_tab(what, None, window, cx);
        };
        let id = self.make_pane(what, None, window, cx)?;
        self.tabs[ix].root.split(beside, id, axis);
        self.focus_pane(id, window, cx);
        Ok(id)
    }

    /// Open the code editor on the project the focused pane is in (or
    /// `root`), reusing one already open on it: on its file tree or its git
    /// changes, and with `file` open when given.
    pub fn open_editor(
        &mut self,
        view: &'static str,
        root: Option<PathBuf>,
        file: Option<(String, Option<u32>, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let root = root
            .or_else(|| self.focused().and_then(|id| self.cwd_of(id, cx)))
            .or_else(|| self.focused_cwd(cx))
            .unwrap_or_else(|| self.cwd.clone());
        let root = crate::inspect::project_root(&root);
        let open = self.panes.iter().find_map(|(&id, p)| match &p.kind {
            PaneKind::Browser(b) if b.read(cx).editor().is_some_and(|e| e.root == root) => {
                Some((id, b.clone()))
            }
            _ => None,
        });
        if let Some((id, b)) = open {
            self.focus_pane(id, window, cx);
            b.update(cx, |b, _| {
                b.editor_show(view);
                if let Some((path, line, diff)) = &file {
                    b.editor_open(path, *line, *diff);
                }
            });
            cx.notify();
            return Ok(id);
        }
        let editor = crate::code::Editor {
            root,
            open: file.map(|(path, _, _)| path),
            view,
            commit: None,
        };
        self.new_tab(Open::Editor(editor), None, window, cx)
    }

    /// A commit in the editor's History, on the project at `root`.
    pub fn open_commit(
        &mut self,
        root: PathBuf,
        hash: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let root = crate::inspect::project_root(&root);
        let open = self.panes.iter().find_map(|(&id, p)| match &p.kind {
            PaneKind::Browser(b) if b.read(cx).editor().is_some_and(|e| e.root == root) => {
                Some((id, b.clone()))
            }
            _ => None,
        });
        if let Some((id, b)) = open {
            self.focus_pane(id, window, cx);
            b.read(cx).editor_commit(&hash);
            cx.notify();
            return Ok(id);
        }
        let editor = crate::code::Editor { root, open: None, view: "history", commit: Some(hash) };
        self.new_tab(Open::Editor(editor), None, window, cx)
    }

    /// Move the keyboard to the pane beside the focused one (⌥⌘ arrows).
    fn focus_toward(&mut self, axis: Axis, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if let Some(Some(next)) = tab.root.neighbor(tab.focused, axis, forward) {
            self.focus_pane(next, window, cx);
            cx.notify();
        }
    }

    fn tab_of(&self, pane: PaneId) -> Option<usize> {
        self.tabs.iter().position(|t| t.root.contains(pane))
    }

    fn focused(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|t| t.focused)
    }

    fn pane_env(&self, id: PaneId, token: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        if let Some(server) = &self.control {
            env.insert("DECK_SOCKET".into(), server.path.display().to_string());
            env.insert("DECK_TOKEN".into(), token.to_string());
        }
        env.insert("DECK_PANE".into(), id.to_string());
        env.insert("TERM_PROGRAM".into(), "deck".into());
        // Panes get their own token; the launcher's must not leak into them.
        env.insert("DECK_AUTOMATION_TOKEN".into(), String::new());
        // Launched from Finder there is no locale at all, and zsh then treats
        // every non-ASCII byte as garbage. Never override one the user set.
        if ["LANG", "LC_ALL", "LC_CTYPE"]
            .iter()
            .all(|k| std::env::var_os(k).is_none())
        {
            env.insert("LANG".into(), "en_US.UTF-8".into());
        }
        if let Ok(exe) = std::env::current_exe() {
            // So `deck …` resolves to this very binary inside its own panes,
            // and tools that honour $BROWSER (gh, python) open links in a
            // pane here instead of another app. Just the path: many run
            // $BROWSER as one program, and `deck URL` opens it.
            if let Some(dir) = exe.parent() {
                let path = std::env::var("PATH").unwrap_or_default();
                env.insert(
                    "PATH".into(),
                    crate::tidy_path(&format!("{}:{path}", dir.display())),
                );
            }
            env.insert("BROWSER".into(), exe.display().to_string());
        }
        env
    }

    /// Record focus that already happened natively (a click in the pane).
    fn mark_focused(&mut self, id: PaneId, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_of(id) {
            self.touch_mru(ix);
            if self.tabs[ix].focused != id || self.active != ix {
                self.tabs[ix].focused = id;
                self.active = ix;
                self.refresh_inspector(cx);
                cx.notify();
            }
        }
    }

    fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_of(id) else {
            return;
        };
        for p in self.tabs[ix].leaves() {
            self.unseen.remove(&p);
        }
        self.touched.insert(id, crate::local::now());
        self.active = ix;
        self.tabs[ix].focused = id;
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Terminal(t)) => {
                window.focus(&t.read(cx).focus);
                crate::browser::reclaim_keyboard(window);
                t.update(cx, |t, cx| t.seen(cx));
            }
            Some(PaneKind::Browser(b)) => {
                b.update(cx, |b, _| b.set_visible(true));
                window.blur();
                b.read(cx).focus_native();
            }
            Some(PaneKind::Chat(c)) => {
                window.focus(&c.read(cx).composer.read(cx).focus);
                crate::browser::reclaim_keyboard(window);
            }
            Some(PaneKind::Settings(s)) => {
                window.focus(&s.read(cx).focus);
                crate::browser::reclaim_keyboard(window);
            }
            Some(PaneKind::Review(r)) => {
                window.focus(&r.read(cx).focus);
                crate::browser::reclaim_keyboard(window);
            }
            Some(PaneKind::Launcher(l)) => {
                window.focus(&l.read(cx).focus);
                crate::browser::reclaim_keyboard(window);
            }
            Some(PaneKind::Page(..)) => {
                match self.page_focus.get(&id) {
                    Some(f) => window.focus(f),
                    None => window.blur(),
                }
                crate::browser::reclaim_keyboard(window);
            }
            None => {}
        }
        self.inspector.data = Default::default();
        self.refresh_inspector(cx);
        cx.notify();
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.tabs.get(ix) {
            let id = t.focused;
            self.focus_pane(id, window, cx);
        }
    }

    /// Open the branch picker for the repository at `root`, its top-left
    /// near `at` (it is moved to fit the window).
    pub fn open_branch_picker(&mut self, root: PathBuf, at: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let out = std::process::Command::new("git")
            .args(["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)\t%(committerdate:relative)", "refs/heads"])
            .current_dir(&root)
            .output();
        let branches: Vec<(String, String)> = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let (b, w) = l.split_once('\t')?;
                    Some((b.to_string(), w.replace(" ago", "")))
                })
                .collect(),
            _ => return self.flash("Not a git repository.".to_string(), cx),
        };
        let field = cx.new(|cx| {
            let mut f = crate::field::Field::new("Filter branches", cx);
            f.bare = true;
            f.popup = true;
            f
        });
        let sub = cx.subscribe_in(&field, window, |ws, _, ev: &crate::field::FieldEvent, window, cx| {
            use crate::field::FieldEvent;
            let n = ws.branch_matches(cx).len();
            match ev {
                FieldEvent::Changed => {
                    if let Some(p) = ws.branch_picker.as_mut() {
                        p.sel = 0;
                    }
                }
                FieldEvent::PopupKey("up") => {
                    if let Some(p) = ws.branch_picker.as_mut() {
                        p.sel = (p.sel + n.max(1) - 1) % n.max(1);
                    }
                }
                FieldEvent::PopupKey("down") | FieldEvent::PopupKey("tab") => {
                    if let Some(p) = ws.branch_picker.as_mut() {
                        p.sel = (p.sel + 1) % n.max(1);
                    }
                }
                FieldEvent::PopupKey("enter") | FieldEvent::Submit(_) => {
                    let sel = ws.branch_picker.as_ref().map_or(0, |p| p.sel);
                    if let Some(b) = ws.branch_matches(cx).get(sel).cloned() {
                        ws.switch_branch(b, window, cx);
                    }
                }
                FieldEvent::PopupKey("escape") | FieldEvent::Cancel => ws.close_branch_picker(window, cx),
                _ => {}
            }
            cx.notify();
        });
        window.focus(&field.read(cx).focus);
        let current = git_head(&root);
        self.branch_picker = Some(BranchPicker { field, _sub: sub, branches, current, sel: 0, root, at });
        cx.notify();
    }

    /// What to open on a host: its shell, an agent's own interface, or a
    /// Deck chat whose agent runs there.
    fn open_remote(&mut self, host: &str, dir: &str, what: RemoteOpen, window: &mut Window, cx: &mut Context<Self>) {
        let dir = if dir.trim().is_empty() { "~".to_string() } else { dir.trim().to_string() };
        let opened = match what {
            RemoteOpen::Terminal => self.new_tab(
                Open::Terminal { cmd: Some(crate::remote::terminal_command(host, &dir, None)), cwd: None },
                None,
                window,
                cx,
            ),
            RemoteOpen::Tui(agent) => self.new_tab(
                Open::Terminal { cmd: Some(crate::remote::terminal_command(host, &dir, Some(agent.cmd))), cwd: None },
                None,
                window,
                cx,
            ),
            RemoteOpen::Chat(agent) => {
                let opened = self.new_tab(Open::Chat { agent: Some(agent.to_string()), cwd: None }, None, window, cx);
                if let Ok(id) = &opened {
                    if let Some(PaneKind::Chat(c)) = self.panes.get(id).map(|p| &p.kind) {
                        let (host, dir) = (host.to_string(), PathBuf::from(&dir));
                        c.update(cx, |c, cx| c.set_remote(host, dir, cx));
                    }
                }
                opened
            }
        };
        match opened {
            Ok(_) => self.flash(format!("On {host}"), cx),
            Err(e) => self.flash(format!("Couldn't open {host}: {e}"), cx),
        }
    }

    fn open_remote_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| {
            // Empty means home (`open_remote` reads it as `~`).
            let mut f = crate::field::Field::new("~  (home), or a folder such as ~/src/app", cx);
            f.bare = true;
            f.popup = true;
            f
        });
        let sub = cx.subscribe_in(&field, window, |ws, _, ev: &crate::field::FieldEvent, window, cx| {
            use crate::field::FieldEvent;
            let Some(p) = ws.remote_picker.as_mut() else { return };
            let n = p.hosts.len().max(1);
            match ev {
                FieldEvent::PopupKey("up") => p.sel = (p.sel + n - 1) % n,
                FieldEvent::PopupKey("down") | FieldEvent::PopupKey("tab") => p.sel = (p.sel + 1) % n,
                FieldEvent::PopupKey("enter") | FieldEvent::Submit(_) => {
                    if let Some(h) = p.hosts.get(p.sel).map(|h| h.name.clone()) {
                        let dir = p.field.read(cx).text().to_string();
                        ws.close_remote_picker(window, cx);
                        ws.open_remote(&h, &dir, RemoteOpen::Terminal, window, cx);
                    }
                }
                FieldEvent::PopupKey("escape") | FieldEvent::Cancel => ws.close_remote_picker(window, cx),
                _ => {}
            }
            cx.notify();
        });
        window.focus(&field.read(cx).focus);
        self.remote_picker = Some(RemotePicker { hosts: crate::remote::hosts(), field, sel: 0, _sub: sub });
        cx.notify();
    }

    fn close_remote_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_picker.take().is_some() {
            if let Some(id) = self.focused() {
                self.focus_pane(id, window, cx);
            }
            cx.notify();
        }
    }

    fn render_remote_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.remote_picker.as_ref()?;
        let dir = p.field.read(cx).text().to_string();
        let agents: Vec<&'static Agent> = self
            .installed
            .iter()
            .copied()
            .filter(|a| matches!(a.cmd, "claude" | "codex" | "gemini" | "opencode"))
            .collect();
        // A quiet action: no border, a tint on hover; `primary` (the one ⏎
        // opens) carries the tint at rest.
        let action = |id: ElementId, primary: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(u(6.))
                .h(u(26.))
                .px(u(9.))
                .rounded(u(6.))
                .text_size(u(12.))
                .text_color(rgb(if primary { theme::text() } else { theme::text_2() }))
                .when(primary, |d| d.bg(rgba(theme::tint())))
                .cursor_pointer()
                .hover(|s| s.bg(rgba(theme::tint())).text_color(rgb(theme::text())))
        };
        let mut list = div().id("remote-list").flex().flex_col().gap(u(2.)).max_h(u(380.)).overflow_y_scroll().p(u(6.));
        for (i, h) in p.hosts.iter().enumerate() {
            let selected = i == p.sel;
            let mut row = div()
                .id(ElementId::NamedInteger("host".into(), i as u64))
                .flex()
                .flex_col()
                .gap(u(8.))
                .px(u(10.))
                .py(u(9.))
                .rounded(u(8.))
                .when(selected, |d| d.bg(rgb(theme::row_active())))
                .when(!selected, |d| d.hover(|s| s.bg(rgb(theme::row_hover()))))
                .on_hover(cx.listener(move |ws, hovered: &bool, _, cx| {
                    if let Some(p) = ws.remote_picker.as_mut().filter(|p| *hovered && p.sel != i) {
                        p.sel = i;
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(10.))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_center()
                                .size(u(28.))
                                .rounded(u(7.))
                                .bg(rgba(theme::tint()))
                                .child(icon(Icon::Globe, 14., if selected { theme::text_2() } else { theme::dim() })),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .one_line()
                                        .text_size(u(13.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(theme::text()))
                                        .child(h.name.clone()),
                                )
                                .children((!h.detail.is_empty()).then(|| {
                                    div().one_line().text_size(u(11.5)).text_color(rgb(theme::dim())).child(h.detail.clone())
                                })),
                        )
                        .when(!selected, |d| d.child(icon(Icon::Terminal, 13., theme::dim()))),
                );
            if selected {
                let (host, d) = (h.name.clone(), dir.clone());
                let mut actions = div().flex().flex_wrap().items_center().gap(u(4.)).pl(u(38.)).child(
                    action(ElementId::NamedInteger("host-term".into(), i as u64), true)
                        .child(icon(Icon::Terminal, 12., theme::text_2()))
                        .child("Terminal")
                        .child(crate::kbd::key("⏎"))
                        .on_click(cx.listener(move |ws, _, w, cx| {
                            ws.close_remote_picker(w, cx);
                            ws.open_remote(&host, &d, RemoteOpen::Terminal, w, cx);
                        })),
                );
                for (j, a) in agents.iter().enumerate() {
                    let (host, d, agent) = (h.name.clone(), dir.clone(), *a);
                    let chat = self.chats.as_ref().is_some_and(|c| c.registry.get(&agent_core::AgentId::new(agent.cmd)).is_some());
                    actions = actions.child(
                        action(ElementId::Name(format!("host-{i}-{j}").into()), false)
                            .child(icon(Icon::agent(agent.cmd), 12., Icon::agent(agent.cmd).brand().unwrap_or(theme::text_2())))
                            .child(agent.name)
                            .on_click(cx.listener(move |ws, ev: &gpui::ClickEvent, w, cx| {
                                // A click: Deck's chat with it there (its own
                                // interface when Deck has no chat for it);
                                // ⌥-click: its own interface in a terminal.
                                let what = if chat && !ev.modifiers().alt { RemoteOpen::Chat(agent.cmd) } else { RemoteOpen::Tui(agent) };
                                ws.close_remote_picker(w, cx);
                                ws.open_remote(&host, &d, what, w, cx);
                            })),
                    );
                }
                row = row.child(actions);
            }
            list = list.child(row);
        }
        if p.hosts.is_empty() {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(6.))
                    .px(u(12.))
                    .py(u(18.))
                    .text_size(u(12.5))
                    .text_color(rgb(theme::dim()))
                    .child(div().text_color(rgb(theme::text_2())).child("No hosts yet"))
                    .child("Add a Host to ~/.ssh/config, or run `deck remote add user@host --name box --key ~/box.pem`."),
            );
        }
        let panel = div()
            .id("remote-picker")
            .absolute()
            .top(u(72.))
            .left_0()
            .right_0()
            .mx_auto()
            .w(u(600.))
            .flex()
            .flex_col()
            .rounded(u(12.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .shadow_lg()
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .h(u(48.))
                    .px(u(16.))
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .child(icon(Icon::Folder, 14., theme::dim()))
                    .child(div().flex_1().text_size(u(13.5)).child(p.field.clone()))
                    .child(crate::kbd::key("esc")),
            )
            .child(
                div()
                    .px(u(16.))
                    .pt(u(10.))
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child(format!("Hosts · {}", p.hosts.len())),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(u(34.))
                    .px(u(16.))
                    .border_t_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child(crate::kbd::hints("↑↓ choose · ⏎ terminal · ⌥ click an agent for its own interface · esc close").gap_x(u(18.))),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _, w, cx| ws.close_remote_picker(w, cx)),
                )
                .child(gpui::deferred(panel).with_priority(3))
                .into_any_element(),
        )
    }

    fn close_branch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_picker.take().is_some() {
            if let Some(id) = self.focused() {
                self.focus_pane(id, window, cx);
            }
            cx.notify();
        }
    }

    /// The branches the filter lets through, in order.
    fn branch_matches(&self, cx: &App) -> Vec<String> {
        let Some(p) = &self.branch_picker else { return Vec::new() };
        let q = p.field.read(cx).text().to_lowercase();
        p.branches.iter().map(|(b, _)| b).filter(|b| q.split_whitespace().all(|w| b.to_lowercase().contains(w))).cloned().collect()
    }

    /// `git switch` in the picker's repository; what git said if it would
    /// not (uncommitted changes in the way).
    fn switch_branch(&mut self, branch: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.branch_picker.as_ref().map(|p| p.root.clone()) else { return };
        self.close_branch_picker(window, cx);
        let task = cx.background_executor().spawn(async move {
            // One that does not exist yet is made from where you are.
            let exists = std::process::Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
                .current_dir(&root)
                .status()
                .is_ok_and(|s| s.success());
            let args: Vec<&str> = if exists { vec!["switch", &branch] } else { vec!["switch", "-c", &branch] };
            std::process::Command::new("git").args(&args).current_dir(&root).output().map(|o| (o, branch))
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |ws, cx| {
                match r {
                    Ok((o, b)) if o.status.success() => ws.flash(format!("On {b}."), cx),
                    Ok((o, _)) => {
                        let why = String::from_utf8_lossy(&o.stderr);
                        let why = why.lines().find(|l| l.starts_with("error")).unwrap_or("git would not switch").trim_start_matches("error: ");
                        ws.flash(format!("Couldn't switch: {why}"), cx)
                    }
                    Err(e) => ws.flash(format!("Couldn't run git: {e}"), cx),
                }
                ws.refresh_bar_git(cx);
                ws.refresh_inspector(cx);
            });
        })
        .detach();
    }

    /// The branch switcher, over the status bar's branch: the current one
    /// on top with its project, a filter, every branch with how long since
    /// its last commit, and, for a name that matches none, "Create branch".
    fn render_branch_picker(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.branch_picker.as_ref()?;
        let matches = self.branch_matches(cx);
        let when: HashMap<&str, &str> = p.branches.iter().map(|(b, w)| (b.as_str(), w.as_str())).collect();
        const W: f32 = 320.;
        let zoom = self.settings.zoom;
        let vw = f32::from(window.viewport_size().width) / zoom;
        let vh = f32::from(window.viewport_size().height) / zoom;
        let left = (f32::from(p.at.x) / zoom).min(vw - W - 8.).max(8.);
        // Above its anchor when it sits low (the status bar), else below.
        let low = f32::from(p.at.y) / zoom > vh / 2.;
        let typed = p.field.read(cx).text().trim().to_string();
        let can_create = !typed.is_empty()
            && !typed.contains(char::is_whitespace)
            && !p.branches.iter().any(|(b, _)| *b == typed);
        let project = p.root.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let mut list = div().id("branch-list").flex().flex_col().gap(u(1.)).max_h(u(300.)).overflow_y_scroll().p(u(4.));
        for (i, b) in matches.iter().enumerate() {
            let on = i == p.sel;
            let current = p.current.as_deref() == Some(b.as_str());
            let name = b.clone();
            list = list.child(
                div()
                    .id(ElementId::NamedInteger("branch".into(), i as u64))
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(30.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .cursor_pointer()
                    .text_size(u(13.))
                    .text_color(rgb(if on || current { theme::text() } else { theme::text_2() }))
                    .when(on, |d| d.bg(rgba(theme::alpha(theme::text(), 0x12))))
                    .when(!on, |d| d.hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text()))))
                    .child(icon(Icon::Git, 12., if current { theme::accent() } else { theme::dim() }))
                    .child(div().flex_1().min_w_0().one_line().when(current, |d| d.font_weight(FontWeight::MEDIUM)).child(b.clone()))
                    .when(current, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .h(u(18.))
                                .px(u(6.))
                                .flex()
                                .items_center()
                                .rounded(u(4.))
                                .bg(rgba(theme::alpha(theme::accent(), 0x22)))
                                .text_size(u(10.5))
                                .text_color(rgb(theme::accent()))
                                .child("Current"),
                        )
                    })
                    .children(when.get(b.as_str()).map(|w| div().flex_none().text_size(u(11.)).text_color(rgb(theme::dim())).child(w.to_string())))
                    .on_mouse_down(MouseButton::Left, cx.listener(move |ws, _, window, cx| ws.switch_branch(name.clone(), window, cx))),
            );
        }
        if matches.is_empty() && !can_create {
            list = list.child(div().h(u(36.)).flex().items_center().justify_center().text_size(u(12.5)).text_color(rgb(theme::dim())).child("No branch matches"));
        }
        let action = |id: &'static str, glyph: Icon, label: String| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(8.))
                .h(u(30.))
                .px(u(8.))
                .rounded(u(6.))
                .cursor_pointer()
                .text_size(u(12.5))
                .text_color(rgb(theme::text_2()))
                .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                .child(icon(glyph, 13., theme::dim()))
                .child(div().flex_1().min_w_0().one_line().child(label))
        };
        let new_name = typed.clone();
        let footer = div()
            .flex()
            .flex_col()
            .gap(u(1.))
            .p(u(4.))
            .border_t_1()
            .border_color(rgba(theme::hairline()))
            .when(can_create, |d| {
                d.child(
                    action("branch-create", Icon::Plus, format!("Create branch “{new_name}” from here"))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |ws, _, window, cx| ws.switch_branch(new_name.clone(), window, cx))),
                )
            })
            .child(
                action("branch-changes", Icon::Files, "Open changes".to_string()).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, _, window, cx| {
                        ws.close_branch_picker(window, cx);
                        if let Err(e) = ws.open_editor("changes", None, None, window, cx) {
                            tracing::warn!("could not open the editor: {e}");
                        }
                    }),
                ),
            );
        let panel = div()
            .id("branch-picker")
            .absolute()
            .left(u(left))
            .when(low, |d| d.bottom(u(vh - f32::from(p.at.y) / zoom + 6.)))
            .when(!low, |d| d.top(u(f32::from(p.at.y) / zoom + 6.)))
            .w(u(W))
            .flex()
            .flex_col()
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(6.))
                    .h(u(34.))
                    .px(u(12.))
                    .text_size(u(12.))
                    .child(div().font_weight(FontWeight::MEDIUM).text_color(rgb(theme::text())).child("Switch branch"))
                    .child(div().flex_1())
                    .child(icon(Icon::Folder, 11., theme::dim()))
                    .child(div().text_color(rgb(theme::dim())).child(project)),
            )
            .child(
                // The filter: a field in the header, no box of its own.
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(34.))
                    .mx(u(4.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .bg(rgba(theme::tint()))
                    .child(icon(Icon::Search, 13., theme::dim()))
                    .child(div().flex_1().child(p.field.clone())),
            )
            .child(list)
            .child(footer)
            .child(
                div()
                    .flex()
                    .gap(u(12.))
                    .h(u(26.))
                    .px(u(12.))
                    .items_center()
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child("↑↓ choose")
                    .child("⏎ switch")
                    .child("esc close"),
            )
            .with_animation(
                "branch-picker-in",
                Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                |d, t| d.opacity(t),
            );
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, window, cx| ws.close_branch_picker(window, cx)))
                .child(gpui::deferred(panel).with_priority(3))
                .into_any_element(),
        )
    }

    /// The import offer was answered: no browser pane shows it again.
    fn answer_import_offer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for p in self.panes.values() {
            if let PaneKind::Browser(b) = &p.kind {
                b.update(cx, |b, cx| {
                    b.import_offer = None;
                    b.importing = false;
                    cx.notify();
                });
            }
        }
        let mut next = self.settings.clone();
        next.import_offered = true;
        self.apply_settings(next, window, cx);
    }

    /// Import from the browser the offer named (the first one found):
    /// history, bookmarks and its cookies, then the offer goes.
    fn import_from_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(src) = crate::browser_import::sources().into_iter().next() else {
            return self.answer_import_offer(window, cx);
        };
        for p in self.panes.values() {
            if let PaneKind::Browser(b) = &p.kind {
                b.update(cx, |b, cx| {
                    b.importing = true;
                    cx.notify();
                });
            }
        }
        let task = cx.background_executor().spawn(async move { crate::browser_import::import(&src, true) });
        cx.spawn_in(window, async move |this, cx| {
            let (report, jar) = task.await;
            if !jar.is_empty() {
                crate::browser_import::install_cookies(&jar);
            }
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.flash(report.summary(), cx);
                ws.answer_import_offer(window, cx);
            });
        })
        .detach();
    }

    /// Go to the next pane that wants you (a question, an approval, a bell),
    /// after the one you are in, round the end.
    fn next_needs_you(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let order: Vec<PaneId> = self.tabs.iter().flat_map(|t| t.leaves()).collect();
        let at = self.focused().and_then(|f| order.iter().position(|p| *p == f)).unwrap_or(0);
        let next = (1..=order.len())
            .map(|i| order[(at + i) % order.len()])
            .find(|id| self.status(*id, cx) == Some(Status::Attention) || self.unseen.contains(id));
        match next {
            Some(id) => self.focus_pane(id, window, cx),
            None => self.flash("Nothing needs you.".to_string(), cx),
        }
    }

    /// Open the sidebar's filter (or focus it if open).
    fn open_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar = true;
        if let Some((f, _)) = &self.filter {
            window.focus(&f.read(cx).focus);
            return;
        }
        let field = cx.new(|cx| crate::field::Field::new("Filter tabs and threads", cx));
        let sub = cx.subscribe_in(&field, window, |ws, _, ev: &crate::field::FieldEvent, window, cx| match ev {
            crate::field::FieldEvent::Cancel => {
                ws.filter = None;
                if let Some(id) = ws.focused() {
                    ws.focus_pane(id, window, cx);
                }
                cx.notify();
            }
            crate::field::FieldEvent::Submit(_) => {
                // ⏎ opens the first row that matches.
                if let Some(ix) = (0..ws.tabs.len()).find(|&ix| ws.row_matches(ix, cx)) {
                    ws.filter = None;
                    ws.activate(ix, window, cx);
                }
                cx.notify();
            }
            _ => cx.notify(),
        });
        window.focus(&field.read(cx).focus);
        self.filter = Some((field, sub));
        cx.notify();
    }

    /// The row passes the sidebar's filter: its title, project or second
    /// line contain every word typed.
    fn row_matches(&self, ix: usize, cx: &App) -> bool {
        let Some((f, _)) = &self.filter else { return true };
        let q = f.read(cx).text().to_lowercase();
        if q.trim().is_empty() {
            return true;
        }
        let Some(tab) = self.tabs.get(ix) else { return false };
        let mut hay = self.pane_title(tab.focused, cx).to_lowercase();
        for id in tab.leaves() {
            if let Some(p) = self.places.get(&id) {
                hay.push(' ');
                hay.push_str(&p.project.to_lowercase());
            }
            if let Some(d) = self.row_detail(id, cx) {
                hay.push(' ');
                hay.push_str(&d.to_lowercase());
            }
        }
        q.split_whitespace().all(|w| hay.contains(w))
    }

    /// A click on a sidebar row: ⌘ adds it to (or takes it from) the
    /// selection, ⇧ selects the rows between the last one and it; a plain
    /// click clears the selection and opens the row.
    fn row_click(&mut self, ix: usize, m: gpui::Modifiers, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.tabs.get(ix).map(|t| t.leaves()[0]) else { return };
        if m.platform {
            if !self.picked.remove(&key) {
                self.picked.insert(key);
            }
            self.pick_anchor = Some(key);
            cx.notify();
            return;
        }
        if m.shift {
            let from = self.pick_anchor.and_then(|a| self.tab_of(a)).unwrap_or(self.active);
            let (a, b) = if from <= ix { (from, ix) } else { (ix, from) };
            for t in &self.tabs[a..=b] {
                if !t.pinned {
                    self.picked.insert(t.leaves()[0]);
                }
            }
            self.pick_anchor = Some(key);
            cx.notify();
            return;
        }
        if !self.picked.is_empty() {
            self.picked.clear();
            cx.notify();
        }
        self.pick_anchor = Some(key);
        self.activate(ix, window, cx);
    }

    fn row_picked(&self, ix: usize) -> bool {
        self.tabs.get(ix).is_some_and(|t| self.picked.contains(&t.leaves()[0]))
    }

    /// Close every picked row.
    fn close_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut ixs: Vec<usize> = std::mem::take(&mut self.picked).iter().filter_map(|id| self.tab_of(*id)).collect();
        ixs.sort_unstable();
        ixs.dedup();
        for ix in ixs.into_iter().rev() {
            if !self.tabs[ix].pinned {
                self.close_tab(ix, window, cx);
            }
        }
        cx.notify();
    }

    /// Close tabs nothing has happened in for a day: not pinned, not the
    /// control chat, not working or waiting on you, not jobs.
    fn close_idle_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = crate::local::now();
        let old: Vec<usize> = (0..self.tabs.len())
            .filter(|&ix| {
                let t = &self.tabs[ix];
                !t.pinned
                    && !t.on_top
                    && Some(ix) != self.settings_tab()
                    && t.leaves().iter().all(|&id| {
                        !matches!(self.status(id, cx), Some(Status::Working | Status::Attention))
                            && now - self.last_active(id, cx) > 86_400
                    })
            })
            .collect();
        let n = old.len();
        for ix in old.into_iter().rev() {
            self.close_tab(ix, window, cx);
        }
        self.flash(
            match n {
                0 => "Nothing idle for a day.".to_string(),
                1 => "Closed 1 tab idle for a day.".to_string(),
                n => format!("Closed {n} tabs idle for a day."),
            },
            cx,
        );
    }

    /// Drop a pane for good. A closed chat thread is archived, not lost.
    fn discard(&mut self, id: PaneId, cx: &mut Context<Self>) {
        // A tab closed: its shell in the service ends too. (Quitting Deck
        // discards nothing, so sessions outlive it.)
        if let Some(name) = self.sessions.remove(&id) {
            cx.background_executor().spawn(async move { crate::daemon::stop(&name) }).detach();
        }
        self.jobs.retain(|j| *j != id);
        self.job_cmds.remove(&id);
        if self.job_open == Some(id) {
            self.job_open = None;
        }
        self.working_since.remove(&id);
        self.touched.remove(&id);
        self.settled.remove(&id);
        self.restored_as.remove(&id);
        if let Some(Pane {
            kind: PaneKind::Chat(c),
            ..
        }) = self.panes.remove(&id)
        {
            c.update(cx, |c, _| c.archive());
        }
    }

    fn close_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_of(id) else {
            self.discard(id, cx);
            return;
        };
        self.discard(id, cx);
        let tab = &mut self.tabs[ix];
        if tab.root == Node::Leaf(id) {
            self.tabs.remove(ix);
            if self.active > ix || self.active >= self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        } else {
            tab.root.remove(id);
            if tab.focused == id {
                tab.focused = tab.leaves()[0];
            }
        }
        if !self.tabs.is_empty() {
            self.activate(self.active, window, cx);
        }
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        for id in tab.leaves() {
            self.discard(id, cx);
        }
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if !self.tabs.is_empty() {
            self.activate(self.active, window, cx);
        }
        cx.notify();
    }

    /// Every other tab of the same kind as `keep` (threads or workspace),
    /// pinned ones aside. A terminal's "Close Other Tabs" leaves your chats.
    fn close_other_tabs(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(keep) else {
            return;
        };
        let (kept, group) = (tab.focused, self.group(tab));
        for ix in (0..self.tabs.len()).rev() {
            if ix == keep || self.tabs[ix].pinned || self.group(&self.tabs[ix]) != group {
                continue;
            }
            for id in self.tabs[ix].leaves() {
                self.discard(id, cx);
            }
            self.tabs.remove(ix);
        }
        if let Some(ix) = self.tab_of(kept) {
            self.activate(ix, window, cx);
        }
        cx.notify();
    }

    /// A sidebar row's right-click menu. Items find their tab again by pane
    /// when they run, since tabs can open or close while the menu is up.
    fn open_tab_menu(
        &mut self,
        ix: usize,
        mut at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::menu::{Entry, Item};
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let pane = tab.focused;
        let threads = self.group(tab) == Group::Threads;
        let url = match self.panes.get(&pane).map(|p| &p.kind) {
            Some(PaneKind::Browser(b)) => {
                Some(b.read(cx).url.to_string()).filter(|u| !u.is_empty())
            }
            _ => None,
        };
        // Control, Settings and a task's work have places of their own.
        let pinnable = !tab.pinned && Some(ix) != self.settings_tab() && !self.is_task_tab(tab, cx);
        let on_top = tab.on_top;
        let ws = cx.entity().downgrade();
        let mut entries: Vec<Entry> = Vec::new();
        if pinnable {
            let pin = ws.clone();
            entries.push(
                Item::new(if on_top { "Unpin" } else { "Pin to Top" }, move |_, cx| {
                    let _ = pin.update(cx, |ws, cx| {
                        if let Some(ix) = ws.tab_of(pane) {
                            ws.tabs[ix].on_top = !on_top;
                            cx.notify();
                        }
                    });
                })
                .icon(Icon::Pin)
                .into(),
            );
            entries.push(Entry::Separator);
        }
        if let Some(url) = url {
            entries.push(
                Item::new("Copy URL", move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(url.clone()))
                })
                .icon(Icon::Link)
                .into(),
            );
            entries.push(Entry::Separator);
        }
        let pick = ws.clone();
        let picked_now = self.picked.contains(&pane) || self.tab_of(pane).is_some_and(|ix| self.row_picked(ix));
        entries.push(
            Item::new(if picked_now { "Deselect" } else { "Select" }, move |_, cx| {
                let _ = pick.update(cx, |ws, cx| {
                    if let Some(ix) = ws.tab_of(pane) {
                        let key = ws.tabs[ix].leaves()[0];
                        if !ws.picked.remove(&key) {
                            ws.picked.insert(key);
                        }
                        ws.pick_anchor = Some(key);
                        cx.notify();
                    }
                });
            })
            .icon(Icon::Check)
            .into(),
        );
        entries.push(Entry::Separator);
        // Where it works: a terminal there, Finder, its path, its files.
        if let Some(dir) = self.cwd_of(pane, cx).filter(|d| d.is_dir()) {
            let (term, reveal, copy, files) = (ws.clone(), dir.clone(), dir.clone(), ws.clone());
            let (d1, d4) = (dir.clone(), dir.clone());
            entries.push(
                Item::new("Open Terminal Here", move |window, cx| {
                    let d = d1.clone();
                    let _ = term.update(cx, |ws, cx| {
                        let _ = ws.new_tab(Open::Terminal { cmd: None, cwd: Some(d) }, None, window, cx);
                    });
                })
                .icon(Icon::Terminal)
                .into(),
            );
            entries.push(
                Item::new("Open Files", move |window, cx| {
                    let d = d4.clone();
                    let _ = files.update(cx, |ws, cx| {
                        let root = crate::inspect::project_root(&d);
                        if let Err(e) = ws.open_editor("files", Some(root), None, window, cx) {
                            tracing::warn!("could not open the editor: {e}");
                        }
                    });
                })
                .icon(Icon::Files)
                .into(),
            );
            entries.push(
                Item::new("Reveal in Finder", move |_, _| {
                    let _ = std::process::Command::new("/usr/bin/open").arg(&reveal).spawn();
                })
                .icon(Icon::Folder)
                .into(),
            );
            entries.push(
                Item::new("Copy Path", move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.display().to_string()))
                })
                .icon(Icon::Copy)
                .into(),
            );
            entries.push(Entry::Separator);
        }
        if self.turn_marks.contains_key(&pane) {
            let diff = ws.clone();
            entries.push(
                Item::new("Last Turn's Changes", move |window, cx| {
                    let _ = diff.update(cx, |ws, cx| {
                        ws.focus_pane(pane, window, cx);
                        ws.show_turn_diff(window, cx);
                    });
                })
                .icon(Icon::Files)
                .into(),
            );
        }
        let key = tab.leaves()[0];
        let picked = self.picked.contains(&key);
        let pick = ws.clone();
        entries.push(
            Item::new(if picked { "Deselect" } else { "Select" }, move |_, cx| {
                let _ = pick.update(cx, |ws, cx| {
                    if !ws.picked.remove(&key) {
                        ws.picked.insert(key);
                    }
                    cx.notify();
                });
            })
            .icon(Icon::Check)
            .into(),
        );
        entries.push(Entry::Separator);
        let close = ws.clone();
        entries.push(
            Item::new(
                if threads { "Close Thread" } else { "Close Tab" },
                move |window, cx| {
                    let _ = close.update(cx, |ws, cx| {
                        if let Some(ix) = ws.tab_of(pane) {
                            ws.close_tab(ix, window, cx);
                        }
                    });
                },
            )
            .icon(Icon::Close)
            .into(),
        );
        entries.push(
            Item::new(
                if threads {
                    "Close Other Threads"
                } else {
                    "Close Other Tabs"
                },
                move |window, cx| {
                    let _ = ws.update(cx, |ws, cx| {
                        if let Some(ix) = ws.tab_of(pane) {
                            ws.close_other_tabs(ix, window, cx);
                        }
                    });
                },
            )
            .icon(Icon::CloseOthers)
            .into(),
        );
        let idle = cx.entity().downgrade();
        let picked_n = self.picked.len();
        if picked_n > 0 {
            let closer = cx.entity().downgrade();
            entries.push(
                Item::new(format!("Close {picked_n} Selected"), move |window, cx| {
                    let _ = closer.update(cx, |ws, cx| ws.close_picked(window, cx));
                })
                .icon(Icon::Close)
                .into(),
            );
        }
        entries.push(
            Item::new("Close Tabs Idle for a Day", move |window, cx| {
                let _ = idle.update(cx, |ws, cx| ws.close_idle_tabs(window, cx));
            })
            .icon(Icon::Clock)
            .into(),
        );
        // A page on screen is a native view the menu cannot cover, so keep
        // the menu over the sidebar.
        if self.browser_in_tab(self.active).is_some() {
            let zoom = self.settings.zoom;
            let room = (self.sidebar_px - crate::menu::MIN_W - theme::GUTTER) * zoom;
            at.x = at.x.min(px(room));
        }
        let restore = window.focused(cx);
        self.menu = Some(crate::menu::open(
            entries,
            at,
            restore,
            window,
            cx,
            |ws: &mut Self, window, cx| {
                ws.menu = None;
                // A page had the keyboard before the menu took it.
                if window.focused(cx).is_none() {
                    if let Some((_, b)) = ws.browser_in_tab(ws.active) {
                        b.read(cx).focus_native();
                    }
                }
            },
        ));
        cx.notify();
    }

    fn focused_browser_pane(&self) -> Option<Entity<BrowserPane>> {
        match &self.panes.get(&self.focused()?)?.kind {
            PaneKind::Browser(b) => Some(b.clone()),
            _ => None,
        }
    }

    fn browser_in_tab(&self, ix: usize) -> Option<(PaneId, Entity<BrowserPane>)> {
        let tab = self.tabs.get(ix)?;
        // Prefer the focused pane, then the first browser in the tab.
        std::iter::once(tab.focused)
            .chain(tab.leaves())
            .find_map(|id| match &self.panes.get(&id)?.kind {
                PaneKind::Browser(b) => Some((id, b.clone())),
                _ => None,
            })
    }

    /// Open `url` next to `pane`: in its tab's browser if it has one, else in a
    /// new browser split beside it. This is what makes a dev server's URL, a
    /// ⌘-clicked link, or `$BROWSER` land right next to the terminal.
    pub fn open_url_near(
        &mut self,
        pane: PaneId,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        if let Some((id, b)) = self.tab_of(pane).and_then(|ix| self.browser_in_tab(ix)) {
            let _ = b.update(cx, |b, cx| b.navigate(url, cx));
            self.focus_pane(id, window, cx);
            return Some(id);
        }
        self.split(
            pane,
            Open::Browser { url: url.into() },
            Axis::Row,
            window,
            cx,
        )
        .ok()
    }

    pub fn open_url_beside(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.focused() {
            Some(p) => {
                self.open_url_near(p, url, window, cx);
            }
            None => {
                let _ = self.new_tab(Open::Browser { url: url.into() }, None, window, cx);
            }
        }
    }

    fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let b = match self.browser_in_tab(self.active) {
            Some((id, b)) => {
                self.focus_pane(id, window, cx);
                b
            }
            None => {
                let id = match self.focused() {
                    Some(p) => self.split(
                        p,
                        Open::Browser {
                            url: "about:blank".into(),
                        },
                        Axis::Row,
                        window,
                        cx,
                    ),
                    None => self.new_tab(
                        Open::Browser {
                            url: "about:blank".into(),
                        },
                        None,
                        window,
                        cx,
                    ),
                };
                match id.ok().and_then(|id| self.panes.get(&id)) {
                    Some(Pane {
                        kind: PaneKind::Browser(b),
                        ..
                    }) => b.clone(),
                    _ => return,
                }
            }
        };
        b.update(cx, |b, cx| b.focus_address(window, cx));
        self.refresh_servers(cx);
    }

    fn cycle_pane(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let order = tab.leaves();
        let at = order.iter().position(|&p| p == tab.focused).unwrap_or(0) as isize;
        let next = order[(at + step).rem_euclid(order.len() as isize) as usize];
        self.focus_pane(next, window, cx);
    }

    /// Save the project of terminal agent `id` as a checkpoint (out of the
    /// way of its branch, index and stash), off the main thread.
    fn snapshot_turn(&mut self, id: PaneId, cx: &mut Context<Self>) {
        let is_agent = matches!(self.panes.get(&id).map(|p| &p.kind), Some(PaneKind::Terminal(_)))
            && self.places.get(&id).is_some_and(|p| p.agent.is_some());
        let Some(cwd) = is_agent.then(|| self.cwd_of(id, cx)).flatten() else { return };
        let task = cx.background_executor().spawn(async move {
            let root = crate::checkpoint::root(&cwd)?;
            let refname = crate::checkpoint::refname(&format!("pane-{id}"), "turn");
            crate::checkpoint::save(&root, &refname).ok().map(|c| (root, c))
        });
        cx.spawn(async move |this, cx| {
            if let Some(found) = task.await {
                let _ = this.update(cx, |ws, _| {
                    ws.turn_marks.insert(id, found);
                });
            }
        })
        .detach();
    }

    /// What pane `id`'s agent changed since its turn began: `git diff`
    /// against the snapshot, and the files it added.
    fn turn_diff(&self, id: PaneId) -> Result<(PathBuf, String), String> {
        let (root, commit) = self.turn_marks.get(&id).ok_or("no turn recorded for that pane yet: one starts when its agent starts working")?;
        let out = std::process::Command::new("git")
            .args(["diff", "--stat", "--patch", commit.trim()])
            .current_dir(root)
            .output()
            .map_err(|e| e.to_string())?;
        Ok((root.clone(), String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    /// ⌘K "Last Turn's Changes": the focused agent's turn diff in a
    /// terminal beside it, paged.
    fn show_turn_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.focused() else { return };
        let Some((root, commit)) = self.turn_marks.get(&id).cloned() else {
            return self.flash("No agent turn recorded in this pane yet.".to_string(), cx);
        };
        let cmd = format!("git -c color.ui=always diff --stat --patch {} | less -R", commit.trim());
        if let Err(e) = self.split(id, Open::Terminal { cmd: Some(cmd), cwd: Some(root) }, Axis::Row, window, cx) {
            self.flash(format!("Couldn't show the diff: {e}"), cx);
        }
    }

    /// A tab was looked at: first in the recently-used order, unless a
    /// ⌃Tab walk is passing through it.
    fn touch_mru(&mut self, ix: usize) {
        let walking = self.mru_walk.is_some_and(|(_, at)| at.elapsed() < MRU_WALK);
        let Some(key) = self.tabs.get(ix).map(|t| t.leaves()[0]) else { return };
        if walking {
            return;
        }
        self.mru.retain(|k| *k != key);
        self.mru.insert(0, key);
    }

    /// ⌃Tab: the tab used before this one; pressed again within a moment,
    /// the one before that, and so on, as app switchers do. Where the walk
    /// stops becomes the most recent.
    fn recent_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let keys: Vec<PaneId> = self.tabs.iter().map(|t| t.leaves()[0]).collect();
        self.mru.retain(|k| keys.contains(k));
        for k in &keys {
            if !self.mru.contains(k) {
                self.mru.push(*k);
            }
        }
        if self.mru.len() < 2 {
            return;
        }
        let walking = self.mru_walk.is_some_and(|(_, at)| at.elapsed() < MRU_WALK);
        let pos = if walking {
            self.mru_walk.map_or(0, |(p, _)| p)
        } else {
            // The last walk ended where it stopped: that tab is now first.
            self.mru_walk = None;
            self.touch_mru(self.active);
            0
        };
        let n = self.mru.len() as isize;
        let next = (pos as isize + step).rem_euclid(n) as usize;
        self.mru_walk = Some((next, Instant::now()));
        let key = self.mru[next];
        if let Some(ix) = self.tab_of(key) {
            self.activate(ix, window, cx);
        }
    }

    /// The focused pane, out of its split and into a tab of its own, in
    /// front: a page or terminal that needs the whole window.
    fn move_to_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.focused() else { return };
        let Some(ix) = self.tab_of(id) else { return };
        if self.tabs[ix].root == Node::Leaf(id) {
            return self.flash("Already a tab of its own.".to_string(), cx);
        }
        let tab = &mut self.tabs[ix];
        tab.root.remove(id);
        if tab.focused == id {
            tab.focused = tab.leaves()[0];
        }
        self.tabs.insert(
            ix + 1,
            Tab {
                root: Node::Leaf(id),
                focused: id,
                pinned: false,
                on_top: false,
            },
        );
        self.focus_pane(id, window, cx);
        cx.notify();
    }

    fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.activate(
            (self.active as isize + step).rem_euclid(n) as usize,
            window,
            cx,
        );
    }

    /// Once a second. Does nothing that costs a process spawn or a repaint
    /// unless something visible changed.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ticks += 1;
        // Terminal agents waiting on you, read off their screens.
        let mut blocked = std::collections::HashSet::new();
        let mut newly: Vec<(PaneId, String)> = Vec::new();
        for (&id, pane) in &self.panes {
            let PaneKind::Terminal(t) = &pane.kind else { continue };
            let agent = pane.agent.map(|a| a.cmd).or_else(|| self.places.get(&id).and_then(|p| p.agent.as_ref().map(|(c, _)| *c)));
            let Some(agent) = agent else { continue };
            if let Some(why) = agent_blocked(agent, &t.read(cx).tail(24)) {
                if !self.blocked.contains(&id) {
                    newly.push((id, why.to_string()));
                }
                blocked.insert(id);
            }
        }
        for (id, why) in newly {
            self.alert(id, Some(why), window, cx);
        }
        if blocked != self.blocked {
            self.blocked = blocked;
            cx.notify();
        }
        if self.ticks % 2 == 0 && !self.daemon_asking {
            self.daemon_asking = true;
            let ask = cx.background_executor().spawn(async move { crate::daemon::list() });
            cx.spawn_in(window, async move |this, cx| {
                let jobs = ask.await;
                let _ = this.update_in(cx, |ws, window, cx| {
                    ws.daemon_asking = false;
                    // No answer is not "no jobs": leave every pane as it is
                    // (closing and reopening them is a flicker every 2 s).
                    if let Ok(jobs) = jobs {
                        ws.sync_daemon(jobs, window, cx);
                    }
                });
            })
            .detach();
        }
        // Agent dots go idle on a timer, not on an event; repaint only when
        // one of them actually changes.
        let statuses: Vec<(PaneId, Status)> = self
            .panes
            .keys()
            .filter_map(|&id| self.status(id, cx).map(|s| (id, s)))
            .collect();
        let mut sorted = statuses;
        sorted.sort_by_key(|(id, _)| *id);
        let focused = self.focused();
        let looking = window.is_window_active();
        let mut ask: Vec<(PaneId, String)> = Vec::new();
        let stamp = crate::local::now();
        for (id, now) in &sorted {
            let was = self.statuses.iter().find(|(p, _)| p == id).map(|(_, s)| *s);
            if was == Some(Status::Working) && *now != Status::Working && focused != Some(*id) {
                self.unseen.insert(*id);
            }
            // A terminal agent starting a turn: the project as it was, so
            // the turn's changes can be shown (`deck pane diff --turn`).
            if *now == Status::Working && was.is_some_and(|w| w != Status::Working) {
                self.snapshot_turn(*id, cx);
            }
            // Began or stopped work, or started wanting you.
            if was.is_some_and(|w| w != *now) {
                self.touched.insert(*id, stamp);
            }
            // A chat that started waiting on you, while you look elsewhere.
            if *now == Status::Attention
                && was != Some(Status::Attention)
                && !(looking && focused == Some(*id))
            {
                if let Some(PaneKind::Chat(c)) = self.panes.get(id).map(|p| &p.kind) {
                    let c = c.read(cx);
                    if let Some(why) = c.waiting_for() {
                        ask.push((*id, format!("{}: {why}", c.agent_name())));
                    }
                }
            }
        }
        // Agents (chats and agent terminals alike) that just stopped
        // working, after long enough that you may have looked away.
        let mut done: Vec<(PaneId, String)> = Vec::new();
        for (id, now) in &sorted {
            let was = self.statuses.iter().find(|(p, _)| p == id).map(|(_, s)| *s);
            if *now == Status::Working {
                self.working_since.entry(*id).or_insert_with(Instant::now);
            } else if let Some(since) = self.working_since.remove(id) {
                let long = since.elapsed() >= Duration::from_secs(10);
                if long
                    && was == Some(Status::Working)
                    && *now == Status::Idle
                    && !(looking && focused == Some(*id))
                {
                    done.push((*id, self.finished_line(*id, cx)));
                }
            }
        }
        for (id, now) in &sorted {
            let was = self.statuses.iter().find(|(p, _)| p == id).map(|(_, s)| *s);
            if was == Some(Status::Working) && *now == Status::Idle {
                self.report_to_parent(*id, cx);
            }
        }
        if self.settings.notify_attention {
            for (id, body) in ask {
                self.post(id, body, cx);
            }
        }
        if self.settings.notify_done {
            for (id, body) in done {
                self.post(id, body, cx);
            }
        }
        let wanting = sorted
            .iter()
            .filter(|(_, s)| *s == Status::Attention)
            .count();
        if wanting != self.badge {
            self.badge = wanting;
            set_badge(wanting);
        }
        // Repaint when a state changed, and every second while something is
        // working, so its timer moves.
        let working = sorted.iter().any(|(_, s)| *s == Status::Working);
        if sorted != self.statuses || working {
            self.statuses = sorted;
            cx.notify();
        }
        // A pane is first touched when it opens; a chat keeps its own time.
        for (id, pane) in &self.panes {
            if !matches!(pane.kind, PaneKind::Chat(_)) {
                self.touched.entry(*id).or_insert(stamp);
            }
        }
        self.settle(cx);
        // The sidebar's "5m"s move on.
        if self.sidebar && self.ticks.is_multiple_of(30) {
            cx.notify();
        }
        if window.is_window_active() && self.ticks.is_multiple_of(2) {
            self.refresh_inspector(cx);
        }
        if self.ticks.is_multiple_of(2) || !self.settings.status_bar {
            self.refresh_servers(cx);
        }
        if self.settings.status_bar {
            self.refresh_bar_git(cx);
        }
        // The status bar's meter and the sidebar's Usage row read these.
        // The conversation's numbers are read incrementally (about a
        // millisecond) and limits are cached a couple of minutes below, so
        // every few seconds keeps ctx in step with Claude Code's own line;
        // the open panel keeps them fresher.
        if self.settings.status_bar || self.sidebar || self.usage_open {
            let every = if self.usage_open { 3 } else { 5 };
            if self.ticks % every == 1 {
                self.refresh_usage(cx);
            }
        }
        self.refresh_places(cx);
        self.save_session(cx);
        if self.ticks.is_multiple_of(3) {
            self.refresh_task_changes(cx);
            self.unstick_agents(cx);
        }
    }

    /// The project each terminal and chat is in, and what a terminal is
    /// running, for the tab titles. libproc only, and off the main thread:
    /// walking every terminal's processes each second showed up as a stall.
    fn refresh_places(&mut self, cx: &mut Context<Self>) {
        if self.placing {
            return;
        }
        let jobs: Vec<(PaneId, PlaceJob)> = self
            .panes
            .iter()
            .filter_map(|(&id, pane)| match &pane.kind {
                PaneKind::Terminal(t) => {
                    let t = t.read(cx);
                    Some((id, PlaceJob::Shell(t.pid(), t.shell_cwd())))
                }
                PaneKind::Chat(c) => Some((id, PlaceJob::Dir(c.read(cx).cwd().to_path_buf()))),
                _ => None,
            })
            .collect();
        self.placing = true;
        let scan = cx
            .background_executor()
            .spawn(async move { jobs.into_iter().filter_map(|(id, job)| Some((id, job.place()?))).collect() });
        cx.spawn(async move |this, cx| {
            let next: HashMap<PaneId, Place> = scan.await;
            let _ = this.update(cx, |ws, cx| {
                ws.placing = false;
                ws.apply_places(next, cx);
            });
        })
        .detach();
    }

    fn apply_places(&mut self, next: HashMap<PaneId, Place>, cx: &mut Context<Self>) {
        // Busy: something other than the shell is in front and printing.
        let shell = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_string))
            .unwrap_or_else(|| "zsh".into());
        let busy: std::collections::HashSet<PaneId> = self
            .panes
            .iter()
            .filter(|(id, p)| match &p.kind {
                // What the program says beats a guess: OSC 9;4 progress, or
                // Claude Code's title. Its screen keeps changing while idle
                // (the status line), so recent output alone is not work.
                PaneKind::Terminal(t) => match t.read(cx).activity() {
                    Some(working) => working,
                    None => {
                        t.read(cx).last_output.elapsed() < Duration::from_millis(1500)
                            && next
                                .get(id)
                                .and_then(|pl| pl.front.as_deref())
                                .is_some_and(|f| f != shell && !f.starts_with('-'))
                    }
                },
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        if next != self.places || busy != self.busy {
            self.places = next;
            self.busy = busy;
            cx.notify();
        }
        self.read_conv_titles(cx);
    }

    /// First asks of the Claude Code conversations in our terminals that
    /// are not read yet, off the main thread.
    fn read_conv_titles(&mut self, cx: &mut Context<Self>) {
        let wanted: Vec<(String, PathBuf)> = self
            .places
            .values()
            .filter_map(|p| {
                let (cmd, argv) = p.agent.as_ref()?;
                (*cmd == "claude").then_some(())?;
                let id = crate::session::session_arg(argv)?;
                (!self.conv_titles.contains_key(&id)).then(|| (id, p.cwd.clone()))
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        for (id, _) in &wanted {
            self.conv_titles.insert(id.clone(), None);
        }
        let read = cx.background_executor().spawn(async move {
            wanted
                .into_iter()
                .map(|(id, cwd)| {
                    let titles = crate::session::claude_titles(&cwd, &id);
                    (id, titles)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let found = read.await;
            let _ = this.update(cx, |ws, cx| {
                for (id, titles) in found {
                    ws.conv_titles.insert(id, Some(titles));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A Claude Code terminal's title, unless Claude Code gave the same one
    /// to other terminals too (it has retitled every open session alike):
    /// then the conversation's own first ask.
    fn own_agent_title(&self, id: PaneId, title: String, cx: &App) -> String {
        // The command line it was started with is no title either.
        let command = AGENTS.iter().any(|a| title.starts_with(&format!("{} ", a.cmd)));
        let shared = command
            || self.panes.iter().any(|(&other, p)| {
                other != id
                    && matches!(&p.kind, PaneKind::Terminal(t) if clean_title(&t.read(cx).title).as_deref() == Some(title.as_str()))
            });
        if !shared {
            return title;
        }
        self.places
            .get(&id)
            .and_then(|p| p.agent.as_ref())
            .and_then(|(_, argv)| crate::session::session_arg(argv))
            .and_then(|sid| self.conv_titles.get(&sid)?.as_ref()?.pick(&title))
            .unwrap_or(title)
    }

    /// The sidebar and inspector islands, made on first use.
    fn islands(&mut self, cx: &mut Context<Self>) -> (Entity<Island>, Entity<Island>) {
        if let Some(i) = &self.islands {
            return i.clone();
        }
        let ws = cx.entity();
        let sidebar = Island::new(&ws, |ws, _, cx| ws.render_sidebar(cx), cx);
        let inspector = Island::new(
            &ws,
            |ws, _, cx| {
                let title = ws
                    .focused()
                    .map(|id| ws.pane_title(id, cx))
                    .unwrap_or_default();
                inspector::render(ws, title, cx)
            },
            cx,
        );
        self.islands = Some((sidebar.clone(), inspector.clone()));
        (sidebar, inspector)
    }

    /// Dev servers for any blank browser tab on screen: every port a process
    /// under one of our terminals listens on. libproc only, no spawning.
    fn refresh_servers(&mut self, cx: &mut Context<Self>) {
        let blank = self
            .browser_in_tab(self.active)
            .filter(|(_, b)| b.read(cx).is_blank())
            .map(|(_, b)| b);
        if blank.is_none() && !self.settings.status_bar {
            return;
        }
        let pids: Vec<u32> = self
            .panes
            .values()
            .filter_map(|p| match &p.kind {
                PaneKind::Terminal(t) => Some(t.read(cx).pid()),
                _ => None,
            })
            .flat_map(|pid| inspect::descendants(pid).into_iter().map(|p| p.pid))
            .collect();
        let mut servers: Vec<(u16, String)> = inspect::listening_ports(&pids)
            .into_iter()
            .filter(|p| dev_server(p.port, &p.process))
            .map(|p| (p.port, p.process))
            .collect();
        servers.dedup_by_key(|(port, _)| *port);
        // Ask the ones not asked yet, off the main thread; show only those
        // that speak HTTP (a daemon or language server does not).
        let ask: Vec<(u16, String)> = servers.iter().filter(|k| !self.port_web.contains_key(*k)).cloned().collect();
        if !ask.is_empty() {
            for k in &ask {
                self.port_web.insert(k.clone(), None);
            }
            let probe = cx.background_executor().spawn(async move {
                ask.into_iter().map(|k| {
                    let web = speaks_http(k.0);
                    (k, web)
                }).collect::<Vec<_>>()
            });
            cx.spawn(async move |this, cx| {
                let found = probe.await;
                let _ = this.update(cx, |ws, cx| {
                    for (k, web) in found {
                        ws.port_web.insert(k, Some(web));
                    }
                    ws.refresh_servers(cx);
                });
            })
            .detach();
        }
        if self.port_web.len() > 256 {
            self.port_web.retain(|k, _| servers.contains(k));
        }
        servers.retain(|k| self.port_web.get(k) == Some(&Some(true)));
        if self.bar_ports != servers {
            self.bar_ports = servers.clone();
            cx.notify();
        }
        if let Some(b) = blank {
            b.update(cx, |b, cx| {
                if b.servers != servers {
                    b.servers = servers;
                    cx.notify();
                }
            });
        }
    }

    /// The agent in the focused pane: its command, and for Claude Code the
    /// folder and conversation it is in.
    fn focused_agent(&self, cx: &App) -> Option<(&'static str, Option<Conversation>)> {
        let id = self.focused()?;
        match &self.panes.get(&id)?.kind {
            PaneKind::Chat(c) => {
                let c = c.read(cx);
                let cmd = AGENTS.iter().find(|a| a.cmd == c.agent.as_str())?.cmd;
                let conv = (cmd == "claude").then(|| (c.cwd().to_path_buf(), c.native_id().map(str::to_string)));
                Some((cmd, conv))
            }
            PaneKind::Terminal(_) => {
                let place = self.places.get(&id)?;
                let (cmd, argv) = place.agent.as_ref()?;
                let conv = (*cmd == "claude").then(|| (place.cwd.clone(), crate::session::session_arg(argv)));
                Some((cmd, conv))
            }
            _ => None,
        }
    }

    /// Plan limits, and the focused conversation's numbers, off the main
    /// thread; today's totals only while the panel is open (they read every
    /// conversation written today).
    fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        if self.usage.busy {
            return;
        }
        self.usage.busy = true;
        let conv = self.focused_agent(cx).and_then(|(_, conv)| conv);
        let title = self.focused().map(|id| self.pane_title(id, cx)).unwrap_or_default();
        let today = self.usage_open;
        // Their records are read only when someone is looking at them.
        let agent = self.focused_agent(cx).map(|(cmd, _)| cmd);
        let others = today || matches!(agent, Some("opencode" | "gemini"));
        let work = cx.background_executor().spawn(async move {
            let claude = crate::usage::claude_limits();
            let codex = crate::usage::codex_limits();
            // Kept by day, for the Usage page's limit history.
            crate::usage::record_limits(claude.as_ref().ok(), codex.as_ref());
            let others: Option<Vec<_>> = others.then(|| {
                [crate::usage::opencode_today(), crate::usage::gemini_today()]
                    .into_iter()
                    .flatten()
                    .collect()
            });
            let session = conv.and_then(|(cwd, id)| {
                let id = id.or_else(|| crate::usage::claude_latest_session(&cwd))?;
                crate::usage::claude_session(&cwd, &id)
            });
            let today = today.then(crate::usage::claude_today);
            (claude, codex, session, today, others)
        });
        cx.spawn(async move |this, cx| {
            let (claude, codex, session, today, others) = work.await;
            let _ = this.update(cx, |ws, cx| {
                let u = &mut ws.usage;
                u.busy = false;
                u.at = Some(Instant::now());
                u.claude = Some(claude);
                u.codex = codex;
                u.session = session.map(|s| (title, s));
                if today.is_some() {
                    u.today = today;
                }
                if let Some(o) = others {
                    u.others = o;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The meter at the status bar's end: the plan limit nearest its cap
    /// for the agent you are in (Claude unless it is Codex), and how full
    /// the conversation's context is. A click opens the panel.
    fn render_usage_meter(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let agent = self.focused_agent(cx).map(|(cmd, _)| cmd);
        let limits = match agent {
            Some("codex") => self.usage.codex.as_ref(),
            _ => self.usage.claude.as_ref().and_then(|r| r.as_ref().ok()),
        };
        // The 5-hour limit first, as Claude Code's "used" shows it; a
        // longer one joins it only when it is the one getting close.
        let five = limits.and_then(|l| l.limits.iter().find(|x| x.label.starts_with('5')));
        let top = five.or_else(|| limits.and_then(|l| l.limits.iter().max_by(|a, b| a.used_pct.total_cmp(&b.used_pct))));
        let tight = limits.and_then(|l| {
            l.limits
                .iter()
                .filter(|x| Some(*x) != top && x.used_pct >= 80.)
                .max_by(|a, b| a.used_pct.total_cmp(&b.used_pct))
        });
        let ctx = self
            .usage
            .session
            .as_ref()
            .filter(|_| agent == Some("claude"))
            .map(|(_, s)| s.context_used as f32 / s.context_window.max(1) as f32 * 100.);
        // OpenCode and Gemini CLI have no plan limits to show: their day.
        let day = agent
            .and_then(|a| self.usage.others.iter().find(|d| d.agent == a))
            .filter(|_| top.is_none());
        if top.is_none() && ctx.is_none() && day.is_none() {
            return None;
        }
        let glyph = Icon::agent(match agent {
            Some(a @ ("codex" | "opencode" | "gemini")) => a,
            _ => "claude",
        });
        let on = self.usage_open;
        Some(
            div()
                .id("usage-meter")
                .flex()
                .flex_none()
                .items_center()
                .gap(u(8.))
                .h(u(20.))
                .px(u(6.))
                .rounded(u(5.))
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                .when(!on, |d| d.hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text()))))
                .child(icon(glyph, 12., theme::dim()))
                .children(top.map(|l| {
                    div()
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .child(short_limit(&l.label))
                        .child(meter_bar(l.used_pct, 28.))
                        .child(div().text_color(rgb(level_color(l.used_pct))).child(pct_text(l.used_pct)))
                        // When it frees up, in full colour so it reads at a
                        // glance: "◷ resets in 2h 14m".
                        .children(l.resets_at.and_then(until).map(|t| {
                            div()
                                .flex()
                                .items_center()
                                .gap(u(4.))
                                .ml(u(4.))
                                .child(icon(Icon::Clock, 11., theme::text_2()))
                                .child(div().text_color(rgb(theme::text())).child(format!("resets in {t}")))
                        }))
                }))
                .children(tight.map(|l| {
                    div()
                        .flex()
                        .items_center()
                        .gap(u(4.))
                        .child(short_limit(&l.label))
                        .child(div().text_color(rgb(level_color(l.used_pct))).child(pct_text(l.used_pct)))
                }))
                .children(day.map(|d| {
                    div()
                        .flex()
                        .items_center()
                        .gap(u(4.))
                        .child(div().text_color(rgb(theme::dim())).child("today"))
                        .child(tokens(d.tokens))
                        .when(d.cost_usd >= 0.01, |e| {
                            e.child(div().text_color(rgb(theme::dim())).child(format!("· ${:.2}", d.cost_usd)))
                        })
                }))
                .children(ctx.map(|pct| {
                    div()
                        .flex()
                        .items_center()
                        .gap(u(4.))
                        .child(div().text_color(rgb(theme::dim())).child("ctx"))
                        .child(div().text_color(rgb(level_color(pct))).child(pct_text(pct)))
                }))
                .on_click(cx.listener(|ws, _, _, cx| ws.toggle_usage(false, cx)))
                .into_any_element(),
        )
    }

    /// Open the usage panel over the status bar's meter or over the
    /// sidebar's Usage row, or shut it.
    /// The usage numbers at a glance, over the sidebar's gauge on hover:
    /// the plan's windows, the context, today's spend. No actions.
    fn render_usage_card(&self) -> AnyElement {
        let us = &self.usage;
        let line = |k: String, pct: Option<f32>, v: String| {
            div()
                .flex()
                .items_center()
                .gap(u(8.))
                .h(u(20.))
                .child(div().flex_1().text_color(rgb(theme::text_2())).child(k))
                .children(pct.map(|p| meter_bar(p, 48.)))
                .child(div().min_w(u(40.)).flex().justify_end().text_color(rgb(pct.map_or(theme::text(), level_color))).child(v))
        };
        let mut col = div()
            .absolute()
            .bottom(u(32.))
            .left_0()
            .w(u(240.))
            .p(u(10.))
            .flex()
            .flex_col()
            .gap(u(2.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .text_size(u(12.));
        if let Some(Ok(l)) = &us.claude {
            for lim in &l.limits {
                col = col.child(line(format!("Claude {}", lim.label), Some(lim.used_pct), pct_text(lim.used_pct)));
            }
        }
        if let Some(l) = &us.codex {
            for lim in &l.limits {
                col = col.child(line(format!("Codex {}", lim.label), Some(lim.used_pct), pct_text(lim.used_pct)));
            }
        }
        if let Some((_, s)) = &us.session {
            let pct = s.context_used as f32 / s.context_window.max(1) as f32 * 100.;
            col = col.child(line("Context".into(), Some(pct), pct_text(pct)));
        }
        if let Some(t) = &us.today {
            col = col.child(line(
                "Today".into(),
                None,
                t.cost_usd.map_or_else(|| tokens(t.output + t.input), |c| format!("${c:.2}")),
            ));
        }
        col.child(div().pt(u(6.)).text_size(u(11.)).text_color(rgb(theme::dim())).child("Click for details"))
            .into_any_element()
    }

    fn toggle_usage(&mut self, by_sidebar: bool, cx: &mut Context<Self>) {
        let again = self.usage_open && self.usage_by_sidebar == by_sidebar;
        self.usage_open = !again;
        self.usage_by_sidebar = by_sidebar;
        if self.usage_open {
            self.refresh_usage(cx);
        }
        cx.notify();
    }

    /// The focused chat, when it is one, as its agent reported it: the
    /// usage panel's conversation for any agent whose transcript deck
    /// cannot read itself.
    fn focused_chat_usage(&self, cx: &App) -> Option<ChatUse> {
        let id = self.focused()?;
        let PaneKind::Chat(c) = &self.panes.get(&id)?.kind else {
            return None;
        };
        let c = c.read(cx);
        let usage = *c.usage();
        (usage.tokens() > 0 || usage.context > 0).then(|| ChatUse {
            pane: id,
            title: self.pane_title(id, cx),
            mark: Icon::agent(c.agent.as_str()),
            agent: c.agent_name(),
            model: c.ran_model().map(crate::usage::model_name),
            usage,
        })
    }

    /// Every chat in this window that has spent anything, the heaviest
    /// first, for the panel's list.
    fn chat_spend(&self, cx: &App) -> Vec<ChatUse> {
        let mut out: Vec<ChatUse> = self
            .panes
            .iter()
            .filter_map(|(&id, p)| {
                let PaneKind::Chat(c) = &p.kind else {
                    return None;
                };
                let c = c.read(cx);
                let usage = *c.usage();
                (usage.tokens() > 0).then(|| ChatUse {
                    pane: id,
                    title: self.pane_title(id, cx),
                    mark: Icon::agent(c.agent.as_str()),
                    agent: c.agent_name(),
                    model: c.ran_model().map(crate::usage::model_name),
                    usage,
                })
            })
            .collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.usage.tokens()));
        out
    }

    /// What the sidebar's Usage row says shut: how full the focused
    /// conversation's context is, else the plan limit nearest its cap for
    /// the agent you are in, as the status bar's meter reads them.
    fn usage_glance(&self, cx: &App) -> Option<(SharedString, f32)> {
        let agent = self.focused_agent(cx).map(|(cmd, _)| cmd);
        let ctx = self
            .usage
            .session
            .as_ref()
            .filter(|_| agent == Some("claude"))
            .map(|(_, s)| s.context_used as f32 / s.context_window.max(1) as f32 * 100.)
            .or_else(|| {
                self.focused_chat_usage(cx)
                    .and_then(|c| c.usage.context_used())
                    .map(|f| f * 100.)
            });
        if let Some(pct) = ctx {
            return Some(("ctx".into(), pct));
        }
        let limits = match agent {
            Some("codex") => self.usage.codex.as_ref(),
            _ => self.usage.claude.as_ref().and_then(|r| r.as_ref().ok()),
        }?;
        let five = limits.limits.iter().find(|x| x.label.starts_with('5'));
        let top = five.or_else(|| limits.limits.iter().max_by(|a, b| a.used_pct.total_cmp(&b.used_pct)))?;
        Some((short_limit(&top.label), top.used_pct))
    }

    /// One of Deck's own pages at the foot of the sidebar: opens its tab,
    /// lifted like an open row while it is the one you are in.
    #[allow(dead_code)]
    fn render_page_row(&self, kind: PageKind, glyph: Icon, cx: &mut Context<Self>) -> AnyElement {
        let active = self
            .focused()
            .is_some_and(|id| matches!(self.panes.get(&id).map(|p| &p.kind), Some(PaneKind::Page(_, k)) if *k == kind));
        let tone = if active { theme::text() } else { theme::text_2() };
        let group: SharedString = format!("page-row-{}", kind.title()).into();
        let key = match kind {
            PageKind::Usage => "usage",
            PageKind::Packages => "packages",
        };
        sidebar_row(div().id(ElementId::Name(group.clone())), 0., active, false)
            .group(group.clone())
            .h(u(ROW_ONE_LINE))
            .pr(u(ROW_END + 3.))
            .text_size(u(13.))
            .text_color(rgb(tone))
            .child(lead(icon(glyph, 15., tone)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .group_hover(group.clone(), |s| s.text_color(rgb(theme::text())))
                    .child(kind.title()),
            )
            .children(keycap(hint(key, &self.settings.keys)).map(|k| k.invisible().group_hover(group, |s| s.visible())))
            .on_click(cx.listener(move |ws, _, w, cx| ws.open_page(kind, w, cx)))
            .into_any_element()
    }

    /// Usage at the foot of the sidebar, above Settings: shut, how full the
    /// conversation's context is (or the plan's 5-hour window); a click
    /// opens the usage panel over it, with the context, the tokens and
    /// what is left of the plan.
    #[allow(dead_code)]
    fn render_usage_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = self.usage_open && self.usage_by_sidebar;
        let tone = if on { theme::text() } else { theme::text_2() };
        // Lifted like the open row while its panel is up.
        sidebar_row(div().id("open-usage"), 0., on, false)
            .group("usage-row")
            .h(u(ROW_ONE_LINE))
            .pr(u(ROW_END + 3.))
            .text_size(u(13.))
            .text_color(rgb(tone))
            .child(lead(icon(Icon::Gauge, 15., tone)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .group_hover("usage-row", |s| s.text_color(rgb(theme::text())))
                    .child("Usage"),
            )
            .children(self.usage_glance(cx).map(|(label, pct)| {
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(11.))
                    .child(div().text_color(rgb(theme::dim())).child(label))
                    .child(meter_bar(pct, 28.))
                    .child(div().text_color(rgb(level_color(pct))).child(pct_text(pct)))
            }))
            .on_click(cx.listener(|ws, _, _, cx| ws.toggle_usage(true, cx)))
            .into_any_element()
    }

    /// The usage panel over the status bar's end (or the sidebar's Usage
    /// row): four tabs, Limits, This chat, Chats and Today, switched by
    /// hovering or clicking. One height for all four, so switching never
    /// makes it jump; the indicator slides, the content cross-fades.
    fn render_usage_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        const W: f32 = 340.;
        const PAD: f32 = 14.;
        const BODY_H: f32 = 330.;
        let us = &self.usage;
        let label = |text: &str| {
            div()
                .pb(u(6.))
                .text_size(u(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::dim()))
                .child(text.to_string())
        };
        let meter_row = |name: SharedString, pct: f32, note: Option<String>| {
            div()
                .flex()
                .flex_col()
                .gap(u(5.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .child(div().flex_1().text_color(rgb(theme::text())).child(name))
                        .children(note.map(|n| div().text_size(u(11.)).text_color(rgb(theme::dim())).child(n)))
                        .child(div().w(u(38.)).flex().justify_end().text_color(rgb(level_color(pct))).child(pct_text(pct))),
                )
                .child(meter_bar(pct, 0.))
        };
        let agent_head = |mark: Icon, name: &str, plan: Option<String>| {
            div()
                .flex()
                .items_center()
                .gap(u(8.))
                .child(icon(mark, 14., mark.brand().unwrap_or(theme::text())))
                .child(div().flex_1().font_weight(FontWeight::MEDIUM).text_color(rgb(theme::text())).child(name.to_string()))
                .children(plan.map(|p| div().text_size(u(11.)).text_color(rgb(theme::dim())).child(p)))
        };
        let limits_block = |l: &crate::usage::Limits, name: &str| {
            let mut col = div().flex().flex_col().gap(u(10.)).child(agent_head(Icon::agent(l.agent), name, l.plan.clone()));
            for lim in &l.limits {
                col = col.child(meter_row(lim.label.clone().into(), lim.used_pct, lim.resets_at.map(resets_in)));
            }
            col
        };
        let stat = |k: &str, v: String| {
            div()
                .flex()
                .flex_col()
                .gap(u(2.))
                .flex_1()
                .child(div().text_size(u(11.)).text_color(rgb(theme::dim())).child(k.to_string()))
                .child(div().text_size(u(14.)).font_weight(FontWeight::MEDIUM).text_color(rgb(theme::text())).child(v))
        };
        let empty = |text: &str| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::dim()))
                .child(text.to_string())
        };

        // Limits: each agent's plan windows.
        let mut limits = div().flex().flex_col().gap(u(20.));
        match &us.claude {
            Some(Ok(l)) => limits = limits.child(limits_block(l, "Claude")),
            Some(Err(e)) => limits = limits.child(agent_head(Icon::Claude, "Claude", None)).child(div().text_color(rgb(theme::text_2())).child(e.clone())),
            None => limits = limits.child(div().text_color(rgb(theme::dim())).child("Reading usage…")),
        }
        if let Some(l) = &us.codex {
            limits = limits.child(limits_block(l, "Codex"));
        }
        // Agents without plan limits (OpenCode, Gemini CLI, Grok…): what
        // they used today, so every installed agent is on this tab.
        for a in self.installed.iter().filter(|a| !matches!(a.cmd, "claude" | "codex")) {
            let mark = Icon::agent(a.cmd);
            let day = us.others.iter().find(|d| d.agent == a.cmd);
            let note = match day {
                Some(d) if d.cost_usd >= 0.01 => format!("Today {} replies · {} · ${:.2}", d.replies, tokens(d.tokens), d.cost_usd),
                Some(d) => format!("Today {} replies · {} tokens", d.replies, tokens(d.tokens)),
                None => "Nothing used today".to_string(),
            };
            limits = limits.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(4.))
                    .child(agent_head(mark, a.name, Some("No plan limits".into())))
                    .child(div().pl(u(22.)).text_size(u(11.5)).text_color(rgb(theme::dim())).child(note)),
            );
        }

        // This chat: the conversation you are in.
        let reported = us.session.is_none().then(|| self.focused_chat_usage(cx)).flatten();
        let chat: AnyElement = if let Some((title, s)) = &us.session {
            let pct = s.context_used as f32 / s.context_window.max(1) as f32 * 100.;
            div()
                .flex()
                .flex_col()
                .gap(u(18.))
                .child(div().one_line().font_weight(FontWeight::MEDIUM).text_color(rgb(theme::text())).child(title.clone()))
                .child(meter_row(
                    "Context".into(),
                    pct,
                    Some(format!(
                        "{} of {}{}",
                        tokens(s.context_used),
                        tokens(s.context_window),
                        s.model.as_deref().map(|m| format!(" · {}", crate::usage::model_name(m))).unwrap_or_default()
                    )),
                ))
                .child(div().flex().gap(u(12.)).child(stat("Turns", s.turns.to_string())).child(stat("Cost", s.cost_usd.map_or("—".into(), |c| format!("${c:.2}")))))
                .child(
                    div()
                        .flex()
                        .gap(u(12.))
                        .child(stat("In", tokens(s.input)))
                        .child(stat("Out", tokens(s.output)))
                        .child(stat("Cached", tokens(s.cache_read))),
                )
                .into_any_element()
        } else if let Some(c) = &reported {
            let u_ = &c.usage;
            let who = match &c.model {
                Some(m) => format!("{} · {m}", c.agent),
                None => c.agent.to_string(),
            };
            let mut col = div()
                .flex()
                .flex_col()
                .gap(u(18.))
                .child(div().one_line().font_weight(FontWeight::MEDIUM).text_color(rgb(theme::text())).child(c.title.clone()));
            col = match (u_.context_used(), u_.context_window) {
                (Some(f), Some(w)) => col.child(meter_row("Context".into(), f * 100., Some(format!("{} of {} · {who}", tokens(u_.context), tokens(w))))),
                _ => col.child(div().text_color(rgb(theme::text_2())).child(who)),
            };
            col.child(
                div()
                    .flex()
                    .gap(u(12.))
                    .child(stat("In", tokens(u_.input)))
                    .child(stat("Out", tokens(u_.output)))
                    .child(stat("Cached", tokens(u_.cache_read + u_.cache_write)))
                    .child(stat("Cost", u_.cost_usd.filter(|c| *c > 0.).map_or("—".into(), |c| format!("${c:.2}")))),
            )
            .into_any_element()
        } else {
            empty("Open a chat to see its context and spend").into_any_element()
        };

        // Chats: every chat in this window by what it spent.
        let spend = self.chat_spend(cx);
        let focused = self.focused();
        let chats: AnyElement = if spend.is_empty() {
            empty("No chats have spent anything yet").into_any_element()
        } else {
            let mut list = div().flex().flex_col().gap(u(1.)).mx(u(-6.));
            for c in spend.iter().take(8) {
                let pane = c.pane;
                let figure = match c.usage.cost_usd.filter(|v| *v >= 0.01) {
                    Some(v) => format!("${v:.2}"),
                    None => tokens(c.usage.tokens()),
                };
                list = list.child(
                    div()
                        .id(ElementId::NamedInteger("usage-chat".into(), pane))
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .h(u(36.))
                        .px(u(6.))
                        .rounded(u(7.))
                        .cursor_pointer()
                        .when(focused == Some(pane), |d| d.bg(rgb(theme::surface())))
                        .when(focused != Some(pane), |d| d.hover(|s| s.bg(rgb(theme::row_hover()))))
                        .child(icon(c.mark, 13., c.mark.brand().unwrap_or(theme::text_2())))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(div().w_full().one_line().text_color(rgb(theme::text())).child(c.title.clone()))
                                .child(div().w_full().one_line().text_size(u(10.5)).text_color(rgb(theme::dim())).child(match &c.model {
                                    Some(m) => format!("{} · {m}", c.agent),
                                    None => c.agent.to_string(),
                                })),
                        )
                        .child(div().flex_none().text_size(u(11.5)).text_color(rgb(theme::text_2())).child(figure))
                        .on_click(cx.listener(move |ws, _, window, cx| {
                            ws.usage_open = false;
                            ws.focus_pane(pane, window, cx);
                            cx.notify();
                        })),
                );
            }
            list.into_any_element()
        };

        // Today: Claude Code across projects, and agents with no limits.
        let mut today = div().flex().flex_col().gap(u(18.));
        if let Some(t) = &us.today {
            today = today
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(u(10.))
                        .child(agent_head(Icon::Claude, "Claude Code", Some(format!("{} conversations", t.sessions))))
                        .child(
                            div()
                                .flex()
                                .gap(u(12.))
                                .child(stat("Cost", t.cost_usd.map_or("—".into(), |c| format!("${c:.2}"))))
                                .child(stat("Written", tokens(t.output)))
                                .child(stat("New in", tokens(t.input + t.cache_write)))
                                .child(stat("Cached", tokens(t.cache_read))),
                        ),
                );
        }
        for d in &us.others {
            let name = AGENTS.iter().find(|a| a.cmd == d.agent).map_or(d.agent, |a| a.name);
            today = today.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(10.))
                    .child(agent_head(Icon::agent(d.agent), name, d.model.clone().map(|m| crate::usage::model_name(&m))))
                    .child(
                        div()
                            .flex()
                            .gap(u(12.))
                            .child(stat("Cost", if d.cost_usd >= 0.01 { format!("${:.2}", d.cost_usd) } else { "—".into() }))
                            .child(stat("Replies", d.replies.to_string()))
                            .child(stat("Tokens", tokens(d.tokens))),
                    ),
            );
        }
        if us.today.is_none() && us.others.is_empty() {
            today = today.child(empty("Nothing used yet today"));
        }

        let tab = self.usage_tab.min(3);
        let prev = self.usage_tab_prev.min(3);
        let names = ["Limits", "This chat", "Chats", "Today"];
        let tab_w = (W - PAD * 2. - 4.) / 4.;
        let mut tabs = div()
            .relative()
            .flex()
            .h(u(28.))
            .p(u(2.))
            .rounded(u(8.))
            .bg(rgba(theme::tint()))
            // The indicator under the tabs, sliding from the last tab to
            // this one.
            .child(
                div()
                    .absolute()
                    .top(u(2.))
                    .h(u(24.))
                    .w(u(tab_w))
                    .rounded(u(6.))
                    .bg(rgb(theme::row_active()))
                    .border_1()
                    .border_color(rgba(theme::hairline()))
                    .with_animation(
                        ElementId::NamedInteger("usage-ind".into(), (prev * 4 + tab) as u64 + self.usage_tab_moves * 16),
                        Animation::new(Duration::from_millis(150)).with_easing(gpui::ease_out_quint()),
                        move |d, t| d.left(u(2. + (prev as f32 + (tab as f32 - prev as f32) * t) * tab_w)),
                    ),
            );
        for (i, name) in names.iter().enumerate() {
            let on = i == tab;
            tabs = tabs.child(
                div()
                    .id(ElementId::NamedInteger("usage-tab".into(), i as u64))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(u(tab_w))
                    .h(u(24.))
                    .text_size(u(12.))
                    .cursor_pointer()
                    .text_color(rgb(if on { theme::text() } else { theme::text_2() }))
                    .when(on, |d| d.font_weight(FontWeight::MEDIUM))
                    .hover(|s| s.text_color(rgb(theme::text())))
                    .on_hover(cx.listener(move |ws, over: &bool, _, cx| {
                        if *over {
                            ws.set_usage_tab(i, cx);
                        }
                    }))
                    .on_click(cx.listener(move |ws, _, _, cx| ws.set_usage_tab(i, cx)))
                    .child(*name),
            );
        }
        let content: AnyElement = match tab {
            0 => limits.into_any_element(),
            1 => chat,
            2 => chats,
            _ => today.into_any_element(),
        };
        let body = div()
            .id("usage-body")
            .flex()
            .flex_col()
            .h(u(BODY_H))
            .pt(u(16.))
            .overflow_y_scroll()
            .child(div().flex().flex_col().min_h_full().child(label(names[tab])).child(content))
;

        let updated = us.at.map(|t| match t.elapsed().as_secs() {
            0..=9 => "Updated just now".to_string(),
            s if s < 120 => format!("Updated {s}s ago"),
            s => format!("Updated {}m ago", s / 60),
        });
        // Over the row that opened it: the sidebar's Usage row (above
        // Settings, under the list) or the status bar's meter.
        let bar = if self.settings.status_bar { 26. } else { 0. };
        let by_sidebar = self.usage_by_sidebar && self.sidebar;
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, _, _, cx| {
                    ws.usage_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("usage-panel")
                    .absolute()
                    .when(by_sidebar, |d| {
                        d.bottom(u(bar + theme::GUTTER + 34. + 6. + 6.)).left(u(theme::GUTTER))
                    })
                    .when(!by_sidebar, |d| d.bottom(u(bar + 6.)).right(u(theme::GUTTER)))
                    .w(u(W))
                    .flex()
                    .flex_col()
                    .p(u(PAD))
                    .rounded(u(12.))
                    .bg(rgb(theme::menu()))
                    .border_1()
                    .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                    .text_size(u(12.5))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(tabs)
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(4.))
                            .mt(u(10.))
                            .pt(u(10.))
                            .border_t_1()
                            .border_color(rgba(theme::hairline()))
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child(div().flex_1().child(updated.unwrap_or_default()))
                            .child(
                                div()
                                    .id("usage-open-page")
                                    .flex()
                                    .items_center()
                                    .gap(u(4.))
                                    .h(u(22.))
                                    .px(u(7.))
                                    .rounded(u(6.))
                                    .cursor_pointer()
                                    .text_color(rgb(theme::text_2()))
                                    .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                                    .child(icon(Icon::Chart, 12., theme::text_2()))
                                    .child("History")
                                    .on_click(cx.listener(|ws, _, w, cx| {
                                        ws.usage_open = false;
                                        ws.open_page(PageKind::Usage, w, cx);
                                    })),
                            )
                            .child(
                                icon_button("usage-refresh", Icon::Refresh)
                                    .size(u(22.))
                                    .on_click(cx.listener(|ws, _, _, cx| ws.refresh_usage(cx))),
                            ),
                    )
                    .with_animation(
                        "usage-panel-in",
                        Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                        |d, t| d.opacity(t).mb(u(-4. * (1. - t))),
                    ),
            )
            .into_any_element()
    }

    fn set_usage_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        if tab != self.usage_tab {
            self.usage_tab_prev = self.usage_tab;
            self.usage_tab = tab;
            self.usage_tab_moves += 1;
            cx.notify();
        }
    }

    /// The focused pane's project and branch, read from `.git` itself so it
    /// costs no process.
    fn refresh_bar_git(&mut self, cx: &mut Context<Self>) {
        // A pane with no folder (Settings, a page) keeps the last project.
        let Some(cwd) = self.focused_cwd(cx) else { return };
        let git = Some(cwd).map(|cwd| {
            let root = inspect::project_root(&cwd);
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string());
            (name, git_head(&root))
        });
        if git != self.bar_git {
            self.bar_git = git;
            cx.notify();
        }
    }

    pub fn refresh_inspector(&mut self, cx: &mut Context<Self>) {
        if !self.inspector.open || self.refreshing {
            return;
        }
        let Some(PaneKind::Terminal(t)) = self
            .focused()
            .and_then(|id| self.panes.get(&id))
            .map(|p| &p.kind)
        else {
            return;
        };
        let pid = t.read(cx).pid();
        let fallback = self.cwd.clone();
        let expanded: Vec<PathBuf> = self.inspector.expanded.iter().cloned().collect();
        // git status spawns a process: every 6s, or when the data is fresh.
        let previous = (self.inspector.data.root.is_some() && !self.ticks.is_multiple_of(6))
            .then(|| self.inspector.data.git.clone());
        self.refreshing = true;
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async move { inspector::collect(pid, fallback, expanded, previous) })
                .await;
            let _ = this.update(cx, |ws, cx| {
                ws.refreshing = false;
                ws.inspector.data = data;
                cx.notify();
            });
        })
        .detach();
    }

    /// The directory the focused terminal is in (its deepest process: the
    /// foreground job, or the shell), so a new terminal opens there.
    fn focused_cwd(&self, cx: &App) -> Option<PathBuf> {
        let tab = self.tabs.get(self.active)?;
        std::iter::once(tab.focused)
            .chain(tab.leaves())
            .find_map(|id| match &self.panes.get(&id)?.kind {
                PaneKind::Terminal(t) => {
                    let t = t.read(cx);
                    let place = self.places.get(&id);
                    // Where an agent stands, not the scratch folder one of
                    // its tools is running in; for a shell, where it says it
                    // is (OSC 7), which follows `cd` at once.
                    match place {
                        Some(p) if p.agent.is_some() => Some(p.cwd.clone()),
                        _ => t
                            .shell_cwd()
                            .or_else(|| place.map(|p| p.cwd.clone()))
                            .or_else(|| inspect::descendants(t.pid()).iter().rev().find_map(|p| inspect::cwd(p.pid))),
                    }
                }
                PaneKind::Launcher(l) => l.read(cx).cwd.clone(),
                PaneKind::Review(r) => Some(r.read(cx).task.worktree.clone()),
                PaneKind::Browser(_) | PaneKind::Chat(_) | PaneKind::Settings(_) | PaneKind::Page(..) => None,
            })
    }

    fn shell_here(&self, cx: &App) -> Open {
        Open::Terminal {
            cmd: None,
            cwd: self.focused_cwd(cx),
        }
    }

    /// ⌘+ / ⌘− / ⌘0: one zoom for the whole app (UI, terminals, pages),
    /// wherever the focus is, remembered across launches.
    fn zoom(&mut self, step: i32, window: &mut Window, cx: &mut Context<Self>) {
        let mut next = self.settings.clone();
        next.zoom = self.settings.stepped(step);
        self.apply_settings(next, window, cx);
    }

    /// Take new settings from anywhere (the Settings tab, ⌘+), save them, and
    /// push what changed to every open pane, so nothing waits for a relaunch.
    fn apply_settings(
        &mut self,
        next: crate::settings::Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if next == self.settings {
            return;
        }
        let zoomed = next.zoom != self.settings.zoom;
        let themed = next.theme != self.settings.theme;
        // Agents follow: Claude Code takes its ANSI theme, which draws
        // with the terminal palette Deck sets for this theme.
        if themed && next.match_agent_themes {
            let dark = next.theme.colors(crate::theme::is_dark()).dark;
            std::thread::spawn(move || crate::shell::match_agent_themes(dark));
        }
        if next.keys != self.settings.keys {
            rebind(cx, &next.keys);
        }
        crate::browser::set_search_engine(next.search);
        crate::refine::set(next.refine, &next.refine_model);
        crate::retry::set_enabled(next.auto_retry);
        if themed {
            wear(next.theme, window);
        }
        if next.agent_skill != self.settings.agent_skill {
            let on = next.agent_skill;
            std::thread::spawn(move || crate::shell::install_skill(on));
        }
        self.settings = next;
        crate::settings::save(&self.settings);
        let look = self.settings.term_look();
        // The editor page is zoomed as a whole; it takes the size before zoom.
        crate::code::set_font(&look.family, self.settings.font_px);
        let zoom = self.settings.zoom as f64;
        for pane in self.panes.values() {
            match &pane.kind {
                PaneKind::Terminal(t) => {
                    let look = look.clone();
                    t.update(cx, |t, cx| t.set_look(look, window, cx))
                }
                PaneKind::Browser(b) => b.update(cx, |b, _| {
                    b.set_zoom(zoom);
                    b.editor_restyle();
                }),
                PaneKind::Settings(s) => {
                    let settings = self.settings.clone();
                    s.update(cx, |s, cx| s.sync(&settings, None, cx))
                }
                PaneKind::Chat(_) | PaneKind::Launcher(_) | PaneKind::Review(_) | PaneKind::Page(..) => {}
            }
        }
        if zoomed || themed {
            // The UI follows through the rem size and the theme, both read
            // as it draws; cached panes would keep their old frame, so
            // everything is drawn once more.
            window.refresh();
        }
        cx.notify();
    }

    /// The theme changed under us (System, with macOS switching): the
    /// terminals' palette follows, then everything redraws.
    fn restyle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let look = self.settings.term_look();
        // The editor page is zoomed as a whole; it takes the size before zoom.
        crate::code::set_font(&look.family, self.settings.font_px);
        for pane in self.panes.values() {
            if let PaneKind::Terminal(t) = &pane.kind {
                let look = look.clone();
                t.update(cx, |t, cx| t.set_look(look, window, cx));
            }
        }
        window.refresh();
        cx.notify();
    }

    /// The agent a new chat starts with: the one chosen in Settings if it is
    /// ready, else Claude if it is, else the first agent that is.
    fn default_agent(&self) -> Option<agent_core::AgentId> {
        let env = self.chats.as_ref()?;
        self.settings
            .default_agent
            .as_deref()
            .map(agent_core::AgentId::new)
            .filter(|id| env.registry.get(id).is_some_and(|e| e.is_ready()))
            .or_else(|| env.default_agent())
    }

    /// Agents the Settings tab offers: those ready to chat right now.
    fn ready_agents(&self) -> Vec<(SharedString, SharedString)> {
        self.chats
            .as_ref()
            .map(|env| {
                env.registry
                    .ready()
                    .map(|e| {
                        (
                            SharedString::from(e.id().as_str().to_string()),
                            SharedString::from(e.display_name().to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn make_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (PaneKind, Option<String>, Subscription) {
        let agents = self.ready_agents();
        let settings = self.settings.clone();
        let s = cx.new(|cx| SettingsPane::new(settings, agents, window, cx));
        let sub = cx.subscribe_in(
            &s,
            window,
            |ws, _, ev: &SettingsEvent, window, cx| match ev {
                SettingsEvent::Changed(next) => ws.apply_settings(next.clone(), window, cx),
                SettingsEvent::DefaultTerminal(on) => ws.make_default_terminal(*on, window, cx),
                SettingsEvent::Account(a) => ws.account_action(*a, cx),
                SettingsEvent::Update(a) => ws.update_action(*a, cx),
            },
        );
        (PaneKind::Settings(s), None, sub)
    }

    // -- command palette -----------------------------------------------------

    /// ⌘K: open the palette over everything (pages step aside, since a
    /// native view would cover it), or close it if it is open.
    /// ⌘K, or with `snippets_only` ⌘;: the snippets alone.
    /// ⌃R at a terminal's prompt: what was run there, then the shell's
    /// history, newest first; the pick is typed at the prompt.
    fn open_history(&mut self, from: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        let Some(PaneKind::Terminal(t)) = self.panes.get(&from).map(|p| &p.kind) else {
            return;
        };
        let here = t.read(cx).session_commands();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut seen = std::collections::HashSet::new();
        let mut items: Vec<Item> = Vec::new();
        let mut push = |cmd: String, detail: String, items: &mut Vec<Item>| {
            if seen.insert(cmd.clone()) {
                items.push(Item {
                    kind: Kind::Command,
                    title: crate::chat::one_line_pub(&cmd).into(),
                    detail: detail.into(),
                    icon: Icon::Terminal,
                    hint: None,
                    pick: crate::palette::Pick::Command(cmd),
                });
            }
        };
        for cmd in here {
            push(cmd, "this terminal".into(), &mut items);
        }
        for (cmd, when) in crate::shell::zsh_history(3000) {
            let detail = when.map(|t| format!("{} ago", short_secs((now - t).max(0) as u64))).unwrap_or_default();
            push(cmd, detail, &mut items);
        }
        let palette = cx.new(|cx| Palette::history(items, cx));
        let sub = cx.subscribe_in(&palette, window, move |ws, _, ev: &PaletteEvent, window, cx| match ev {
            PaletteEvent::Close => ws.close_palette(window, cx),
            PaletteEvent::Run(pick) => {
                let pick = pick.clone();
                ws.close_palette(window, cx);
                ws.run_pick(pick, window, cx);
            }
            PaletteEvent::Query(_) => {}
        });
        let focus = palette.read(cx).focus(cx);
        self.palette = Some((palette, Some(from), sub));
        window.focus(&focus);
        crate::browser::reclaim_keyboard(window);
        cx.notify();
    }

    /// ⌃K (actions) or ⌘P (tabs): the palette narrowed to one kind. Its
    /// own key again closes it; another palette's key switches to that one.
    fn toggle_scoped_palette(&mut self, scope: crate::palette::Scope, window: &mut Window, cx: &mut Context<Self>) {
        use crate::palette::Kind;
        if self.palette.is_some() {
            let same = self.palette_scope == Some(scope);
            self.close_palette(window, cx);
            if same {
                return;
            }
        }
        let want = match scope {
            crate::palette::Scope::Actions => Kind::Action,
            crate::palette::Scope::Tabs => Kind::Tab,
            crate::palette::Scope::All => return self.toggle_palette(false, window, cx),
        };
        let from = self.focused();
        let items: Vec<Item> = self.palette_items(cx).into_iter().filter(|i| i.kind == want).collect();
        let palette = cx.new(|cx| Palette::scoped(items, scope, cx));
        let sub = cx.subscribe_in(&palette, window, move |ws, _, ev: &PaletteEvent, window, cx| match ev {
            PaletteEvent::Close => ws.close_palette(window, cx),
            PaletteEvent::Run(pick) => {
                let pick = pick.clone();
                ws.close_palette(window, cx);
                ws.run_pick(pick, window, cx);
            }
            PaletteEvent::Query(_) => {}
        });
        let focus = palette.read(cx).focus(cx);
        self.palette = Some((palette, from, sub));
        self.palette_scope = Some(scope);
        window.focus(&focus);
        crate::browser::reclaim_keyboard(window);
        cx.notify();
    }

    fn toggle_palette(&mut self, snippets_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            let same = self.palette_scope == Some(crate::palette::Scope::All) || snippets_only;
            self.close_palette(window, cx);
            if same {
                return;
            }
        }
        self.palette_scope = Some(crate::palette::Scope::All);
        let from = self.focused();
        let cwd = self.focused_cwd(cx).unwrap_or_else(|| self.cwd.clone());
        let in_repo = cwd.ancestors().any(|d| d.join(".git").exists());
        let palette = if snippets_only {
            let items = snippet_items(Some(&crate::inspect::project_root(&cwd)));
            cx.new(|cx| Palette::snippets(items, cx))
        } else {
            let items = self.palette_items(cx);
            cx.new(|cx| Palette::new(items, in_repo, cx))
        };
        let sub = cx.subscribe_in(
            &palette,
            window,
            move |ws, _, ev: &PaletteEvent, window, cx| match ev {
                PaletteEvent::Close => ws.close_palette(window, cx),
                PaletteEvent::Run(pick) => {
                    let pick = pick.clone();
                    ws.close_palette(window, cx);
                    ws.run_pick(pick, window, cx);
                }
                PaletteEvent::Query(_) if snippets_only => {}
                PaletteEvent::Query(q) => {
                    let pages = crate::history::search(q, 6)
                        .into_iter()
                        .map(|v| Item {
                            kind: Kind::Page,
                            title: if v.title.is_empty() {
                                crate::history::strip(&v.url).to_string().into()
                            } else {
                                v.title.clone().into()
                            },
                            detail: crate::history::strip(&v.url).to_string().into(),
                            icon: Icon::Globe,
                            hint: None,
                            pick: crate::palette::Pick::Page(v.url),
                        })
                        .collect();
                    if let Some((p, ..)) = &ws.palette {
                        p.update(cx, |p, cx| p.set_pages(pages, cx));
                    }
                }
            },
        );
        // The project's files arrive once git has listed them.
        let root = crate::inspect::project_root(&cwd);
        let weak = palette.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            if snippets_only {
                return;
            }
            let files = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { crate::chat::project_files_cached(&root) }
                })
                .await;
            let items: Vec<Item> = files
                .iter()
                .filter(|f| !f.ends_with('/'))
                .map(|f| {
                    let name = f.rsplit('/').next().unwrap_or(f).to_string();
                    let dir = f
                        .rsplit_once('/')
                        .map(|(d, _)| d.to_string())
                        .unwrap_or_default();
                    Item {
                        kind: Kind::File,
                        title: name.into(),
                        detail: dir.into(),
                        icon: Icon::File,
                        hint: None,
                        pick: crate::palette::Pick::File(root.join(f)),
                    }
                })
                .collect();
            let _ = weak.update(cx, |p, cx| p.add(items, cx));
        })
        .detach();
        let focus = palette.read(cx).focus(cx);
        self.palette = Some((palette, from, sub));
        window.focus(&focus);
        crate::browser::reclaim_keyboard(window);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_scope = None;
        if let Some((_, from, _)) = self.palette.take() {
            if let Some(id) = from.filter(|id| self.panes.contains_key(id)) {
                self.focus_pane(id, window, cx);
            }
            cx.notify();
        }
    }

    /// Pages are native views gpui cannot draw over: hide them while
    /// something floats over the window.
    fn cover_pages(&self, covered: bool, cx: &mut Context<Self>) {
        for pane in self.panes.values() {
            if let PaneKind::Browser(b) = &pane.kind {
                b.update(cx, |b, cx| b.set_covered(covered, cx));
            }
        }
    }

    fn palette_items(&self, cx: &App) -> Vec<Item> {
        use crate::palette::Pick as P;
        let mut items = Vec::new();
        for (ix, tab) in self.tabs.iter().enumerate() {
            let id = tab.focused;
            let (glyph, _) = self.tab_icon(tab.leaves()[0], cx);
            const HINTS: [&str; 9] = ["⌘1", "⌘2", "⌘3", "⌘4", "⌘5", "⌘6", "⌘7", "⌘8", "⌘9"];
            // A task's worktree folder is its brief as a slug; the project
            // it belongs to says more.
            let task = self.tasks.iter().find(|t| self.task_pane(t, cx).is_some_and(|p| tab.leaves().contains(&p)));
            let detail = match task {
                Some(t) => format!(
                    "{} · task",
                    t.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                )
                .into(),
                None => self.pane_subtitle(id, cx).unwrap_or_default(),
            };
            items.push(Item {
                kind: Kind::Tab,
                title: self.pane_title(id, cx),
                detail,
                icon: glyph,
                hint: HINTS.get(ix).map(|h| SharedString::from(*h)),
                pick: P::Pane(id),
            });
        }
        for t in self
            .tasks
            .iter()
            .filter(|t| t.state == crate::tasks::State::Open)
        {
            items.push(Item {
                kind: Kind::Action,
                title: format!("Review: {}", t.title).into(),
                detail: t.branch.clone().into(),
                icon: Icon::Git,
                hint: None,
                pick: P::Review(t.id.clone()),
            });
        }
        // Shortcuts as they are now, yours from Settings or Deck's.
        let key = |id: &str| Some(hint(id, &self.settings.keys)).filter(|h| !h.is_empty());
        let action = |title: &'static str,
                      detail: &'static str,
                      icon: Icon,
                      hint: Option<SharedString>,
                      a: Box<dyn gpui::Action>| Item {
            kind: Kind::Action,
            title: title.into(),
            detail: detail.into(),
            icon,
            hint,
            pick: P::Action(a),
        };
        items.extend([
            action(
                "New Tab",
                "A terminal, a page or an agent",
                Icon::Plus,
                key("new-tab"),
                Box::new(NewTab),
            ),
            action(
                "New Terminal",
                "",
                Icon::Terminal,
                key("new-terminal"),
                Box::new(NewTerminal),
            ),
            action(
                "New Chat",
                "With the agent new chats start with",
                Icon::Claude,
                key("new-chat"),
                Box::new(NewChat),
            ),
            action(
                "New Control Conversation",
                "Start the control chat over",
                Icon::Plus,
                None,
                Box::new(NewControl),
            ),
            action(
                "Restart Control Agent",
                "The same conversation, a fresh agent",
                Icon::Refresh,
                None,
                Box::new(RestartControl),
            ),
            action(
                "Insert Snippet",
                "A saved message, into the composer or terminal",
                Icon::Snippet,
                key("snippet"),
                Box::new(InsertSnippet),
            ),
            action(
                "Save as Snippet",
                "What is in the composer, or the terminal selection",
                Icon::Snippet,
                None,
                Box::new(SaveSnippet),
            ),
            action(
                "Edit Snippets",
                "Name, change or remove saved messages",
                Icon::Pencil,
                None,
                Box::new(EditSnippets),
            ),
            action(
                "New Browser Tab",
                "",
                Icon::Globe,
                key("new-browser"),
                Box::new(NewBrowser),
            ),
            action(
                "Files",
                "Browse and edit this project's files",
                Icon::Files,
                key("files"),
                Box::new(OpenFiles),
            ),
            action(
                "Git Changes",
                "Diffs, staging, commits and branches",
                Icon::Git,
                key("changes"),
                Box::new(OpenChanges),
            ),
            action(
                "Open Location",
                "The browser's address bar",
                Icon::Search,
                key("address"),
                Box::new(FocusAddress),
            ),
            action(
                "Settings",
                "Theme, terminal, browser, agents",
                Icon::Settings,
                key("settings"),
                Box::new(OpenSettings),
            ),
            action(
                "Usage",
                "Cost and tokens by day and model",
                Icon::Chart,
                key("usage"),
                Box::new(OpenUsage),
            ),
            action(
                "Packages",
                "This project's dependencies, outdated ones and the registry",
                Icon::Package,
                key("packages"),
                Box::new(OpenPackages),
            ),
            action(
                "Connect to Host…",
                "A terminal, an agent or a chat on a machine over SSH",
                Icon::Globe,
                key("connect-host"),
                Box::new(ConnectHost),
            ),
            action(
                "Last Turn's Changes",
                "What the agent in this terminal changed since its turn began",
                Icon::Files,
                None,
                Box::new(ShowTurnDiff),
            ),
            action(
                "Move Pane to New Tab",
                "The focused pane out of its split, into a tab of its own",
                Icon::Maximize,
                key("move-to-tab"),
                Box::new(MoveToTab),
            ),
            action(
                "Close Tabs Idle for a Day",
                "Tabs and threads untouched since yesterday; pinned ones stay",
                Icon::Close,
                None,
                Box::new(CloseIdleTabs),
            ),
            action(
                "Import from Browser",
                "History, bookmarks and sign-ins from Chrome, Arc, Brave, Edge or Safari",
                Icon::Import,
                None,
                Box::new(ImportBrowserData),
            ),
            action(
                "Split Right",
                "",
                Icon::Split,
                key("split-right"),
                Box::new(SplitRight),
            ),
            action(
                "Split Down",
                "",
                Icon::SplitDown,
                key("split-down"),
                Box::new(SplitDown),
            ),
            action(
                "Close",
                "The focused pane",
                Icon::Close,
                key("close"),
                Box::new(ClosePane),
            ),
            action(
                "Find",
                "In the terminal or page",
                Icon::Search,
                Some("⌘F".into()),
                Box::new(Find),
            ),
            action(
                "Clear Terminal",
                "Screen and scrollback",
                Icon::Eraser,
                key("clear"),
                Box::new(ClearTerminal),
            ),
            action(
                "Reload Page",
                "",
                Icon::Refresh,
                key("reload"),
                Box::new(Reload),
            ),
            action(
                "Web Inspector",
                "For the focused page",
                Icon::Info,
                key("devtools"),
                Box::new(ToggleDevTools),
            ),
            action(
                "Toggle Sidebar",
                "",
                Icon::Sidebar,
                key("sidebar"),
                Box::new(ToggleSidebar),
            ),
            action(
                "Sidebar by Project or Time",
                "Projects, or Now, Today, Yesterday…",
                Icon::Clock,
                key("sidebar-view"),
                Box::new(ToggleSidebarView),
            ),
            action(
                "Toggle Inspector",
                "",
                Icon::PanelRight,
                key("inspector"),
                Box::new(ToggleInspector),
            ),
            action("Zoom In", "", Icon::Plus, key("zoom-in"), Box::new(ZoomIn)),
            action(
                "Zoom Out",
                "",
                Icon::Plus,
                key("zoom-out"),
                Box::new(ZoomOut),
            ),
            action(
                "Actual Size",
                "Zoom back to 100%",
                Icon::Plus,
                key("zoom-reset"),
                Box::new(ZoomReset),
            ),
            action(
                "Next Tab",
                "",
                Icon::ChevronDown,
                key("next-tab"),
                Box::new(NextTab),
            ),
            action(
                "Previous Tab",
                "",
                Icon::ChevronUp,
                key("prev-tab"),
                Box::new(PrevTab),
            ),
        ]);
        let root = self
            .focused_cwd(cx)
            .map(|d| crate::inspect::project_root(&d));
        items.extend(snippet_items(root.as_deref()));
        items
    }

    fn run_pick(
        &mut self,
        pick: crate::palette::Pick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::palette::Pick as P;
        match pick {
            P::Pane(id) => self.focus_pane(id, window, cx),
            P::Action(a) => window.dispatch_action(a, cx),
            P::File(path) => cx.open_with_system(&path),
            P::Page(url) => self.open_url_beside(&url, window, cx),
            P::NewTask(brief) => self.start_task(brief, window, cx),
            P::Review(task) => self.open_review(&task, window, cx),
            P::Snippet(body) => self.insert_snippet(&body, window, cx),
            // As typed at the prompt (a paste, so nothing runs until ⏎),
            // with nothing expanded: a command's braces are its own.
            P::Command(text) => match self.focused().and_then(|id| self.panes.get(&id)).map(|p| &p.kind) {
                Some(PaneKind::Terminal(t)) => {
                    let t = t.clone();
                    t.update(cx, |t, _| t.paste_text(&text));
                    window.focus(&t.read(cx).focus);
                }
                _ => {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    self.flash("Command copied. ⌘V to paste it.", cx);
                }
            },
            P::SaveSnippet(text) => {
                let name = crate::snippets::add(&text);
                self.flash(format!("Kept \"{name}\" as a snippet. ⌘; puts it in."), cx);
            }
        }
    }

    // -- snippets -------------------------------------------------------------

    /// Put a snippet's text where you are typing: at the caret in a chat's
    /// composer, or at a terminal's prompt (as a paste, so nothing runs until
    /// you press ⏎). Anywhere else it goes on the clipboard.
    fn insert_snippet(&mut self, body: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.expand_snippet(body, cx);
        let kind = self
            .focused()
            .and_then(|id| self.panes.get(&id))
            .map(|p| match &p.kind {
                PaneKind::Chat(c) => Ok(c.clone()),
                PaneKind::Terminal(t) => Err(Some(t.clone())),
                _ => Err(None),
            });
        match kind {
            Some(Ok(chat)) => {
                let composer = chat.read(cx).composer.clone();
                composer.update(cx, |f, cx| f.insert(&text, cx));
                let focus = composer.read(cx).focus.clone();
                window.focus(&focus);
            }
            Some(Err(Some(term))) => term.update(cx, |t, _| t.paste_text(&text)),
            _ => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.flash("Snippet copied. ⌘V to paste it.", cx);
            }
        }
    }

    /// A snippet's text with its fill-ins: the clipboard, what is selected
    /// in the focused pane, and that pane's branch and folder.
    fn expand_snippet(&self, body: &str, cx: &App) -> String {
        let pane = self.focused().and_then(|id| self.panes.get(&id));
        let cwd = self.focused_cwd(cx);
        crate::snippets::expand(body, |var| match var {
            "clipboard" => cx.read_from_clipboard().and_then(|c| c.text()),
            "selection" => pane.and_then(|p| match &p.kind {
                PaneKind::Terminal(t) => t.read(cx).selection_text(),
                PaneKind::Chat(c) => {
                    let c = c.read(cx);
                    c.selected_text()
                        .or_else(|| c.composer.read(cx).selected_text().map(str::to_string))
                }
                _ => None,
            }),
            "branch" => cwd.as_deref().and_then(crate::snippets::branch),
            "folder" => cwd.as_ref().map(|d| d.display().to_string()),
            _ => None,
        })
    }

    /// Keep what you are writing as a snippet: the composer's text, or what
    /// is selected in the terminal. With neither, open the snippets to add
    /// one by hand.
    fn save_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .focused()
            .and_then(|id| self.panes.get(&id))
            .and_then(|p| match &p.kind {
                PaneKind::Chat(c) => Some(c.read(cx).composer.read(cx).text().to_string()),
                PaneKind::Terminal(t) => t.read(cx).selection_text(),
                _ => None,
            })
            .filter(|t| !t.trim().is_empty());
        match text {
            Some(text) => {
                let name = crate::snippets::add(&text);
                self.flash(format!("Kept \"{name}\" as a snippet. ⌘; puts it in."), cx);
            }
            None => self.edit_snippets(window, cx),
        }
    }

    fn edit_snippets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
        let pane = self.panes.values().find_map(|p| match &p.kind {
            PaneKind::Settings(s) => Some(s.clone()),
            _ => None,
        });
        if let Some(s) = pane {
            s.update(cx, |s, cx| s.show(crate::settings_pane::Page::Snippets, cx));
        }
    }

    // -- default terminal ----------------------------------------------------------

    /// A script or terminal link macOS opened deck with: a new terminal tab
    /// running it, in front.
    fn open_handled(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = url.strip_prefix("deck://job/") {
            let name = crate::handler::percent_decode(name);
            cx.activate(true);
            window.activate_window();
            // A terminal tab's session: go to its tab.
            if let Some(id) = self.sessions.iter().find(|(_, n)| **n == name).map(|(id, _)| *id) {
                self.focus_pane(id, window, cx);
                return;
            }
            match self.named.get(&name).copied().filter(|id| self.panes.contains_key(id)) {
                Some(id) => self.show_job(id, window, cx),
                None => self.show_when_attached = Some(name),
            }
            return;
        }
        if url.starts_with("deck://open") {
            cx.activate(true);
            window.activate_window();
            return;
        }
        if url.starts_with("deck://") {
            self.deck_url(url, cx);
            cx.activate(true);
            return;
        }
        let Some((cmd, cwd)) = crate::handler::command_for(url) else {
            tracing::info!("nothing opens {url}");
            return;
        };
        if let Err(e) = self.new_tab(
            Open::Terminal {
                cmd: Some(cmd),
                cwd,
            },
            None,
            window,
            cx,
        ) {
            self.flash(format!("Could not open {url}: {e}"), cx);
        }
        cx.activate(true);
    }

    /// Once, on a first launch: offer to make deck the Mac's terminal.
    fn offer_default_terminal(&mut self, cx: &mut Context<Self>) {
        if self.settings.asked_default_terminal || crate::handler::is_default() {
            return;
        }
        self.notice = Some(Notice {
            title: "Make Deck your default terminal?".into(),
            detail: Some(
                "Shell scripts, .command files and ssh: links will open in Deck. You can change this in Settings › General."
                    .into(),
            ),
            buttons: vec![
                (
                    "Make Default".into(),
                    Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                        ws.make_default_terminal(true, window, cx)
                    }),
                ),
                (
                    "Not Now".into(),
                    Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                        ws.mark_default_terminal_asked(window, cx)
                    }),
                ),
            ],
        });
        cx.notify();
    }

    fn mark_default_terminal_asked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut next = self.settings.clone();
        next.asked_default_terminal = true;
        self.apply_settings(next, window, cx);
        self.offer_permissions(cx);
    }

    /// Once, on a first launch (after the default-terminal question): when
    /// Deck or its background service lacks Full Disk Access, say what that
    /// breaks and open the way to it. The service's answer is asked off the
    /// main thread; it may still be starting.
    fn offer_permissions(&mut self, cx: &mut Context<Self>) {
        use crate::privacy::{Permission, State};
        if self.settings.asked_permissions || self.notice.is_some() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let (app, service) = cx
                .background_executor()
                .spawn(async move {
                    // The service starts with Deck; give it a moment to answer.
                    let mut service = State::Unknown;
                    for _ in 0..20 {
                        service = crate::privacy::service_full_disk();
                        if service != State::Unknown {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                    (Permission::FullDisk.state(), service)
                })
                .await;
            let missing: Vec<&str> = [(app, "Deck"), (service, "its background service (deckd)")]
                .into_iter()
                .filter(|(s, _)| *s == State::Off)
                .map(|(_, who)| who)
                .collect();
            if missing.is_empty() {
                return;
            }
            let _ = this.update(cx, |ws, cx| {
                if ws.notice.is_some() || ws.settings.asked_permissions {
                    return;
                }
                ws.notice = Some(Notice {
                    title: "Give Deck Full Disk Access?".into(),
                    detail: Some(
                        format!(
                            "{} cannot read Downloads, Desktop or Documents yet, so terminals and agents fail there (an SSH key in Downloads gives \"Operation not permitted\"). Settings › Permissions shows what is on and opens each pane.",
                            missing.join(" and ")
                        )
                        .into(),
                    ),
                    buttons: vec![
                        (
                            "Open Permissions".into(),
                            Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                                ws.mark_permissions_asked(window, cx);
                                ws.open_settings_at(crate::settings_pane::Page::Permissions, window, cx);
                            }),
                        ),
                        (
                            "Not Now".into(),
                            Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                                ws.mark_permissions_asked(window, cx)
                            }),
                        ),
                    ],
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn mark_permissions_asked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut next = self.settings.clone();
        next.asked_permissions = true;
        self.apply_settings(next, window, cx);
    }

    /// Hand shell scripts and terminal links to deck (`on`) or back to
    /// Terminal.
    pub fn make_default_terminal(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let id = if on {
            crate::handler::BUNDLE_ID
        } else {
            crate::handler::TERMINAL_APP
        };
        let failed = crate::handler::set_default(id);
        self.mark_default_terminal_asked(window, cx);
        self.flash(
            match (on, failed) {
                (true, 0) => "Deck is now your default terminal.".to_string(),
                (false, 0) => "Terminal opens scripts and links again.".to_string(),
                (_, n) => format!("macOS refused {n} of the changes; try again from Settings."),
            },
            cx,
        );
        for pane in self.panes.values() {
            if let PaneKind::Settings(s) = &pane.kind {
                s.update(cx, |s, cx| s.refresh_system(cx));
            }
        }
    }

    fn flash(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some((text.into(), Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(6)).await;
            let _ = this.update(cx, |ws, cx| {
                if ws
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(6))
                {
                    ws.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    // -- tasks ----------------------------------------------------------------

    /// The task's app running from its own worktree, beside its review: the
    /// project's dev command in a terminal below it, on a port of its own so
    /// it does not fight the server in your main checkout, and the page
    /// opening beside that once something there listens. With no command
    /// deck knows, the terminal is there to start it by hand and the page
    /// still follows.
    fn preview_task(
        &mut self,
        review: PaneId,
        task: &crate::tasks::Task,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port = crate::tasks::free_port();
        let cmd = crate::tasks::dev_command(&task.worktree).map(|c| match port {
            Some(p) => format!("export PORT={p}; {c}"),
            None => c,
        });
        let known = cmd.is_some();
        let term = match self.split(
            review,
            Open::Terminal {
                cmd,
                cwd: Some(task.worktree.clone()),
            },
            Axis::Column,
            window,
            cx,
        ) {
            Ok(id) => id,
            Err(e) => {
                self.flash(format!("Could not start the preview: {e}"), cx);
                return;
            }
        };
        self.flash(
            if known {
                "Starting the task's dev server; its page opens once it listens."
            } else {
                "No dev script found: start the server in this terminal and its page opens once it listens."
            },
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            // Installing can take a while; give it a few minutes.
            for _ in 0..360 {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let Ok(found) = this.update(cx, |ws, cx| {
                    let Some(PaneKind::Terminal(t)) = ws.panes.get(&term).map(|p| &p.kind) else {
                        return Err(());
                    };
                    let pids: Vec<u32> = inspect::descendants(t.read(cx).pid())
                        .into_iter()
                        .map(|p| p.pid)
                        .collect();
                    let ports: Vec<u16> = inspect::listening_ports(&pids)
                        .into_iter()
                        .map(|p| p.port)
                        .collect();
                    // The port it was given, else its lowest (a dev server's
                    // reload socket usually sits on a higher one).
                    Ok(port
                        .filter(|p| ports.contains(p))
                        .or_else(|| ports.iter().min().copied()))
                }) else {
                    return;
                };
                match found {
                    Err(()) => return, // the terminal was closed
                    Ok(None) => continue,
                    Ok(Some(p)) => {
                        let _ = this.update_in(cx, |ws, window, cx| {
                            let url = format!("http://localhost:{p}");
                            let _ = ws.split(term, Open::Browser { url }, Axis::Row, window, cx);
                        });
                        return;
                    }
                }
            }
        })
        .detach();
    }

    /// A task from a brief: its own branch and worktree off the project you
    /// are in, and a chat with the default agent working in it.
    fn start_task(&mut self, brief: String, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.focused_cwd(cx).unwrap_or_else(|| self.cwd.clone());
        self.spawn_task(brief, dir, None, None, None, window, cx);
    }

    /// Start a task in the repository `dir` is in. `agent` picks who works
    /// on it (the default when `None`), `parent` is the chat thread that
    /// asked for it, and `reply` answers a `deck task new` once it runs.
    #[allow(clippy::too_many_arguments)]
    fn spawn_task(
        &mut self,
        brief: String,
        dir: PathBuf,
        agent: Option<String>,
        parent: Option<String>,
        reply: Option<Reply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flash("Starting a task…", cx);
        cx.spawn_in(window, async move |this, cx| {
            let made = cx
                .background_executor()
                .spawn({
                    let brief = brief.clone();
                    async move { crate::tasks::create(&dir, &brief) }
                })
                .await;
            let _ = this.update_in(cx, |ws, window, cx| match made {
                Ok(mut task) => {
                    task.parent = parent;
                    let agent =
                        agent.or_else(|| ws.default_agent().map(|a| a.as_str().to_string()));
                    let opened = ws.new_tab(
                        Open::Chat {
                            agent,
                            cwd: Some(task.worktree.clone()),
                        },
                        None,
                        window,
                        cx,
                    );
                    match opened {
                        Ok(id) => {
                            if let Some(PaneKind::Chat(c)) = ws.panes.get(&id).map(|p| &p.kind) {
                                task.thread = Some(c.read(cx).thread_id().to_string());
                                let brief = task.brief.clone();
                                c.update(cx, |c, cx| c.send(brief, cx));
                            }
                        }
                        // No chat agent ready: Claude Code itself, in a terminal there.
                        Err(_) => {
                            let line = format!("claude {}", shell_quote(&task.brief));
                            let claude = AGENTS.iter().find(|a| a.cmd == "claude");
                            let _ = ws.new_tab(
                                Open::Terminal {
                                    cmd: Some(line),
                                    cwd: Some(task.worktree.clone()),
                                },
                                claude,
                                window,
                                cx,
                            );
                        }
                    }
                    ws.flash(format!("Working on {} in {}", task.title, task.branch), cx);
                    let answer = ws.task_json(&task, cx);
                    ws.tasks.push(task);
                    crate::tasks::save_all(&ws.tasks);
                    ws.refresh_task_changes(cx);
                    if let Some(r) = reply {
                        r.send(Ok(answer));
                    }
                }
                Err(e) => {
                    ws.flash(format!("Could not start the task: {e}"), cx);
                    if let Some(r) = reply {
                        r.send(Err(e));
                    }
                }
            });
        })
        .detach();
    }

    /// `deck task new BRIEF`: a task in the project the caller works in,
    /// told back to the caller's chat when it finishes.
    fn rpc_task_new(
        &mut self,
        p: &Value,
        caller: PaneId,
        reply: Reply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(brief) = str_param(p, "text").filter(|b| !b.trim().is_empty()) else {
            return reply.send(Err("say what the task is: deck task new 'brief'".into()));
        };
        let brief = brief.to_string();
        let dir = str_param(p, "cwd")
            .map(PathBuf::from)
            .or_else(|| self.cwd_of(caller, cx))
            .unwrap_or_else(|| self.cwd.clone());
        let agent = str_param(p, "agent").map(str::to_string);
        let parent = match self.panes.get(&caller).map(|p| &p.kind) {
            Some(PaneKind::Chat(c)) => Some(c.read(cx).thread_id().to_string()),
            _ => None,
        };
        self.spawn_task(brief, dir, agent, parent, Some(reply), window, cx);
    }

    /// Where a pane works: a terminal's current folder, a chat's.
    fn cwd_of(&self, id: PaneId, cx: &App) -> Option<PathBuf> {
        match &self.panes.get(&id)?.kind {
            PaneKind::Chat(c) => Some(c.read(cx).cwd().to_path_buf()),
            PaneKind::Terminal(t) => self.places.get(&id).map(|p| p.cwd.clone()).or_else(|| {
                let procs = inspect::descendants(t.read(cx).pid());
                procs.iter().rev().find_map(|p| inspect::cwd(p.pid))
            }),
            _ => None,
        }
    }

    /// The task a call names with `--task ID` (a unique prefix will do).
    fn task_param(&self, p: &Value) -> Result<crate::tasks::Task, String> {
        let id = str_param(p, "task").ok_or("missing --task ID (see deck task list)")?;
        let open = self
            .tasks
            .iter()
            .filter(|t| t.state == crate::tasks::State::Open);
        let hits: Vec<&crate::tasks::Task> = open
            .clone()
            .filter(|t| t.id == id)
            .chain(open.filter(|t| t.id != id && t.id.starts_with(id)))
            .collect();
        match hits.as_slice() {
            [one, ..] if one.id == id => Ok((*one).clone()),
            [one] => Ok((*one).clone()),
            [] => Err(format!("no open task {id:?}")),
            _ => Err(format!("{id:?} names more than one task")),
        }
    }

    fn task_json(&self, t: &crate::tasks::Task, cx: &App) -> Value {
        let pane = self.task_pane(t, cx);
        let status = match pane.and_then(|p| self.status(p, cx)) {
            Some(Status::Working) => "working",
            Some(Status::Attention) => "needs-you",
            Some(Status::Idle) => "idle",
            None if pane.is_some() => "idle",
            None => "closed",
        };
        let c = self.task_changes.get(&t.id).cloned().unwrap_or_default();
        json!({
            "task": t.id,
            "title": t.title,
            "branch": t.branch,
            "base": t.base,
            "worktree": t.worktree,
            "status": status,
            "pane": pane,
            "files": c.files,
            "added": c.added,
            "removed": c.removed,
        })
    }

    /// A task's agent finished a stretch of work: tell the chat that handed
    /// it out, which reads it as soon as it is free.
    fn report_to_parent(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let Some(task) = self
            .tasks
            .iter()
            .find(|t| t.state == crate::tasks::State::Open && self.task_pane(t, cx) == Some(pane))
            .cloned()
        else {
            return;
        };
        let Some(parent) = task.parent.as_deref() else {
            return;
        };
        let Some(PaneKind::Chat(c)) = self
            .panes
            .values()
            .map(|p| &p.kind)
            .find(|k| matches!(k, PaneKind::Chat(c) if c.read(cx).thread_id() == parent))
        else {
            return;
        };
        let said = match self.panes.get(&pane).map(|p| &p.kind) {
            Some(PaneKind::Chat(w)) => w.read(cx).last_reply(),
            _ => None,
        };
        let ch = self.task_changes.get(&task.id).cloned().unwrap_or_default();
        let msg = format!(
            "[deck] Sub-task \"{}\" ({}) finished: {}. It changed {} file(s), +{} -{}. \
             Review with `deck task diff --task {id}`, follow up with `deck task send --task {id} '…'`, \
             then `deck task merge --task {id}` (or `pr` / `discard`).",
            task.title,
            task.id,
            said.unwrap_or_else(|| "no reply text".into()),
            ch.files,
            ch.added,
            ch.removed,
            id = task.id,
        );
        let c = c.clone();
        c.update(cx, |c, cx| c.send(msg, cx));
    }

    fn task(&self, id: &str) -> Option<&crate::tasks::Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// The pane doing a task's work: its chat, or a terminal in its worktree.
    fn task_pane(&self, task: &crate::tasks::Task, cx: &App) -> Option<PaneId> {
        self.panes.iter().find_map(|(id, p)| match &p.kind {
            PaneKind::Chat(c) if task.thread.as_deref() == Some(c.read(cx).thread_id()) => {
                Some(*id)
            }
            PaneKind::Terminal(_)
                if self
                    .places
                    .get(id)
                    .is_some_and(|pl| pl.cwd.starts_with(&task.worktree)) =>
            {
                Some(*id)
            }
            _ => None,
        })
    }

    /// Whether a tab is doing a task's work, so the sidebar lists it there.
    fn is_task_tab(&self, tab: &Tab, cx: &App) -> bool {
        tab.leaves().iter().any(|id| {
            self.tasks
                .iter()
                .filter(|t| t.state == crate::tasks::State::Open)
                .any(|t| self.task_pane(t, cx) == Some(*id))
        })
    }

    /// Count what every open task has changed, off the main thread.
    fn refresh_task_changes(&mut self, cx: &mut Context<Self>) {
        let open: Vec<crate::tasks::Task> = self
            .tasks
            .iter()
            .filter(|t| t.state == crate::tasks::State::Open && t.worktree.exists())
            .cloned()
            .collect();
        if open.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let counted = cx
                .background_executor()
                .spawn(async move {
                    open.into_iter()
                        .filter_map(|t| crate::tasks::changes(&t).ok().map(|c| (t.id, c)))
                        .collect::<HashMap<_, _>>()
                })
                .await;
            let _ = this.update(cx, |ws, cx| {
                if counted != ws.task_changes {
                    ws.task_changes = counted;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A task's review, beside its work if that is open.
    fn open_review(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.panes.iter().find_map(|(pid, p)| match &p.kind {
            PaneKind::Review(r) if r.read(cx).task.id == id => Some(*pid),
            _ => None,
        });
        if let Some(pid) = existing {
            if let PaneKind::Review(r) = &self.panes[&pid].kind {
                r.update(cx, |r, cx| r.reload(cx));
            }
            self.focus_pane(pid, window, cx);
            return;
        }
        let beside = self.task(id).and_then(|t| self.task_pane(t, cx));
        let made = match beside {
            Some(p) => self.split(p, Open::Review(id.to_string()), Axis::Row, window, cx),
            None => self.new_tab(Open::Review(id.to_string()), None, window, cx),
        };
        if let Err(e) = made {
            self.flash(format!("Could not open the review: {e}"), cx);
        }
    }

    fn make_review(
        &mut self,
        pane: PaneId,
        task: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<(PaneKind, Option<String>, Subscription)> {
        let task = self
            .task(task)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no task {task}"))?;
        let r = cx.new(|cx| ReviewPane::new(task, cx));
        let sub = cx.subscribe_in(&r, window, move |ws, r, ev: &ReviewEvent, window, cx| {
            let task = r.read(cx).task.clone();
            match ev {
                ReviewEvent::Terminal => {
                    let _ = ws.split(
                        pane,
                        Open::Terminal {
                            cmd: None,
                            cwd: Some(task.worktree.clone()),
                        },
                        Axis::Column,
                        window,
                        cx,
                    );
                }
                ReviewEvent::Preview => ws.preview_task(pane, &task, window, cx),
                ReviewEvent::Merge => ws.finish_task(pane, task, Finish::Merge, window, cx),
                ReviewEvent::Discard => ws.finish_task(pane, task, Finish::Discard, window, cx),
                ReviewEvent::OpenPr => ws.finish_task(pane, task, Finish::Pr, window, cx),
            }
        });
        Ok((PaneKind::Review(r), None, sub))
    }

    /// Merge, discard or open a PR for a task, off the main thread; then
    /// close what was working on it (or, for a PR, open the PR beside).
    fn finish_task(
        &mut self,
        review: PaneId,
        task: crate::tasks::Task,
        how: Finish,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_task_with(Some(review), task, how, None, window, cx);
    }

    fn finish_task_with(
        &mut self,
        review: Option<PaneId>,
        task: crate::tasks::Task,
        how: Finish,
        reply: Option<Reply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(PaneKind::Review(r)) = review.and_then(|r| self.panes.get(&r)).map(|p| &p.kind)
        {
            r.update(cx, |r, cx| {
                r.set_working(
                    Some(match how {
                        Finish::Merge => "Merging…",
                        Finish::Discard => "Discarding…",
                        Finish::Pr => "Pushing and opening a pull request…",
                    }),
                    cx,
                )
            });
        }
        cx.spawn_in(window, async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn({
                    let task = task.clone();
                    async move {
                        match how {
                            Finish::Merge => crate::tasks::merge(&task).map(|_| None),
                            Finish::Discard => crate::tasks::discard(&task).map(|_| None),
                            Finish::Pr => crate::tasks::open_pr(&task).map(Some),
                        }
                    }
                })
                .await;
            let _ = this.update_in(cx, |ws, window, cx| {
                let review_pane =
                    review
                        .and_then(|r| ws.panes.get(&r))
                        .and_then(|p| match &p.kind {
                            PaneKind::Review(r) => Some(r.clone()),
                            _ => None,
                        });
                match done {
                    Err(e) => {
                        if let Some(r) = review_pane {
                            let e = e.clone();
                            r.update(cx, |r, cx| r.fail(e, cx));
                        }
                        if let Some(r) = reply {
                            r.send(Err(e));
                        }
                    }
                    Ok(Some(url)) => {
                        if let Some(r) = review_pane {
                            r.update(cx, |r, cx| r.set_working(None, cx));
                        }
                        let near = review.or_else(|| ws.task_pane(&task, cx));
                        match near {
                            Some(p) => {
                                ws.open_url_near(p, &url, window, cx);
                            }
                            None => ws.open_url_beside(&url, window, cx),
                        }
                        ws.flash("Pull request opened", cx);
                        if let Some(r) = reply {
                            r.send(Ok(json!({ "task": task.id, "pr": url })));
                        }
                    }
                    Ok(None) => {
                        let worker = ws.task_pane(&task, cx);
                        if let Some(t) = ws.tasks.iter_mut().find(|t| t.id == task.id) {
                            t.state = if how == Finish::Merge {
                                crate::tasks::State::Merged
                            } else {
                                crate::tasks::State::Discarded
                            };
                        }
                        crate::tasks::save_all(&ws.tasks);
                        if let Some(review) = review {
                            ws.close_pane(review, window, cx);
                        }
                        if let Some(w) = worker {
                            ws.close_pane(w, window, cx);
                        }
                        if let Some(r) = reply {
                            r.send(Ok(json!({
                                "task": task.id,
                                "state": if how == Finish::Merge { "merged" } else { "discarded" },
                            })));
                        }
                        ws.flash(
                            if how == Finish::Merge {
                                format!("Merged “{}” into {}", task.title, task.base)
                            } else {
                                format!("Discarded “{}”", task.title)
                            },
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    fn render_task_row(&self, task: &crate::tasks::Task, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.task_pane(task, cx);
        let status = pane.and_then(|p| self.status(p, cx));
        let active = pane.is_some_and(|p| self.focused() == Some(p));
        let ch = self.task_changes.get(&task.id).cloned().unwrap_or_default();
        let working = status == Some(Status::Working);
        let wants = status == Some(Status::Attention);
        let line: SharedString = match pane
            .filter(|_| working || wants)
            .and_then(|p| self.agent_activity(p, cx))
        {
            Some(doing) => doing.into(),
            None if ch.files == 0 => "No changes yet".into(),
            None => format!(
                "{} file{} · +{} −{}",
                ch.files,
                if ch.files == 1 { "" } else { "s" },
                ch.added,
                ch.removed
            )
            .into(),
        };
        let id = task.id.clone();
        let review_id = task.id.clone();
        let key = SharedString::from(format!("task-{}", task.id));
        sidebar_row(div().id(ElementId::Name(key.clone())), 0., active, !working && !wants)
            .group(key.clone())
            .h(u(ROW_TWO_LINE))
            .child(lead(if working {
                thinking(
                    ElementId::Name(format!("task-think-{}", task.id).into()),
                    Icon::Git,
                    15.,
                    theme::accent(),
                )
            } else {
                icon(
                    Icon::Git,
                    15.,
                    if active {
                        theme::text()
                    } else {
                        theme::text_2()
                    },
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .w_full()
                            .one_line()
                            .text_size(u(13.))
                            .text_color(rgb(if active {
                                theme::text()
                            } else {
                                theme::text_2()
                            }))
                            .child(task.title.clone()),
                    )
                    .child(
                        div()
                            .w_full()
                            .one_line()
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child(line),
                    ),
            )
            .child(if wants {
                alert_pill().into_any_element()
            } else if working {
                div()
                    .text_size(u(11.))
                    .child(shimmer(
                        ElementId::Name(format!("task-sh-{}", task.id).into()),
                        "Working".into(),
                    ))
                    .into_any_element()
            } else {
                div()
                    .id(ElementId::Name(format!("task-review-{}", task.id).into()))
                    .flex_none()
                    .flex()
                    .items_center()
                    .h(u(22.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .bg(rgb(theme::surface()))
                    .text_size(u(11.))
                    .text_color(rgb(theme::text()))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::surface_hover())))
                    .child("Review")
                    .on_click(cx.listener(move |ws, _, window, cx| {
                        cx.stop_propagation();
                        ws.open_review(&review_id, window, cx);
                    }))
                    .into_any_element()
            })
            .on_click(cx.listener(move |ws, _, window, cx| {
                let pane = ws.task(&id).and_then(|t| ws.task_pane(t, cx));
                match pane {
                    Some(p) => ws.focus_pane(p, window, cx),
                    None => ws.open_review(&id, window, cx),
                }
            }))
            .into_any_element()
    }

    fn make_launcher(
        &mut self,
        id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (PaneKind, Option<String>, Subscription) {
        let cwd = self.focused_cwd(cx);
        let place = cwd.as_deref().map(|c| place_of(c).label());
        let mut choices = vec![
            Choice {
                pick: Pick::Terminal,
                label: "Terminal".into(),
                detail: "Your shell".into(),
                glyph: Icon::Terminal,
                enabled: true,
                tui: None,
            },
            Choice {
                pick: Pick::Browser,
                label: "Browser".into(),
                detail: "Search or open a page".into(),
                glyph: Icon::Globe,
                enabled: true,
                tui: None,
            },
        ];
        for agent in AGENTS {
            let installed = self.installed.iter().any(|a| a.cmd == agent.cmd);
            let chat = self
                .chats
                .as_ref()
                .and_then(|env| env.registry.get(&agent_core::AgentId::new(agent.cmd)))
                .is_some_and(|e| e.is_ready());
            choices.push(Choice {
                pick: if chat {
                    Pick::Chat(agent)
                } else {
                    Pick::Tui(agent)
                },
                label: agent.name.into(),
                detail: match (chat, installed) {
                    (true, _) => "Chat · or in a terminal",
                    (false, true) => "In a terminal",
                    (false, false) => "Not installed",
                }
                .into(),
                glyph: Icon::agent(agent.cmd),
                enabled: chat || installed,
                tui: (chat && installed).then_some(agent),
            });
        }
        let l = cx.new(|cx| LauncherPane::new(choices, cwd, place, cx));
        let sub = cx.subscribe_in(
            &l,
            window,
            move |ws, l, ev: &LauncherEvent, window, cx| match ev {
                LauncherEvent::Pick(pick) => {
                    let cwd = l.read(cx).cwd.clone();
                    ws.turn_into(id, *pick, cwd, window, cx);
                }
                LauncherEvent::Cancel => ws.close_pane(id, window, cx),
            },
        );
        (PaneKind::Launcher(l), None, sub)
    }

    /// Turn the launcher `id` into what was picked, in the same place.
    fn turn_into(
        &mut self,
        id: PaneId,
        pick: Pick,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (what, agent) = match pick {
            Pick::Terminal => (Open::Terminal { cmd: None, cwd }, None),
            Pick::Browser => (
                Open::Browser {
                    url: "about:blank".into(),
                },
                None,
            ),
            Pick::Chat(a) => (
                Open::Chat {
                    agent: Some(a.cmd.into()),
                    cwd,
                },
                None,
            ),
            Pick::Tui(a) => (
                Open::Terminal {
                    cmd: Some(a.cmd.into()),
                    cwd,
                },
                Some(a),
            ),
        };
        let Some(ix) = self.tab_of(id) else {
            return;
        };
        let new = match self.make_pane(what, agent, window, cx) {
            Ok(new) => new,
            Err(e) => {
                tracing::warn!("new tab: {e}");
                return;
            }
        };
        let tab = &mut self.tabs[ix];
        tab.root.replace(id, new);
        if tab.focused == id {
            tab.focused = new;
        }
        self.discard(id, cx);
        self.focus_pane(new, window, cx);
        if matches!(pick, Pick::Browser) {
            self.focus_address(window, cx);
        }
    }

    /// ⌘,: the one Settings tab, opened or brought forward.
    /// One of Deck's own pages: the tab it has, or a new one.
    fn open_page(&mut self, kind: PageKind, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.panes.iter().find_map(|(id, p)| match p.kind {
            PaneKind::Page(_, k) if k == kind => Some(*id),
            _ => None,
        });
        match existing {
            Some(id) => self.focus_pane(id, window, cx),
            None => {
                if let Err(e) = self.new_tab(Open::Page(kind), None, window, cx) {
                    tracing::warn!("{}: {e}", kind.title());
                }
            }
        }
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.panes.iter().find_map(|(id, p)| match p.kind {
            PaneKind::Settings(_) => Some(*id),
            _ => None,
        });
        match existing {
            Some(id) => self.focus_pane(id, window, cx),
            None => {
                if let Err(e) = self.new_tab(Open::Settings, None, window, cx) {
                    tracing::warn!("settings: {e}");
                }
            }
        }
    }

    /// Show or hide the inspector. Asking for it in a window too narrow for
    /// three columns trades the sidebar for it rather than doing nothing.
    fn toggle_inspector(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // In unzoomed units, so the breakpoints hold at any zoom.
        let width = f32::from(window.viewport_size().width) / self.settings.zoom;
        let fits = |sidebar: bool| {
            width - if sidebar { self.sidebar_px } else { 0. } - self.inspector_px
                >= MIN_CONTENT_PX
        };
        // Narrow windows are handled where the window is drawn (the sidebar
        // steps aside while the inspector is open); here it only opens and
        // closes, so closing it brings back exactly what was there.
        let _ = fits;
        self.inspector.open = !self.inspector.open;
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// Title and URL when the focused pane is a browser.
    /// The inspector's width, as dragged.
    pub fn inspector_width(&self) -> f32 {
        self.inspector_px
    }

    pub fn focused_browser(&self, cx: &App) -> Option<(SharedString, SharedString)> {
        let id = self.focused()?;
        let PaneKind::Browser(b) = &self.panes.get(&id)?.kind else {
            return None;
        };
        Some((self.pane_title(id, cx), b.read(cx).url.clone()))
    }

    fn status(&self, id: PaneId, cx: &App) -> Option<Status> {
        let pane = self.panes.get(&id)?;
        if let PaneKind::Chat(c) = &pane.kind {
            return Some(match c.read(cx).status {
                ChatStatus::Working => Status::Working,
                ChatStatus::Idle => Status::Idle,
                ChatStatus::Waiting | ChatStatus::Failed => Status::Attention,
            });
        }
        let PaneKind::Terminal(t) = &pane.kind else {
            return None;
        };
        // Anything ringing wants you; past that, only agents have a state,
        // unless the program said its own with `deck report`.
        if t.read(cx).bell {
            return Some(Status::Attention);
        }
        if self.blocked.contains(&id) {
            return Some(Status::Attention);
        }
        if let Some((state, _)) = &t.read(cx).reported {
            return Some(match state.as_str() {
                "working" => Status::Working,
                "blocked" | "error" => Status::Attention,
                _ => Status::Idle,
            });
        }
        let agent = pane.agent.is_some() || self.places.get(&id).is_some_and(|p| p.agent.is_some());
        agent.then(|| {
            if self.busy.contains(&id) {
                Status::Working
            } else {
                Status::Idle
            }
        })
    }

    /// A pane wants you. If you are looking at it, that is answered already;
    /// otherwise its tab says so and, for an agent, macOS does too.
    fn alert(
        &mut self,
        id: PaneId,
        said: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PaneKind::Terminal(t)) = self.panes.get(&id).map(|p| &p.kind) else {
            return;
        };
        if window.is_window_active() && self.focused() == Some(id) {
            t.update(cx, |t, cx| t.seen(cx));
            return;
        }
        let agent = self
            .places
            .get(&id)
            .and_then(|p| p.agent.as_ref())
            .and_then(|(cmd, _)| AGENTS.iter().find(|a| a.cmd == *cmd))
            .or(self.panes.get(&id).and_then(|p| p.agent));
        // A shell's bell (a failed completion) marks the tab and no more.
        let Some(agent) = agent else {
            cx.notify();
            return;
        };
        if self.settings.notify_attention {
            let body = said.unwrap_or_else(|| format!("{} needs you", agent.name));
            self.post(id, body, cx);
        }
        cx.notify();
    }

    /// Claude Code in a terminal waits out API errors itself ("Retrying in
    /// 18s · attempt 4/10") and can sit there a long time, or get no answer
    /// at all. Deck presses Esc and, five seconds later, Enter (if Esc put
    /// the message back in the box) or "continue". One Esc per stall: a
    /// second would open Claude Code's rewind menu.
    fn unstick_agents(&mut self, cx: &mut Context<Self>) {
        if !crate::retry::enabled() {
            return;
        }
        let now = Instant::now();
        let claude: Vec<(PaneId, Entity<TerminalPane>)> = self
            .panes
            .iter()
            .filter(|(id, _)| {
                matches!(
                    self.places.get(id).and_then(|p| p.agent.as_ref()),
                    Some(("claude", _))
                )
            })
            .filter_map(|(id, p)| match &p.kind {
                PaneKind::Terminal(t) => Some((*id, t.clone())),
                _ => None,
            })
            .collect();
        for (id, t) in claude {
            let screen = t.update(cx, |t, _| t.read_text());
            if crate::retry::stall_line(&screen).is_none() {
                // A quiet minute ends a streak.
                if self
                    .stalls
                    .get(&id)
                    .is_some_and(|(_, at)| now - *at > Duration::from_secs(60))
                {
                    self.stalls.remove(&id);
                }
                continue;
            }
            let (n, last) = self
                .stalls
                .get(&id)
                .copied()
                .unwrap_or((0, now - Duration::from_secs(3600)));
            if now - last < Duration::from_secs(30) || n >= crate::retry::MAX_TRIES {
                continue;
            }
            self.stalls.insert(id, (n + 1, now));
            t.update(cx, |t, cx| t.send("\x1b", false, cx));
            let weak = t.downgrade();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let _ = weak.update(cx, |t, cx| {
                    if crate::retry::input_has_text(&t.read_text()) {
                        t.send("", true, cx);
                    } else {
                        t.send("continue", true, cx);
                    }
                });
            })
            .detach();
            tracing::info!(
                pane = id,
                try_ = n + 1,
                "unstuck Claude Code from an API wait"
            );
        }
    }

    /// "Claude finished: <the opening of its reply>", or of its task.
    fn finished_line(&self, id: PaneId, cx: &App) -> String {
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Chat(c)) => {
                let c = c.read(cx);
                match c.last_reply() {
                    Some(r) => format!("{} finished: {r}", c.agent_name()),
                    None => format!("{} finished", c.agent_name()),
                }
            }
            _ => {
                let who = self
                    .places
                    .get(&id)
                    .and_then(|p| p.agent.as_ref())
                    .and_then(|(cmd, _)| AGENTS.iter().find(|a| a.cmd == *cmd))
                    .or(self.panes.get(&id).and_then(|p| p.agent))
                    .map_or("The agent", |a| a.name);
                format!("{who} finished")
            }
        }
    }

    /// What an agent is up to, for the second line of its row: the step it
    /// is on, or what it waits on you for.
    fn agent_activity(&self, id: PaneId, cx: &App) -> Option<String> {
        match &self.panes.get(&id)?.kind {
            PaneKind::Chat(c) => {
                let c = c.read(cx);
                c.activity().or_else(|| c.waiting_for())
            }
            PaneKind::Terminal(t) => {
                let t = t.read(cx);
                t.reported.as_ref().map(|(_, m)| m.clone()).filter(|m| !m.is_empty()).or_else(|| t.alert.clone())
            }
            _ => None,
        }
    }

    /// One notification per pane at a time: an agent that rings and posts
    /// for the same prompt shows up once.
    /// A command at a shell finished. One that took a while, in a pane
    /// you aren't looking at, says so (as agents do when they finish).
    fn command_done(
        &mut self,
        id: PaneId,
        command: Option<String>,
        exit: Option<i32>,
        took: Duration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let looking = window.is_window_active() && self.focused() == Some(id);
        if looking || took < Duration::from_secs(10) || !self.settings.notify_done {
            return;
        }
        let what = command
            .map(|c| crate::chat::one_line_pub(&c))
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "The command".into());
        let secs = took.as_secs();
        let long = if secs >= 60 { format!("{}m {}s", secs / 60, secs % 60) } else { format!("{secs}s") };
        let body = match exit {
            Some(0) | None => format!("✓ {what} finished in {long}"),
            Some(code) => format!("✗ {what} failed (exit {code}) after {long}"),
        };
        self.post(id, body, cx);
    }

    /// "# …" at a terminal's prompt: a command for it, typed at the prompt
    /// (not run) once the model has written it.
    fn write_command(&mut self, id: PaneId, request: String, cwd: Option<PathBuf>, cx: &mut Context<Self>) {
        let cwd = cwd.filter(|c| c.is_dir()).or_else(|| self.cwd_of(id, cx)).unwrap_or_else(|| self.cwd.clone());
        let model = crate::refine::model();
        cx.spawn(async move |this, cx| {
            let got = cx
                .background_executor()
                .spawn(async move { crate::refine::shell_command(&request, &cwd, &model) })
                .await;
            let _ = this.update(cx, |ws, cx| match got {
                Ok(cmd) => {
                    if let Some(PaneKind::Terminal(t)) = ws.panes.get(&id).map(|p| &p.kind) {
                        t.update(cx, |t, cx| {
                            t.ask_done(cx);
                            t.paste_text(&cmd)
                        });
                    }
                }
                Err(e) => {
                    if let Some(PaneKind::Terminal(t)) = ws.panes.get(&id).map(|p| &p.kind) {
                        t.update(cx, |t, cx| t.ask_done(cx));
                    }
                    ws.flash(format!("Couldn't write that command: {e}"), cx)
                }
            });
        })
        .detach();
    }

    /// "Fix with agent" on a terminal's command: a chat beside it, in its
    /// folder, with the command and its output.
    fn ask_agent_beside(
        &mut self,
        id: PaneId,
        prompt: String,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = cwd.filter(|c| c.is_dir()).or_else(|| self.cwd_of(id, cx));
        let open = Open::Chat { agent: None, cwd };
        match self.split(id, open, Axis::Row, window, cx) {
            Ok(chat) => {
                if let Some(PaneKind::Chat(c)) = self.panes.get(&chat).map(|p| &p.kind) {
                    c.update(cx, |c, cx| c.send(prompt, cx));
                }
            }
            Err(e) => tracing::warn!("could not start a chat: {e}"),
        }
    }

    /// A button on one of Deck's notifications: answer the chat's request
    /// where it is, without coming to the window; a click opens the pane.
    fn notification_action(&mut self, ev: crate::notify::Event, window: &mut Window, cx: &mut Context<Self>) {
        use crate::notify::Event;
        let pane = match &ev {
            Event::Open(p) | Event::Allow(p) | Event::AllowAlways(p) | Event::Deny(p) | Event::Reply(p, _) => *p,
        };
        let chat = match self.panes.get(&pane).map(|p| &p.kind) {
            Some(PaneKind::Chat(c)) => Some(c.clone()),
            Some(_) => None,
            None => return,
        };
        match (ev, chat) {
            (Event::Allow(_), Some(c)) => c.update(cx, |c, cx| c.approve(false, cx)),
            (Event::AllowAlways(_), Some(c)) => c.update(cx, |c, cx| c.approve(true, cx)),
            (Event::Deny(_), Some(c)) => c.update(cx, |c, cx| c.deny(cx)),
            (Event::Reply(_, text), Some(c)) => c.update(cx, |c, cx| c.reply(text, cx)),
            (Event::Reply(_, text), None) => {
                // A terminal agent: the words go to its prompt.
                if let Some(PaneKind::Terminal(t)) = self.panes.get(&pane).map(|p| &p.kind) {
                    t.update(cx, |t, cx| t.send(&text, true, cx));
                }
            }
            _ => {
                window.activate_window();
                self.focus_pane(pane, window, cx);
            }
        }
        cx.notify();
    }

    fn post(&mut self, id: PaneId, body: String, cx: &App) {
        let now = Instant::now();
        if self
            .alerted
            .get(&id)
            .is_some_and(|(at, said)| now - *at < Duration::from_secs(5) && *said == body)
        {
            return;
        }
        self.alerted.insert(id, (now, body.clone()));
        let title = self.pane_title(id, cx);
        use crate::notify::Kind;
        let kind = match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Chat(c)) => match c.read(cx).ask_is_permission() {
                Some(true) => Kind::Permission,
                Some(false) => Kind::Question,
                None => Kind::Info,
            },
            _ => Kind::Info,
        };
        let sound = match (kind, self.status(id, cx)) {
            (Kind::Permission | Kind::Question, _) | (_, Some(Status::Attention)) => &self.settings.sound_attention,
            _ => &self.settings.sound_done,
        };
        crate::notify::play(sound, self.settings.sound_volume);
        crate::notify::post_kind(id, &title, &body, kind);
    }

    fn pane_title(&self, id: PaneId, cx: &App) -> SharedString {
        match self.panes.get(&id) {
            // Claude Code and friends title the terminal with the task at
            // hand, which says more than the agent's name.
            Some(Pane {
                agent: Some(a),
                kind: PaneKind::Terminal(t),
                ..
            }) => clean_title(&t.read(cx).title)
                .map(|t| self.own_agent_title(id, t, cx))
                .filter(|t| {
                    !t.eq_ignore_ascii_case(a.name)
                        && !t.eq_ignore_ascii_case(a.cmd)
                        && !t.starts_with(&format!("{} ", a.cmd))
                })
                .unwrap_or_else(|| a.name.to_string())
                .into(),
            Some(Pane { agent: Some(a), .. }) => a.name.into(),
            Some(Pane {
                kind: PaneKind::Chat(c),
                ..
            }) => {
                let c = c.read(cx);
                if c.title.is_empty() {
                    c.agent_name()
                } else {
                    c.title.clone()
                }
            }
            // The sidebar's name for it, so a tab is called one thing
            // everywhere: never "login" or the command line an agent ran.
            Some(Pane {
                kind: PaneKind::Terminal(t),
                ..
            }) => match self.terminal_row_text(id, cx) {
                Some((title, _)) => title,
                None => clean_title(&t.read(cx).title)
                    .map(|t| self.own_agent_title(id, t, cx))
                    .or_else(|| self.places.get(&id).and_then(|p| p.front.clone()))
                    .unwrap_or_else(|| short_title(&t.read(cx).title))
                    .into(),
            },
            Some(Pane {
                kind: PaneKind::Settings(_),
                ..
            }) => "Settings".into(),
            Some(Pane {
                kind: PaneKind::Launcher(_),
                ..
            }) => "New Tab".into(),
            Some(Pane {
                kind: PaneKind::Page(_, k),
                ..
            }) => k.title().into(),
            Some(Pane {
                kind: PaneKind::Review(r),
                ..
            }) => format!("Review: {}", r.read(cx).task.title).into(),
            Some(Pane {
                kind: PaneKind::Browser(b),
                ..
            }) => {
                let b = b.read(cx);
                if !b.title.is_empty() {
                    b.title.clone()
                } else if b.url.is_empty() || b.url.as_ref() == "about:blank" {
                    "New Tab".into()
                } else {
                    host(&b.url).into()
                }
            }
            None => "".into(),
        }
    }

    /// The second line of a tab: where a terminal or chat is working, the
    /// site a page is on.
    /// A sidebar row's second line, under its project's header: a page's
    /// host, the editor, or where in the project a terminal is and what it
    /// runs.
    fn row_detail(&self, id: PaneId, cx: &App) -> Option<SharedString> {
        match &self.panes.get(&id)?.kind {
            PaneKind::Browser(b) => {
                let b = b.read(cx);
                if b.is_editor() {
                    return Some("Editor".into());
                }
                (!b.is_blank()).then(|| crate::history::host(&b.url).to_string().into())
            }
            PaneKind::Terminal(_) => {
                let place = self.places.get(&id)?;
                let front = place.front.as_deref().map(|f| f.trim_start_matches('-')).unwrap_or("");
                Some(match (place.sub.is_empty(), front.is_empty()) {
                    (true, true) => "terminal".into(),
                    (true, false) => front.to_string().into(),
                    (false, true) => place.sub.clone().into(),
                    (false, false) => format!("{} · {front}", place.sub).into(),
                })
            }
            _ => self.pane_subtitle(id, cx),
        }
    }

    /// What a pane is, for a row with nothing more specific to say.
    fn kind_label(&self, id: PaneId) -> SharedString {
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Browser(_)) => "New page".into(),
            Some(PaneKind::Chat(_)) => "Chat".into(),
            Some(PaneKind::Settings(_)) => "Preferences".into(),
            Some(PaneKind::Launcher(_)) => "New tab".into(),
            Some(PaneKind::Review(_)) => "Task review".into(),
            Some(PaneKind::Page(_, PageKind::Usage)) => "Cost and tokens".into(),
            Some(PaneKind::Page(_, PageKind::Packages)) => "Dependencies".into(),
            Some(PaneKind::Terminal(_)) | None => "terminal".into(),
        }
    }

    fn pane_subtitle(&self, id: PaneId, cx: &App) -> Option<SharedString> {
        match &self.panes.get(&id)?.kind {
            PaneKind::Browser(b) => {
                let b = b.read(cx);
                (!b.is_blank()).then(|| crate::history::host(&b.url).to_string().into())
            }
            _ => self.places.get(&id).map(|p| p.label().into()),
        }
    }

    fn group(&self, tab: &Tab) -> Group {
        let first = tab.leaves()[0];
        match self.panes.get(&first) {
            Some(Pane { agent: Some(_), .. })
            | Some(Pane {
                kind: PaneKind::Chat(_),
                ..
            }) => Group::Threads,
            _ => Group::Workspace,
        }
    }

    // -- control plane ------------------------------------------------------

    fn handle(
        &mut self,
        req: control::Request,
        reply: Reply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let automated =
            !req.token.is_empty() && self.automation.as_deref() == Some(req.token.as_str());
        let caller = self
            .panes
            .iter()
            .find(|(_, p)| p.token.as_deref() == Some(req.token.as_str()))
            .map(|(id, _)| *id)
            .or_else(|| self.focused().filter(|_| automated));
        let p = &req.params;
        let Some(caller) = caller else {
            // Opening a page is the one thing a caller from outside may do:
            // it is what `open -a` would let any process do anyway.
            if req.method == "browser.open" {
                let r = match str_param(p, "url") {
                    Some(url) => self
                        .new_tab(Open::Browser { url: url.into() }, None, window, cx)
                        .map(|id| json!({ "pane": id }))
                        .map_err(|e| e.to_string()),
                    None => Err("missing URL".into()),
                };
                return reply.send(r);
            }
            return reply.send(Err(
                "unknown or expired token: run deck from a terminal inside a deck window".into(),
            ));
        };
        let r = match req.method.as_str() {
            "pane.list" => Ok(self.list(caller, cx)),
            "pane.split" => self.rpc_split(p, caller, window, cx),
            "pane.run" => self.rpc_run(p, caller, window, cx),
            "tab.new" => self.rpc_tab(p, window, cx),
            "pane.send" => self.rpc_send(p, cx),
            "pane.read" => return self.rpc_read(p, reply, cx),
            "pane.focus" => self.existing(p).map(|id| {
                self.focus_pane(id, window, cx);
                json!({ "pane": id })
            }),
            "pane.close" => self.existing(p).map(|id| {
                self.close_pane(id, window, cx);
                json!({ "closed": id })
            }),
            "job.start" => self.rpc_job_start(p, caller, window, cx),
            "job.list" => Ok(self.job_list(cx)),
            "job.show" => self.existing(p).map(|id| {
                self.show_job(id, window, cx);
                json!({ "shown": id })
            }),
            "job.stop" => self.existing(p).map(|id| {
                self.stop_job(id, window, cx);
                json!({ "stopped": id })
            }),
            "usage.get" => Ok(self.usage_json(cx)),
            "action.list" => Ok(json!({ "actions": self.palette_items(cx).into_iter().filter_map(|i| match i.pick {
                crate::palette::Pick::Action(_) => Some(json!({ "name": i.title.as_ref(), "about": i.detail.as_ref(), "keys": i.hint.as_ref().map(|h| h.as_ref()) })),
                _ => None,
            }).collect::<Vec<_>>() })),
            "action.run" => {
                if !self.settings.cli_actions {
                    return reply.send(Err("running Deck's actions from the CLI is off (Settings › Agents)".into()));
                }
                let want = str_param(p, "name").or_else(|| str_param(p, "text")).unwrap_or("").to_lowercase();
                let found = self.palette_items(cx).into_iter().find_map(|i| match i.pick {
                    crate::palette::Pick::Action(a) if i.title.to_lowercase() == want => Some((i.title, a)),
                    _ => None,
                });
                match found {
                    Some((title, a)) => {
                        window.dispatch_action(a, cx);
                        Ok(json!({ "ran": title.as_ref() }))
                    }
                    None => Err(format!("no action {want:?} (see `deck do --list`)")),
                }
            }
            "page.open" => {
                let kind = match str_param(p, "page").or_else(|| str_param(p, "text")).unwrap_or("usage") {
                    "usage" => PageKind::Usage,
                    "packages" | "deps" => PageKind::Packages,
                    other => return reply.send(Err(format!("no page {other:?}: usage or packages"))),
                };
                self.open_page(kind, window, cx);
                Ok(json!({ "page": kind.title() }))
            }
            "thread.history" => Ok(self.thread_history(p, cx)),
            "thread.resume" => self.thread_resume(p, window, cx),
            "pane.key" => self.rpc_key(p, cx),
            "remote.open" => {
                let host = str_param(p, "host").unwrap_or("").to_string();
                if host.is_empty() {
                    return reply.send(Err("which host? see `deck remote list`".into()));
                }
                let dir = str_param(p, "dir").unwrap_or("~").to_string();
                let agent = str_param(p, "agent").and_then(|a| AGENTS.iter().find(|x| x.cmd == a));
                let what = match (agent, p.get("chat").and_then(Value::as_bool).unwrap_or(false)) {
                    (Some(a), true) => RemoteOpen::Chat(a.cmd),
                    (Some(a), false) => RemoteOpen::Tui(a),
                    (None, _) => RemoteOpen::Terminal,
                };
                self.open_remote(&host, &dir, what, window, cx);
                Ok(json!({ "host": host, "dir": dir }))
            }
            "pane.diff" => self.existing(p).and_then(|id| {
                let (root, diff) = self.turn_diff(id)?;
                Ok(json!({ "pane": id, "root": root, "diff": diff }))
            }),
            "browser.open" => self.rpc_open(p, caller, window, cx),
            "browser.eval" => return self.rpc_eval(p, caller, reply, cx),
            "browser.logs" => self.rpc_logs(p, caller, cx),
            "browser.shot" => self.rpc_browser_shot(p, caller, window, cx),
            "browser.dialog" => self.rpc_dialog(p, caller, cx),
            "settings.open" => {
                use crate::settings_pane::Page;
                let page = match str_param(p, "page").unwrap_or("general") {
                    "general" | "account" | "updates" => Ok(Page::General),
                    "appearance" | "theme" => Ok(Page::Appearance),
                    "terminal" => Ok(Page::Terminal),
                    "browser" => Ok(Page::Browser),
                    "agents" => Ok(Page::Agents),
                    "models" => Ok(Page::Models),
                    "permissions" | "privacy" => Ok(Page::Permissions),
                    "keyboard" | "shortcuts" | "keys" => Ok(Page::Keyboard),
                    "snippets" => Ok(Page::Snippets),
                    other => Err(format!("no settings page {other}")),
                };
                page.map(|page| {
                    self.open_settings_at(page, window, cx);
                    json!({ "page": format!("{page:?}").to_lowercase() })
                })
            }
            "window.shot" => {
                let out = str_param(p, "out")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| control::socket_dir().join("shot.png"));
                crate::shot::capture(window, &out)
                    .map(|(w, h)| json!({ "path": out, "width": w, "height": h }))
            }
            "task.new" => return self.rpc_task_new(p, caller, reply, window, cx),
            "task.list" => Ok(json!({
                "tasks": self
                    .tasks
                    .iter()
                    .filter(|t| t.state == crate::tasks::State::Open)
                    .map(|t| self.task_json(t, cx))
                    .collect::<Vec<_>>()
            })),
            "task.show" => self.task_param(p).map(|t| {
                let mut v = self.task_json(&t, cx);
                if let Some(PaneKind::Chat(c)) = self
                    .task_pane(&t, cx)
                    .and_then(|id| self.panes.get(&id))
                    .map(|p| &p.kind)
                {
                    v["last_reply"] = json!(c.read(cx).last_reply());
                    v["waiting_for"] = json!(c.read(cx).waiting_for());
                }
                v
            }),
            "task.diff" => match self.task_param(p) {
                Ok(t) => {
                    cx.background_executor()
                        .spawn(async move {
                            reply.send(
                                crate::tasks::diff(&t).map(|d| json!({ "task": t.id, "diff": d })),
                            )
                        })
                        .detach();
                    return;
                }
                Err(e) => Err(e),
            },
            "task.send" => self.task_param(p).and_then(|t| {
                let text = str_param(p, "text")
                    .ok_or("missing the message")?
                    .to_string();
                let pane = self
                    .task_pane(&t, cx)
                    .ok_or("the task's agent is not open; open it from the sidebar")?;
                match &self.panes[&pane].kind {
                    PaneKind::Chat(c) => {
                        c.update(cx, |c, cx| c.send(text, cx));
                        Ok(json!({ "task": t.id, "pane": pane }))
                    }
                    PaneKind::Terminal(term) => {
                        term.update(cx, |term, cx| term.send(&text, true, cx));
                        Ok(json!({ "task": t.id, "pane": pane }))
                    }
                    _ => Err("the task's agent cannot take messages".into()),
                }
            }),
            "task.merge" | "task.discard" | "task.pr" => match self.task_param(p) {
                Ok(t) => {
                    let how = match req.method.as_str() {
                        "task.merge" => Finish::Merge,
                        "task.discard" => Finish::Discard,
                        _ => Finish::Pr,
                    };
                    let review = self.panes.iter().find_map(|(id, pane)| match &pane.kind {
                        PaneKind::Review(r) if r.read(cx).task.id == t.id => Some(*id),
                        _ => None,
                    });
                    return self.finish_task_with(review, t, how, Some(reply), window, cx);
                }
                Err(e) => Err(e),
            },
            m => Err(format!("unknown method {m:?}; run `deck help`")),
        };
        reply.send(r);
    }

    fn list(&self, caller: PaneId, cx: &App) -> Value {
        let mut panes = Vec::new();
        for (ix, tab) in self.tabs.iter().enumerate() {
            for id in tab.leaves() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let focused = ix == self.active && tab.focused == id;
                let mut v = match &pane.kind {
                    PaneKind::Terminal(_) => json!({ "kind": "terminal" }),
                    PaneKind::Browser(b) => {
                        json!({ "kind": "browser", "url": b.read(cx).url.as_ref() })
                    }
                    PaneKind::Chat(c) => {
                        let c = c.read(cx);
                        json!({ "kind": "chat", "agent": c.agent.as_str(), "working": c.busy(), "needs_you": c.status == ChatStatus::Waiting })
                    }
                    PaneKind::Settings(_) => json!({ "kind": "settings" }),
                    PaneKind::Launcher(_) => json!({ "kind": "new-tab" }),
                    PaneKind::Page(_, k) => json!({ "kind": k.title().to_lowercase() }),
                    PaneKind::Review(r) => json!({ "kind": "review", "task": r.read(cx).task.id }),
                };
                v["id"] = json!(id);
                v["tab"] = json!(ix + 1);
                v["status"] = json!(match self.status(id, cx) {
                    Some(Status::Working) => "working",
                    Some(Status::Attention) => "waiting",
                    Some(Status::Idle) => "idle",
                    None => "none",
                });
                if let Some(msg) = self.agent_activity(id, cx) {
                    v["activity"] = json!(msg);
                }
                v["title"] = json!(self.pane_title(id, cx).as_ref());
                v["focused"] = json!(focused);
                if let Some(a) = pane.agent {
                    v["agent"] = json!(a.cmd);
                }
                if tab.pinned && tab.root == Node::Leaf(id) {
                    v["control"] = json!(true);
                }
                if id == caller {
                    v["you"] = json!(true);
                }
                panes.push(v);
            }
        }
        json!({ "you": caller, "panes": panes })
    }

    fn existing(&self, p: &Value) -> Result<PaneId, String> {
        // `--name dev` finds the pane a `deck run --name dev` opened.
        if let Some(n) = str_param(p, "name") {
            return self
                .named
                .get(n)
                .copied()
                .filter(|id| self.panes.contains_key(id))
                .ok_or_else(|| format!("no pane named {n:?} (start one with `deck run --name {n} CMD` or `deck open --name {n} URL`)"));
        }
        let id = p
            .get("pane")
            .and_then(Value::as_u64)
            .ok_or("missing --pane N (see `deck pane list`)")?;
        self.panes
            .contains_key(&id)
            .then_some(id)
            .ok_or_else(|| format!("no pane {id} in this window"))
    }

    fn open_from(p: &Value) -> Result<(Open, Option<&'static Agent>), String> {
        if let Some(agent) = p.get("chat") {
            return Ok((
                Open::Chat {
                    agent: agent.as_str().map(Into::into),
                    cwd: str_param(p, "cwd").map(Into::into),
                },
                None,
            ));
        }
        if let Some(name) = str_param(p, "agent") {
            let agent = AGENTS
                .iter()
                .find(|a| a.cmd == name)
                .ok_or_else(|| format!("unknown agent {name:?}"))?;
            return Ok((
                Open::Terminal {
                    cmd: Some(agent.cmd.into()),
                    cwd: str_param(p, "cwd").map(Into::into),
                },
                Some(agent),
            ));
        }
        Ok(match str_param(p, "kind").unwrap_or("terminal") {
            "terminal" => (
                Open::Terminal {
                    cmd: str_param(p, "cmd").map(Into::into),
                    cwd: str_param(p, "cwd").map(Into::into),
                },
                None,
            ),
            "browser" => (
                Open::Browser {
                    url: str_param(p, "url").unwrap_or("about:blank").into(),
                },
                None,
            ),
            "chat" => (
                Open::Chat {
                    agent: None,
                    cwd: str_param(p, "cwd").map(Into::into),
                },
                None,
            ),
            k => return Err(format!("--kind is terminal, browser or chat, got {k:?}")),
        })
    }

    fn rpc_split(
        &mut self,
        p: &Value,
        caller: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let beside = match p.get("pane") {
            Some(_) => self.existing(p)?,
            None => caller,
        };
        let axis = match str_param(p, "dir").unwrap_or("right") {
            "right" => Axis::Row,
            "down" => Axis::Column,
            d => return Err(format!("--dir is right or down, got {d:?}")),
        };
        let (what, _) = Self::open_from(p)?;
        let id = self
            .split(beside, what, axis, window, cx)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "pane": id }))
    }

    /// `deck run [--name NAME] CMD`: a terminal beside the caller running
    /// CMD, where you can watch it. With a name it is kept: running the same
    /// name again stops what runs there (⌃C) and runs the new command in the
    /// same pane, so a dev server restarts in place instead of piling up.
    fn rpc_run(
        &mut self,
        p: &Value,
        caller: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let cmd = str_param(p, "cmd").filter(|c| !c.trim().is_empty()).ok_or("nothing to run")?.to_string();
        let name = str_param(p, "name").map(str::to_string);
        if let Some(id) = name.as_ref().and_then(|n| self.named.get(n)).copied() {
            if let Some(PaneKind::Terminal(t)) = self.panes.get(&id).map(|p| &p.kind) {
                let t = t.clone();
                t.update(cx, |t, cx| t.send("\u{3}", false, cx));
                // A moment for the program to stop and the shell to return.
                let line = cmd.clone();
                cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    let _ = t.update(cx, |t, cx| t.send(&line, true, cx));
                })
                .detach();
                return Ok(json!({ "pane": id, "name": name, "reused": true }));
            }
        }
        let axis = match str_param(p, "dir").unwrap_or("right") {
            "right" => Axis::Row,
            "down" => Axis::Column,
            d => return Err(format!("--dir is right or down, got {d:?}")),
        };
        let cwd = str_param(p, "cwd").map(PathBuf::from).filter(|c| c.is_dir()).or_else(|| self.cwd_of(caller, cx));
        let id = self
            .split(caller, Open::Terminal { cmd: Some(cmd), cwd }, axis, window, cx)
            .map_err(|e| e.to_string())?;
        if let Some(n) = &name {
            self.named.insert(n.clone(), id);
        }
        // The caller keeps the keyboard; the new pane is there to watch.
        self.focus_pane(caller, window, cx);
        Ok(json!({ "pane": id, "name": name, "reused": false }))
    }

    /// `deck job start`: a terminal outside every tab, running CMD. Its
    /// name finds it again; starting the same name restarts it in place.
    fn rpc_job_start(
        &mut self,
        p: &Value,
        caller: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let cmd = str_param(p, "cmd").filter(|c| !c.trim().is_empty()).ok_or("nothing to run")?.to_string();
        let name = str_param(p, "name").map(str::to_string).unwrap_or_else(|| job_name(&cmd));
        let cwd = str_param(p, "cwd").map(PathBuf::from).filter(|c| c.is_dir()).or_else(|| self.cwd_of(caller, cx));
        let id = self.start_job(name.clone(), cmd, cwd, window, cx).map_err(|e| e.to_string())?;
        // The caller keeps the keyboard; the job runs out of the way.
        self.focus_pane(caller, window, cx);
        cx.notify();
        Ok(json!({ "pane": id, "name": name, "restarted": false }))
    }

    fn start_job(
        &mut self,
        name: String,
        cmd: String,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<PaneId> {
        let cwd = cwd.unwrap_or_else(|| self.cwd.clone());
        let job = crate::daemon::start(&name, &cmd, &cwd, crate::daemon::job_env()).map_err(anyhow::Error::msg)?;
        let id = match self.named.get(&name).copied().filter(|id| self.jobs.contains(id)) {
            Some(id) => id,
            None => self.attach_pane(&job.name, &cwd, window, cx)?,
        };
        self.job_cmds.insert(id, cmd);
        self.daemon_jobs.retain(|j| j.name != job.name);
        self.daemon_jobs.push(job);
        cx.notify();
        Ok(id)
    }

    /// A terminal pane showing the background service's job `name`: it
    /// runs `deck job attach NAME`, so closing it (or quitting Deck) leaves
    /// the job running.
    fn attach_pane(&mut self, name: &str, cwd: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) -> anyhow::Result<PaneId> {
        let exe = std::env::current_exe()?;
        let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let cmd = format!("exec {} job attach {}", q(&exe.to_string_lossy()), q(name));
        let id = self.make_pane(Open::Terminal { cmd: Some(cmd), cwd: Some(cwd.to_path_buf()) }, None, window, cx)?;
        self.jobs.push(id);
        self.named.insert(name.to_string(), id);
        Ok(id)
    }

    /// The service's jobs, as just asked: a pane for each new one, and
    /// panes whose job is gone closed.
    fn sync_daemon(&mut self, jobs: Vec<crate::daemon::JobInfo>, window: &mut Window, cx: &mut Context<Self>) {
        let jobs: Vec<crate::daemon::JobInfo> = jobs.into_iter().filter(|j| !j.tab).collect();
        let changed = jobs != self.daemon_jobs;
        for j in &jobs {
            let has = self.named.get(&j.name).is_some_and(|id| self.panes.contains_key(id));
            if !has {
                if let Err(e) = self.attach_pane(&j.name, &j.cwd, window, cx) {
                    tracing::warn!("could not show job {}: {e}", j.name);
                }
            }
        }
        // A job pane whose job the service no longer has.
        let gone: Vec<PaneId> = self
            .jobs
            .iter()
            .copied()
            .filter(|id| matches!(self.panes.get(id).map(|p| &p.kind), Some(PaneKind::Terminal(_))))
            .filter(|id| {
                let name = self.named.iter().find(|(_, v)| *v == id).map(|(n, _)| n.as_str());
                name.is_none_or(|n| !jobs.iter().any(|j| j.name == n))
            })
            .collect();
        self.daemon_jobs = jobs;
        for id in gone {
            self.named.retain(|_, v| *v != id);
            self.close_pane(id, window, cx);
        }
        if let Some(name) = self.show_when_attached.take() {
            match self.named.get(&name).copied() {
                Some(id) => self.show_job(id, window, cx),
                None => self.show_when_attached = Some(name),
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// The service's record of the job in pane `id`.
    fn daemon_job(&self, id: PaneId) -> Option<&crate::daemon::JobInfo> {
        let name = self.named.iter().find(|(_, v)| **v == id).map(|(n, _)| n)?;
        self.daemon_jobs.iter().find(|j| &j.name == name)
    }

    fn job_list(&self, cx: &App) -> Value {
        let names: HashMap<PaneId, &str> = self.named.iter().map(|(n, id)| (*id, n.as_str())).collect();
        let jobs: Vec<Value> = self
            .jobs
            .iter()
            .filter_map(|&id| {
                if let PaneKind::Browser(b) = &self.panes.get(&id)?.kind {
                    return Some(json!({
                        "pane": id,
                        "name": names.get(&id).copied(),
                        "kind": "page",
                        "url": b.read(cx).url.as_ref(),
                    }));
                }
                let j = self.daemon_job(id)?;
                Some(json!({
                    "kind": if j.agent.is_some() { "agent" } else { "job" },
                    "pane": id,
                    "name": j.name,
                    "command": j.cmd,
                    "running": j.running,
                    "exit": j.exit,
                    "cwd": j.cwd,
                }))
            })
            .collect();
        json!({ "jobs": jobs })
    }

    /// Open a job in the drawer, or shut the drawer if it already shows it.
    fn show_job(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.jobs.contains(&id) {
            // A pane in a tab: go there.
            self.focus_pane(id, window, cx);
            return;
        }
        self.job_open = Some(id);
        if let Some(PaneKind::Terminal(t)) = self.panes.get(&id).map(|p| &p.kind) {
            let f = t.read(cx).focus.clone();
            window.focus(&f);
        }
        cx.notify();
    }

    fn stop_job(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(j) = self.daemon_job(id).cloned() {
            self.daemon_jobs.retain(|x| x.name != j.name);
            cx.background_executor().spawn(async move { crate::daemon::stop(&j.name) }).detach();
        }
        self.named.retain(|_, v| *v != id);
        self.close_pane(id, window, cx);
        cx.notify();
    }

    /// The drawer's job as a tab of its own: it leaves the status bar.
    fn job_to_tab(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.jobs.retain(|j| *j != id);
        if let Some(PaneKind::Browser(b)) = self.panes.get(&id).map(|p| &p.kind) {
            b.update(cx, |b, _| b.set_visible(true));
        }
        self.job_open = None;
        self.tabs.push(Tab {
            root: Node::Leaf(id),
            focused: id,
            pinned: false,
            on_top: false,
        });
        self.focus_pane(id, window, cx);
        cx.notify();
    }

    /// Ids of the chat threads open in this window.
    fn open_threads(&self, cx: &App) -> HashMap<String, PaneId> {
        self.panes
            .iter()
            .filter_map(|(&id, p)| match &p.kind {
                PaneKind::Chat(c) => Some((c.read(cx).thread_id().to_string(), id)),
                _ => None,
            })
            .collect()
    }

    /// `deck history [--query Q] [--limit N]`: saved threads, newest first,
    /// open, closed (archived) or past the relaunch window alike.
    fn thread_history(&self, p: &Value, cx: &App) -> Value {
        let q = str_param(p, "query").unwrap_or("").to_lowercase();
        let limit = p.get("limit").and_then(Value::as_u64).or_else(|| str_param(p, "limit").and_then(|l| l.parse().ok())).unwrap_or(30) as usize;
        let open = self.open_threads(cx);
        let mut all: Vec<(crate::threads::Saved, bool)> =
            crate::threads::load_all().into_iter().map(|s| (s, false)).collect();
        all.extend(crate::threads::load_archived().into_iter().map(|s| (s, true)));
        all.sort_by_key(|(s, _)| std::cmp::Reverse(s.updated));
        let rows: Vec<Value> = all
            .into_iter()
            .filter(|(s, _)| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.cwd.to_string_lossy().to_lowercase().contains(&q)
            })
            .take(limit)
            .map(|(s, archived)| {
                json!({
                    "id": s.id,
                    "title": s.title,
                    "agent": s.agent,
                    "cwd": s.cwd,
                    "updated": s.updated / 1000,
                    "messages": s.transcript.messages.len(),
                    "open_pane": open.get(&s.id),
                    "archived": archived,
                })
            })
            .collect();
        json!({ "threads": rows })
    }

    /// `deck resume ID`: the thread back in a tab of its own, or focused if
    /// it is open. An archived one comes out of the archive.
    fn thread_resume(&mut self, p: &Value, window: &mut Window, cx: &mut Context<Self>) -> Result<Value, String> {
        let want = str_param(p, "id").or_else(|| str_param(p, "text")).ok_or("which thread? see `deck history`")?;
        if let Some((_, &pane)) = self.open_threads(cx).iter().find(|(id, _)| id.starts_with(want)) {
            self.focus_pane(pane, window, cx);
            return Ok(json!({ "pane": pane, "reopened": false }));
        }
        let saved = crate::threads::load_all()
            .into_iter()
            .chain(crate::threads::load_archived())
            .filter(|s| s.id.starts_with(want))
            .max_by_key(|s| s.updated)
            .ok_or_else(|| format!("no thread {want:?} (see `deck history`)"))?;
        crate::threads::unarchive(&saved.id);
        let id = self.new_tab(Open::Thread(Box::new(saved)), None, window, cx).map_err(|e| e.to_string())?;
        Ok(json!({ "pane": id, "reopened": true }))
    }

    /// `deck pane key --pane N KEY`: a key press, by name.
    fn rpc_key(&mut self, p: &Value, cx: &mut Context<Self>) -> Result<Value, String> {
        let id = self.existing(p)?;
        let name = str_param(p, "key").or_else(|| str_param(p, "text")).ok_or("which key? e.g. ctrl+c, enter, escape, up")?;
        let seq = key_bytes(name).ok_or_else(|| format!("unknown key {name:?}"))?;
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Terminal(t)) => t.update(cx, |t, cx| t.send(&seq, false, cx)),
            _ => return Err(format!("pane {id} is not a terminal")),
        }
        Ok(json!({ "pane": id, "key": name }))
    }

    /// `deck usage`: what the usage panel shows, as JSON.
    fn usage_json(&self, cx: &App) -> Value {
        let limits = |l: &crate::usage::Limits| {
            json!({
                "agent": l.agent,
                "plan": l.plan,
                "limits": l.limits.iter().map(|x| json!({
                    "label": x.label,
                    "used_pct": x.used_pct,
                    "resets_in_secs": x.resets_at.and_then(|t| t.duration_since(std::time::SystemTime::now()).ok()).map(|d| d.as_secs()),
                })).collect::<Vec<_>>(),
            })
        };
        let us = &self.usage;
        json!({
            "claude": match &us.claude {
                Some(Ok(l)) => limits(l),
                Some(Err(e)) => json!({ "error": e }),
                None => Value::Null,
            },
            "codex": us.codex.as_ref().map(limits),
            "conversation": us.session.as_ref().map(|(title, s)| json!({
                "title": title.as_ref(),
                "model": s.model,
                "context_used": s.context_used,
                "context_window": s.context_window,
                "input": s.input, "output": s.output,
                "cache_read": s.cache_read, "cache_write": s.cache_write,
                "turns": s.turns, "cost_usd": s.cost_usd,
            })),
            "today": us.today.as_ref().map(|t| json!({
                "conversations": t.sessions,
                "input": t.input, "output": t.output,
                "cache_read": t.cache_read, "cache_write": t.cache_write,
                "cost_usd": t.cost_usd,
            })),
            "others": us.others.iter().map(|d| json!({
                "agent": d.agent, "tokens": d.tokens, "cost_usd": d.cost_usd,
                "replies": d.replies, "model": d.model,
            })).collect::<Vec<_>>(),
            "chats": self.chat_spend(cx).iter().map(|c| json!({
                "pane": c.pane, "title": c.title.as_ref(), "agent": c.agent.as_ref(),
                "tokens": c.usage.tokens(), "cost_usd": c.usage.cost_usd,
            })).collect::<Vec<_>>(),
        })
    }

    fn rpc_tab(
        &mut self,
        p: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let (what, agent) = Self::open_from(p)?;
        // `--kind browser --name app`: a page kept under that name; asking
        // for it again goes to the same pane.
        let name = str_param(p, "name").filter(|_| matches!(what, Open::Browser { .. }));
        if let Some(id) = name.and_then(|n| self.named_browser(n)) {
            let url = str_param(p, "url").filter(|u| !u.trim().is_empty() && *u != "about:blank");
            if let (Some(url), Some(PaneKind::Browser(b))) = (url, self.panes.get(&id).map(|p| &p.kind)) {
                b.update(cx, |b, cx| b.navigate(url, cx))
                    .map_err(|e| e.to_string())?;
            }
            self.focus_pane(id, window, cx);
            return Ok(json!({ "pane": id, "name": name, "reused": true }));
        }
        let id = self
            .new_tab(what, agent, window, cx)
            .map_err(|e| e.to_string())?;
        if let Some(n) = name {
            self.named.insert(n.to_string(), id);
        }
        Ok(json!({ "pane": id, "tab": self.active + 1 }))
    }

    fn rpc_send(&mut self, p: &Value, cx: &mut Context<Self>) -> Result<Value, String> {
        let id = self.existing(p)?;
        let text = str_param(p, "text").ok_or("nothing to send")?;
        let enter = p.get("enter").and_then(Value::as_bool).unwrap_or(true);
        match &self.panes[&id].kind {
            PaneKind::Terminal(t) => t.update(cx, |t, cx| t.send(text, enter, cx)),
            // Messaging a chat is how one agent hands work to another.
            PaneKind::Chat(c) => c.update(cx, |c, cx| c.send(text.to_string(), cx)),
            PaneKind::Browser(_) => {
                return Err(format!("pane {id} is a browser; use `deck browser eval`"))
            }
            PaneKind::Settings(_) => return Err(format!("pane {id} is the Settings tab")),
            PaneKind::Launcher(_) => return Err(format!("pane {id} is an empty new tab")),
            PaneKind::Review(_) => return Err(format!("pane {id} is a task review")),
            PaneKind::Page(_, k) => return Err(format!("pane {id} is the {} page", k.title())),
        }
        Ok(json!({ "pane": id }))
    }

    fn rpc_read(&mut self, p: &Value, reply: Reply, cx: &mut Context<Self>) {
        let id = match self.existing(p) {
            Ok(id) => id,
            Err(e) => return reply.send(Err(e)),
        };
        match &self.panes[&id].kind {
            PaneKind::Terminal(t) => {
                // --lines N: the end of the scrollback, for logs.
                let text = match p.get("lines").and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())) {
                    Some(n) => t.read(cx).tail(n.clamp(1, 5000) as usize),
                    None => t.update(cx, |t, _| t.read_text()),
                };
                reply.send(Ok(json!({ "pane": id, "text": text })));
            }
            PaneKind::Browser(b) => b
                .read(cx)
                .read_text(move |r| reply.send(r.map(|text| json!({ "pane": id, "text": text })))),
            PaneKind::Chat(c) => {
                let text = c.read(cx).read_text();
                reply.send(Ok(json!({ "pane": id, "text": text })));
            }
            PaneKind::Settings(_) => reply.send(Err(format!("pane {id} is the Settings tab"))),
            PaneKind::Launcher(_) => reply.send(Err(format!("pane {id} is an empty new tab"))),
            PaneKind::Review(_) => reply.send(Err(format!("pane {id} is a task review"))),
            PaneKind::Page(_, k) => reply.send(Err(format!("pane {id} is the {} page", k.title()))),
        }
    }

    fn rpc_open(
        &mut self,
        p: &Value,
        caller: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let url = str_param(p, "url").ok_or("missing URL")?;
        // `--name app` keeps the page under that name: opening it again
        // navigates the same pane, wherever it is.
        let name = str_param(p, "name").filter(|n| !n.trim().is_empty());
        let reuse = if p.get("pane").is_some_and(|v| !v.is_null()) {
            let id = p.get("pane").and_then(Value::as_u64).ok_or("--pane takes a number")?;
            Some(self.existing(&json!({ "pane": id }))?)
        } else {
            name.and_then(|n| self.named_browser(n))
        };
        if let Some(id) = reuse {
            let PaneKind::Browser(b) = &self.panes[&id].kind else {
                return Err(format!("pane {id} is not a browser"));
            };
            b.update(cx, |b, cx| b.navigate(url, cx))
                .map_err(|e| e.to_string())?;
            if let Some(n) = name {
                self.named.insert(n.to_string(), id);
            }
            return Ok(json!({ "pane": id, "name": name, "reused": true }));
        }
        let tab = ["tab", "new_tab"]
            .iter()
            .any(|k| p.get(*k).and_then(Value::as_bool) == Some(true));
        // A background page: no split, a tab in the status bar.
        if p.get("bg").and_then(Value::as_bool) == Some(true) {
            let id = self.make_pane(Open::Browser { url: url.into() }, None, window, cx).map_err(|e| e.to_string())?;
            if let Some(PaneKind::Browser(b)) = self.panes.get(&id).map(|p| &p.kind) {
                b.update(cx, |b, _| {
                    b.size_offscreen();
                    b.set_visible(false);
                });
            }
            self.jobs.push(id);
            let n = name.map_or_else(|| format!("page {id}"), str::to_string);
            self.named.insert(n.clone(), id);
            // The caller keeps the keyboard.
            self.focus_pane(caller, window, cx);
            cx.notify();
            return Ok(json!({ "pane": id, "name": n, "background": true }));
        }
        let id = if tab {
            self.new_tab(Open::Browser { url: url.into() }, None, window, cx)
                .map_err(|e| e.to_string())?
        } else if name.is_some() {
            // A named page is its own, not whatever page the tab has.
            self.split(caller, Open::Browser { url: url.into() }, Axis::Row, window, cx)
                .map_err(|e| e.to_string())?
        } else {
            self.open_url_near(caller, url, window, cx)
                .ok_or("could not open a browser pane")?
        };
        if let Some(n) = name {
            self.named.insert(n.to_string(), id);
        }
        Ok(json!({ "pane": id, "name": name }))
    }

    /// The browser pane kept under `name`, while it is open.
    fn named_browser(&self, name: &str) -> Option<PaneId> {
        self.named
            .get(name)
            .copied()
            .filter(|id| matches!(self.panes.get(id).map(|p| &p.kind), Some(PaneKind::Browser(_))))
    }

    /// The page a `browser.*` call is for: `--pane` or `--name`, else the
    /// page in the caller's tab, else the only page open.
    fn browser_target(&self, p: &Value, caller: PaneId, cx: &App) -> Result<PaneId, String> {
        let given = |k: &str| p.get(k).is_some_and(|v| !v.is_null());
        if given("pane") || given("name") {
            let id = self.existing(p)?;
            return match self.panes.get(&id).map(|p| &p.kind) {
                Some(PaneKind::Browser(_)) => Ok(id),
                _ => Err(format!("pane {id} is not a browser")),
            };
        }
        let page = |id: &PaneId| {
            matches!(self.panes.get(id).map(|p| &p.kind),
                Some(PaneKind::Browser(b)) if !b.read(cx).is_editor())
        };
        if let Some(tab) = self.tab_of(caller).map(|ix| &self.tabs[ix]) {
            if let Some(id) = std::iter::once(tab.focused).chain(tab.leaves()).find(|id| page(id)) {
                return Ok(id);
            }
        }
        let pages: Vec<PaneId> = self.panes.keys().copied().filter(|id| page(id)).collect();
        match pages.as_slice() {
            [one] => Ok(*one),
            [] => Err("no page is open; open one with `deck open URL`".into()),
            _ => Err("more than one page is open; say which with --pane N (see `deck pane list`)".into()),
        }
    }

    /// `browser.eval`: run JS in a page and answer with its JSON value.
    /// With `expr` it is one expression, run as written (deck's own page
    /// actions); otherwise it goes through `eval()`, and a page whose
    /// Content Security Policy forbids that gets it as an expression. A
    /// page that does not answer (a dialog is up, a script is spinning)
    /// fails the call after `timeout` seconds instead of holding it.
    fn rpc_eval(&mut self, p: &Value, caller: PaneId, reply: Reply, cx: &mut Context<Self>) {
        let res = self.browser_target(p, caller, cx).and_then(|id| {
            let js = str_param(p, "js").ok_or("missing JS")?;
            match &self.panes[&id].kind {
                PaneKind::Browser(b) => Ok((b.clone(), js.to_string())),
                _ => Err(format!("pane {id} is not a browser")),
            }
        });
        let (b, js) = match res {
            Ok(found) => found,
            Err(e) => return reply.send(Err(e)),
        };
        // A page with a dialog up runs nothing until it is answered.
        let held = |kind: &str, text: &str| {
            format!("the page is showing a {kind} ({text:?}) and waits for it; answer it with `deck browser dialog accept` or `deck browser dialog dismiss`")
        };
        if let Some((kind, text)) = b.read(cx).pending_dialog() {
            return reply.send(Err(held(kind, &text)));
        }
        let expr = p.get("expr").and_then(Value::as_bool).unwrap_or(false);
        let secs = p.get("timeout").and_then(Value::as_u64).unwrap_or(30).clamp(1, 300);
        let (tx, rx) = futures::channel::oneshot::channel::<Result<Value, String>>();
        let done = move |r| {
            let _ = tx.send(r);
        };
        if expr {
            b.read(cx).eval_expr(&js, done);
        } else {
            b.read(cx).eval(&js, done);
        }
        cx.spawn(async move |_, cx| {
            let start = std::time::Instant::now();
            let mut rx = rx;
            let r = loop {
                let tick = cx.background_executor().timer(Duration::from_millis(500));
                match futures::future::select(rx, tick).await {
                    futures::future::Either::Left((r, _)) => {
                        break r.unwrap_or_else(|_| Err("no answer from the page".into()))
                    }
                    futures::future::Either::Right((_, waiting)) => {
                        rx = waiting;
                        let dialog = b.update(cx, |b, _| b.pending_dialog()).ok().flatten();
                        if let Some((kind, text)) = dialog {
                            break Err(held(kind, &text));
                        }
                        if start.elapsed() >= Duration::from_secs(secs) {
                            break Err(format!(
                                "the page did not answer in {secs}s; a script may be busy"
                            ));
                        }
                    }
                }
            };
            match r {
                Err(e) if !expr && e.starts_with("EvalError") => {
                    let sent = b.update(cx, |b, _| {
                        b.eval_expr(&js, move |r| reply.send(r.map(|v| json!({ "value": v }))))
                    });
                    drop(sent);
                }
                r => reply.send(r.map(|v| json!({ "value": v }))),
            }
        })
        .detach();
    }

    /// `browser.dialog`: the alert, confirm, prompt or login a page has up,
    /// and with `action` accept or dismiss, its answer (`text` for a
    /// prompt, `user` and `password` for a login).
    fn rpc_dialog(&mut self, p: &Value, caller: PaneId, cx: &mut Context<Self>) -> Result<Value, String> {
        let id = self.browser_target(p, caller, cx)?;
        let PaneKind::Browser(b) = &self.panes[&id].kind else {
            return Err(format!("pane {id} is not a browser"));
        };
        let b = b.read(cx);
        let accept = match str_param(p, "action") {
            None => {
                let open = b.pending_dialog();
                return Ok(json!({
                    "pane": id,
                    "dialog": open.map(|(kind, text)| json!({ "kind": kind, "text": text })),
                }));
            }
            Some("accept" | "ok" | "yes") => true,
            Some("dismiss" | "cancel" | "no") => false,
            Some(a) => return Err(format!("a dialog is answered accept or dismiss, not {a:?}")),
        };
        let values: Vec<String> = match (str_param(p, "user"), str_param(p, "text")) {
            (Some(user), _) => vec![user.into(), str_param(p, "password").unwrap_or("").into()],
            (None, Some(text)) => vec![text.into()],
            (None, None) => Vec::new(),
        };
        let mut v = b.answer_dialog(accept, &values)?;
        v["pane"] = json!(id);
        Ok(v)
    }

    /// `browser.logs`: what a page logged, threw and loaded, as the
    /// recorder in `browser.rs` saw it.
    fn rpc_logs(&mut self, p: &Value, caller: PaneId, cx: &mut Context<Self>) -> Result<Value, String> {
        let id = self.browser_target(p, caller, cx)?;
        let PaneKind::Browser(b) = &self.panes[&id].kind else {
            return Err(format!("pane {id} is not a browser"));
        };
        let q = crate::browser::LogQuery::from_params(p)?;
        let mut v = b.update(cx, |b, _| b.logs(&q));
        v["pane"] = json!(id);
        Ok(v)
    }

    /// `browser.shot`: a PNG of one page, cut from the window's picture.
    fn rpc_browser_shot(
        &mut self,
        p: &Value,
        caller: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let id = self.browser_target(p, caller, cx)?;
        let PaneKind::Browser(b) = &self.panes[&id].kind else {
            return Err(format!("pane {id} is not a browser"));
        };
        let area = b.read(cx).page_bounds().ok_or_else(|| {
            format!("pane {id}'s page is not on screen; bring it up with `deck pane focus --pane {id}`")
        })?;
        let out = str_param(p, "out")
            .map(PathBuf::from)
            .unwrap_or_else(|| control::socket_dir().join(format!("pane-{id}.png")));
        crate::shot::capture_area(window, area, &out)
            .map(|(w, h)| json!({ "pane": id, "path": out, "width": w, "height": h }))
    }

    // -- rendering ----------------------------------------------------------

    fn render_node(&self, node: &Node, multi: bool, cx: &mut Context<Self>) -> AnyElement {
        match node {
            Node::Leaf(id) => self.render_leaf(*id, multi, cx),
            Node::Split(axis, children, sizes) => {
                // Each pane is its own card, a gutter apart, so the split
                // reads as separate surfaces rather than ruled cells. The
                // gutter is the divider you drag.
                let mut d = div().flex().size_full();
                d = match axis {
                    Axis::Row => d.flex_row(),
                    Axis::Column => d.flex_col(),
                };
                for (i, c) in children.iter().enumerate() {
                    if i > 0 {
                        d = d.child(self.render_divider(
                            *axis,
                            children[i - 1].first(),
                            c.first(),
                            cx,
                        ));
                    }
                    // Where each cell lands, so a drag knows the pixels it
                    // is sharing out.
                    let key = c.first();
                    let cells = self.cells.clone();
                    let mut cell = div()
                        .relative()
                        .flex()
                        .flex_basis(u(0.))
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .child(self.render_node(c, multi, cx))
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    cells.borrow_mut().insert(key, bounds);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        );
                    cell.style().flex_grow = Some(sizes.get(i).copied().unwrap_or(1.));
                    cell.style().flex_shrink = Some(1.);
                    d = d.child(cell);
                }
                d.into_any_element()
            }
        }
    }

    /// The gutter between two panes of a split, as a handle: a short line
    /// shows on hover, a drag moves the boundary, a double-click evens the
    /// split out again.
    fn render_divider(
        &self,
        axis: Axis,
        a: PaneId,
        b: PaneId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = axis == Axis::Row;
        let active = self.resizing.as_ref().is_some_and(|r| r.a == a && r.b == b);
        let group = SharedString::from(format!("divider-{a}-{b}"));
        div()
            .id(ElementId::Name(group.clone()))
            .group(group.clone())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .when(row, |d| d.w(u(theme::GUTTER)).h_full().cursor_col_resize())
            .when(!row, |d| d.h(u(theme::GUTTER)).w_full().cursor_row_resize())
            .child(
                div()
                    .rounded(u(1.))
                    .bg(rgba(theme::focus_edge()))
                    .when(row, |d| d.w(u(2.)).h(u(32.)))
                    .when(!row, |d| d.h(u(2.)).w(u(32.)))
                    .when(!active, |d| {
                        d.opacity(0.).group_hover(group, |s| s.opacity(1.))
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, ev: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let Some(tab) = ws.tabs.get_mut(ws.active) else {
                        return;
                    };
                    if ev.click_count == 2 {
                        tab.root.even(a, b);
                        cx.notify();
                        return;
                    }
                    let Some((wa, wb)) = tab.root.weights(a, b) else {
                        return;
                    };
                    let cells = ws.cells.borrow();
                    let (Some(ba), Some(bb)) = (cells.get(&a), cells.get(&b)) else {
                        return;
                    };
                    let span = if row {
                        ba.size.width + bb.size.width
                    } else {
                        ba.size.height + bb.size.height
                    };
                    let start = if row { ev.position.x } else { ev.position.y };
                    let r = Resize {
                        row,
                        a,
                        b,
                        start: f32::from(start),
                        wa,
                        wb,
                        span: f32::from(span),
                    };
                    drop(cells);
                    ws.resizing = Some(r);
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// A side panel's inner edge as a grip: the gutter beside it, with a
    /// line on hover. `left`: the sidebar's; else the inspector's.
    fn side_handle(&self, left: bool, cx: &mut Context<Self>) -> AnyElement {
        let group = SharedString::from(if left { "side-grip-l" } else { "side-grip-r" });
        let active = self.side_drag.is_some_and(|(l, ..)| l == left);
        let at = if left { self.sidebar_px } else { self.inspector_px } - theme::GUTTER / 2. - 3.;
        div()
            .id(ElementId::Name(group.clone()))
            .group(group.clone())
            .absolute()
            .top(px(theme::TITLEBAR_PX))
            .bottom(px(0.))
            .w(u(6.))
            .when(left, |d| d.left(u(at)))
            .when(!left, |d| d.right(u(at)))
            .flex()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(u(2.))
                    .h_full()
                    .rounded(u(1.))
                    .bg(rgba(theme::focus_edge()))
                    .when(!active, |d| d.opacity(0.).group_hover(group, |s| s.opacity(0.6))),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, ev: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if ev.click_count == 2 {
                        if left {
                            ws.sidebar_px = theme::SIDEBAR_PX;
                        } else {
                            ws.inspector_px = theme::INSPECTOR_PX;
                        }
                        cx.notify();
                        return;
                    }
                    let from = if left { ws.sidebar_px } else { ws.inspector_px };
                    ws.side_drag = Some((left, f32::from(ev.position.x), from));
                }),
            )
            .into_any_element()
    }

    fn drag_side(&mut self, ev: &gpui::MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((left, start, from)) = self.side_drag else {
            return;
        };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.side_drag = None;
            cx.notify();
            return;
        }
        let moved = (f32::from(ev.position.x) - start) / self.settings.zoom;
        if left {
            self.sidebar_px = (from + moved).clamp(SIDEBAR_MIN, SIDEBAR_MAX).round();
        } else {
            self.inspector_px = (from - moved).clamp(INSPECTOR_MIN, INSPECTOR_MAX).round();
        }
        cx.notify();
    }

    /// The pointer moved while a divider is held: share the two panes'
    /// pixels out again, keeping each at least a usable width.
    fn drag_divider(&mut self, ev: &gpui::MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = &self.resizing else {
            return;
        };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.resizing = None;
            cx.notify();
            return;
        }
        let at = f32::from(if r.row { ev.position.x } else { ev.position.y });
        let total = r.wa + r.wb;
        let least = (r.span * 0.15).min(160.);
        let a_px = (r.span * r.wa / total + at - r.start).clamp(least, r.span - least);
        let wa = total * a_px / r.span;
        let (a, b) = (r.a, r.b);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.root.resize(a, b, wa, total - wa) {
                cx.notify();
            }
        }
    }

    fn render_leaf(&self, id: PaneId, multi: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(pane) = self.panes.get(&id) else {
            return div().into_any_element();
        };
        let focused = self.focused() == Some(id);
        // Cached: a pane redraws when it changes, not whenever anything in
        // the window does. Terminal output then costs that terminal's frame
        // alone, and a sidebar tick leaves every pane's layout untouched.
        let view: AnyView = match &pane.kind {
            PaneKind::Terminal(t) => t.clone().into(),
            PaneKind::Browser(b) => b.clone().into(),
            PaneKind::Chat(c) => c.clone().into(),
            PaneKind::Settings(s) => s.clone().into(),
            PaneKind::Launcher(l) => l.clone().into(),
            PaneKind::Review(r) => r.clone().into(),
            PaneKind::Page(v, _) => v.clone(),
        };
        let body = view
            .cached(StyleRefinement::default().size_full())
            .into_any_element();
        div()
            .relative()
            .size_full()
            .bg(rgb(theme::pane()))
            // In a split, the focused card has the brighter edge: where
            // keys go, without a stripe or fading the others.
            .when(multi, |d| {
                d.rounded(u(theme::CARD_RADIUS))
                    .overflow_hidden()
                    .border_1()
                    .border_color(rgba(if focused {
                        theme::focus_edge()
                    } else {
                        theme::hairline()
                    }))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |ws, _, window, cx| {
                    if ws.focused() != Some(id) {
                        ws.focus_pane(id, window, cx)
                    }
                }),
            )
            // A right-click acts on the pane it lands in, so its menu does.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |ws, _, window, cx| {
                    if ws.focused() != Some(id) {
                        ws.focus_pane(id, window, cx)
                    }
                }),
            )
            .child(body)
            .into_any_element()
    }

    /// A page's own icon for its tab, once deck has it.
    fn tab_favicon(&self, first: PaneId, cx: &App) -> Option<Arc<gpui::Image>> {
        match &self.panes.get(&first)?.kind {
            PaneKind::Browser(b) => b.read(cx).favicon(),
            _ => None,
        }
    }

    fn tab_icon(&self, first: PaneId, cx: &App) -> (Icon, u32) {
        match self.panes.get(&first) {
            Some(Pane { agent: Some(a), .. }) => (Icon::agent(a.cmd), theme::text()),
            Some(Pane {
                kind: PaneKind::Chat(c),
                ..
            }) => (Icon::agent(c.read(cx).agent.as_str()), theme::text()),
            Some(Pane {
                kind: PaneKind::Browser(b),
                ..
            }) => (if b.read(cx).is_editor() { Icon::Code } else { Icon::Globe }, theme::text()),
            Some(Pane {
                kind: PaneKind::Settings(_),
                ..
            }) => (Icon::Settings, theme::text()),
            Some(Pane {
                kind: PaneKind::Launcher(_),
                ..
            }) => (Icon::Plus, theme::text_2()),
            Some(Pane {
                kind: PaneKind::Review(_),
                ..
            }) => (Icon::Git, theme::text()),
            Some(Pane {
                kind: PaneKind::Page(_, k),
                ..
            }) => (
                match k {
                    PageKind::Usage => Icon::Chart,
                    PageKind::Packages => Icon::Package,
                },
                theme::text(),
            ),
            _ => (Icon::Terminal, theme::text_2()),
        }
    }

    /// The sidebar is an inbox: the control chat first, then
    /// threads with what each one needs from you, then the terminals and
    /// pages they opened. Empty sections are not drawn.
    /// The project a tab belongs to: its first pane's (a terminal's or
    /// chat's folder, an editor's root). Pages and the rest go under "Web".
    fn tab_project(&self, tab: &Tab, cx: &App) -> (String, Option<PathBuf>) {
        let first = tab.leaves()[0];
        if let Some(place) = self.places.get(&first) {
            let root = crate::inspect::project_root(&place.cwd);
            return (place.project.clone(), Some(root));
        }
        if let Some(PaneKind::Browser(b)) = self.panes.get(&first).map(|p| &p.kind) {
            if let Some(e) = b.read(cx).editor() {
                let name = e.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return (name, Some(e.root));
            }
        }
        (String::new(), None)
    }

    /// The sidebar's projects, in order (the first one opened first), each
    /// with its tabs: its chats and agents, most recent first, then the rest
    /// as they were opened. Tabs in no project come last, as "Web".
    fn project_groups(&self, cx: &App) -> Vec<ProjectGroup> {
        let mut groups: Vec<ProjectGroup> = Vec::new();
        let mut loose = ProjectGroup { name: String::new(), root: None, threads: Vec::new(), others: Vec::new() };
        let settings = self.settings_tab();
        for (ix, tab) in self.tabs.iter().enumerate() {
            // The control chat has its own row; a task's work, its task's;
            // Settings, the footer.
            if tab.pinned || tab.on_top || self.is_task_tab(tab, cx) || Some(ix) == settings {
                continue;
            }
            let (name, root) = self.tab_project(tab, cx);
            let group = if name.is_empty() {
                &mut loose
            } else {
                match groups.iter().position(|g| g.name == name) {
                    Some(i) => &mut groups[i],
                    None => {
                        groups.push(ProjectGroup { name, root, threads: Vec::new(), others: Vec::new() });
                        groups.last_mut().expect("just pushed")
                    }
                }
            };
            match self.group(tab) {
                Group::Threads => group.threads.push((ix, self.settled_at(tab, cx))),
                Group::Workspace => group.others.push(ix),
            }
        }
        for g in &mut groups {
            g.threads.sort_by_key(|t| std::cmp::Reverse(t.1));
        }
        loose.threads.sort_by_key(|t| std::cmp::Reverse(t.1));
        if !loose.threads.is_empty() || !loose.others.is_empty() {
            groups.push(loose);
        }
        groups
    }

    /// ⌘1 opens Control; ⌘2–⌘9 a project: the tab in it you were last
    /// in (its most recent chat when there is one).
    fn go_to_place(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if n == 1 {
            match self.control_tab() {
                Some(ix) => self.activate(ix, window, cx),
                None => self.new_control(window, cx),
            }
            return;
        }
        let groups = self.project_groups(cx);
        let Some(g) = groups.get(n - 2).or(if n == 9 { groups.last() } else { None }) else {
            return;
        };
        let ix = g
            .tabs()
            .max_by_key(|&ix| self.tabs.get(ix).map(|t| self.tab_touched(t, cx)))
            .or_else(|| g.tabs().next());
        if let Some(ix) = ix {
            self.folded.remove(&g.name);
            self.activate(ix, window, cx);
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let groups = self.project_groups(cx);
        let header = sidebar_header;

        let mut list = div()
            .id("tabs")
            .flex()
            .flex_col()
            .flex_1()
            .gap(u(2.))
            .overflow_y_scroll()
            .px(u(theme::GUTTER))
            .pb(u(theme::GUTTER))
            .children(self.filter.as_ref().map(|(f, _)| {
                div()
                    .flex()
                    .items_center()
                    .gap(u(ROW_GAP))
                    .h(u(ROW_ONE_LINE))
                    .pl(u(theme::ROW_PAD))
                    .pr(u(ROW_END + 4.))
                    .mb(u(4.))
                    .rounded(u(theme::ROW_RADIUS))
                    .bg(rgb(theme::surface()))
                    .child(lead(icon(Icon::Search, 14., theme::text_2())))
                    .child(div().flex_1().child(f.clone()))
                    .into_any_element()
            }));

        let open: Vec<crate::tasks::Task> = self
            .tasks
            .iter()
            .filter(|t| t.state == crate::tasks::State::Open)
            .cloned()
            .collect();
        if !open.is_empty() {
            list = list.child(header("Tasks"));
            for t in &open {
                list = list.child(self.render_task_row(t, cx));
            }
        }

        list = list.children(self.render_pinned(cx));
        // New agents: the picker, under its own small header.
        if !groups.is_empty() {
            list = list.child(self.render_view_header(cx));
        } else {
            list = list.child(self.render_sidebar_empty(cx));
        }
        if self.recent {
            list = list.children(self.render_recent(&groups, cx));
        }
        // Folded projects stack close as one list; an open one gets room
        // after its rows, so groups read by space rather than lines.
        let mut prev_open = false;
        for (gi, g) in groups.iter().enumerate().filter(|_| !self.recent) {
            let gap = if gi == 0 { 2. } else if prev_open { 12. } else { 0. };
            list = list.child(self.render_project_header(g, gi, gap, cx));
            prev_open = !self.folded.contains(&g.name);
            if self.folded.contains(&g.name) {
                continue;
            }
            // In the order they were opened: a row never jumps because its
            // agent printed something or because claude started in it.
            let mut ixs: Vec<usize> = g.tabs().collect();
            ixs.sort_unstable();
            for ix in ixs {
                list = list.child(if g.threads.iter().any(|t| t.0 == ix) {
                    self.render_thread_row(ix, None, cx)
                } else {
                    self.render_tab_row(ix, &self.tabs[ix], None, cx)
                });
            }
        }

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(u(self.sidebar_px))
            .h_full()
            .child(
                // The traffic lights live here; the native titlebar under it
                // is the drag handle. Real pixels: the lights do not zoom.
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .h(px(theme::TITLEBAR_PX))
                    .px(u(theme::GUTTER))
                    .on_mouse_down(MouseButton::Left, |ev, window, _| {
                        if ev.click_count == 2 {
                            window.titlebar_double_click();
                        }
                    })
                    .gap(u(2.))
                    .child(self.new_chat_button("new-chat", cx))
                    .child(
                        icon_button("hide-sidebar", Icon::Sidebar).on_click(cx.listener(
                            |ws, _, _, cx| {
                                ws.sidebar = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(list)
            .children((!self.picked.is_empty()).then(|| self.render_pick_bar(cx)))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(u(2.))
                    .mx(u(theme::GUTTER))
                    .pt(u(6.))
                    .pb(u(theme::GUTTER))
                    .border_t_1()
                    .border_color(rgba(theme::hairline()))
                    .child(self.render_sidebar_foot(cx)),
            )
            .into_any_element()
    }

    /// The sidebar's foot: one quiet row. On the left, icons for the
    /// project's packages, usage (with how full the context is) and
    /// Settings; on the right, how many are working, waiting on you and
    /// done, each a click from the next one.
    fn render_sidebar_foot(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut working = 0;
        let mut waiting = 0;
        let mut done = 0;
        for t in &self.tabs {
            for id in t.leaves() {
                match self.status(id, cx) {
                    Some(Status::Working) => working += 1,
                    Some(Status::Attention) => waiting += 1,
                    _ if self.unseen.contains(&id) => done += 1,
                    _ => {}
                }
            }
        }
        let settings_on = self.settings_tab() == Some(self.active);
        let page_on = |k: PageKind| {
            self.focused().is_some_and(|id| matches!(self.panes.get(&id).map(|p| &p.kind), Some(PaneKind::Page(_, kk)) if *kk == k))
        };
        let tool = |id: &'static str, glyph: Icon, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .gap(u(4.))
                .h(u(30.))
                .min_w(u(30.))
                .px(u(6.))
                .rounded(u(7.))
                .cursor_pointer()
                .text_size(u(11.))
                .text_color(rgb(theme::dim()))
                .when(on, |d| d.bg(rgb(theme::row_active())))
                .hover(|s| s.bg(rgb(theme::row_hover())))
                .child(icon(glyph, 15., if on { theme::text() } else { theme::text_2() }))
        };
        let glance = self.usage_glance(cx);
        let count = |id: &'static str, mark: AnyElement, n: usize| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(4.))
                .h(u(26.))
                .px(u(6.))
                .rounded(u(6.))
                .cursor_pointer()
                .text_size(u(12.5))
                .text_color(rgb(theme::text_2()))
                .hover(|s| s.bg(rgb(theme::row_hover())))
                .child(mark)
                .child(n.to_string())
        };
        let done_mark = div()
            .size(u(13.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::text()))
            .child(icon(Icon::Check, 8., theme::sidebar()))
            .into_any_element();
        div()
            .flex()
            .items_center()
            .gap(u(2.))
            .h(u(34.))
            .child(
                tool("foot-packages", Icon::Package, page_on(PageKind::Packages))
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_page(PageKind::Packages, w, cx))),
            )
            .child(
                // Hover: the numbers in a small card. Click: the full panel.
                div()
                    .relative()
                    .child(
                        tool("foot-usage", Icon::Gauge, self.usage_open || page_on(PageKind::Usage))
                            .children(glance.map(|(_, pct)| div().text_color(rgb(level_color(pct))).child(pct_text(pct))))
                            .on_hover(cx.listener(|ws, on: &bool, _, cx| {
                                if ws.gauge_hover != *on {
                                    ws.gauge_hover = *on;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(|ws, _, _, cx| {
                                ws.gauge_hover = false;
                                ws.toggle_usage(true, cx)
                            })),
                    )
                    .when(self.gauge_hover && !self.usage_open, |d| d.child(gpui::deferred(self.render_usage_card()).with_priority(2))),
            )
            .child(tool("foot-settings", Icon::Settings, settings_on).on_click(cx.listener(|ws, _, w, cx| ws.open_settings(w, cx))))
            .child(div().flex_1())
            .when(working > 0, |d| {
                d.child(
                    count("foot-working", spinner("foot-spin", 12., theme::accent()), working)
                        .on_click(cx.listener(|ws, _, w, cx| ws.next_with(Status::Working, w, cx))),
                )
            })
            .when(waiting > 0, |d| {
                d.child(
                    count("foot-waiting", icon(Icon::Bell, 12., theme::accent()), waiting)
                        .on_click(cx.listener(|ws, _, w, cx| ws.next_needs_you(w, cx))),
                )
            })
            .when(done > 0, |d| {
                d.child(count("foot-done", done_mark, done).on_click(cx.listener(|ws, _, w, cx| {
                    let next = ws.tabs.iter().flat_map(|t| t.leaves()).find(|id| ws.unseen.contains(id));
                    if let Some(id) = next {
                        ws.focus_pane(id, w, cx);
                    }
                })))
            })
            .into_any_element()
    }

    /// The next pane in this state after the one you are in.
    fn next_with(&mut self, want: Status, window: &mut Window, cx: &mut Context<Self>) {
        let order: Vec<PaneId> = self.tabs.iter().flat_map(|t| t.leaves()).collect();
        let at = self.focused().and_then(|f| order.iter().position(|p| *p == f)).unwrap_or(0);
        if let Some(id) = (1..=order.len()).map(|i| order[(at + i) % order.len()]).find(|id| self.status(*id, cx) == Some(want)) {
            self.focus_pane(id, window, cx);
        }
    }

    /// "3 selected · Close · Clear", while rows are picked.
    fn render_pick_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.picked.len();
        let button = |id: &'static str, label: &'static str, strong: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(u(26.))
                .px(u(10.))
                .rounded(u(6.))
                .text_size(u(12.))
                .cursor_pointer()
                .when(strong, |d| d.bg(rgb(theme::text())).text_color(rgb(theme::sidebar())).hover(|s| s.opacity(0.88)))
                .when(!strong, |d| d.text_color(rgb(theme::text_2())).hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text()))))
                .child(label)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(u(4.))
            .mx(u(theme::GUTTER))
            .mb(u(6.))
            .h(u(38.))
            .pl(u(12.))
            .pr(u(6.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .text_size(u(12.5))
            .child(div().flex_1().text_color(rgb(theme::text())).child(format!("{n} selected")))
            .child(button("pick-clear", "Clear", false).on_click(cx.listener(|ws, _, _, cx| {
                ws.picked.clear();
                cx.notify();
            })))
            .child(button("pick-close", "Close", true).on_click(cx.listener(|ws, _, w, cx| ws.close_picked(w, cx))))
            .with_animation(
                "pick-bar-in",
                Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                |d, t| d.opacity(t).mt(u(4. * (1. - t))),
            )
            .into_any_element()
    }

    /// The Settings tab while it is open on its own. The sidebar's footer
    /// stands for it then, rather than a second Settings row in the list.
    fn settings_tab(&self) -> Option<usize> {
        self.tabs.iter().position(|t| match t.root {
            Node::Leaf(id) => matches!(self.panes.get(&id).map(|p| &p.kind), Some(PaneKind::Settings(_))),
            Node::Split(..) => false,
        })
    }

    /// Settings, at the foot of the sidebar: opens it, and while it is open
    /// is its row, lifted like any open row and closed the same way.
    #[allow(dead_code)]
    fn render_settings_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.settings_tab();
        let active = tab == Some(self.active);
        let tone = if active { theme::text() } else { theme::text_2() };
        sidebar_row(div().id("open-settings"), 0., active, false)
            .group("settings-row")
            .h(u(ROW_ONE_LINE))
            .pr(u(ROW_END + 3.))
            .text_size(u(13.))
            .text_color(rgb(tone))
            .child(lead(icon(Icon::Settings, 15., tone)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .group_hover("settings-row", |s| s.text_color(rgb(theme::text())))
                    .child("Settings"),
            )
            .child(
                // The shortcut, swapped for close while hovered once open.
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .relative()
                    .min_w(u(22.))
                    .h(u(22.))
                    .children(keycap(hint("settings", &self.settings.keys)).map(|k| {
                        k.when(tab.is_some(), |d| d.group_hover("settings-row", |s| s.opacity(0.)))
                    }))
                    .when_some(tab, |d, ix| {
                        d.child(
                            icon_button("close-settings", Icon::Close)
                                .absolute()
                                .right_0()
                                .size(u(22.))
                                .opacity(0.)
                                .group_hover("settings-row", |s| s.opacity(1.))
                                .on_click(cx.listener(move |ws, _, window, cx| {
                                    cx.stop_propagation();
                                    ws.close_tab(ix, window, cx);
                                })),
                        )
                    }),
            )
            .on_click(cx.listener(|ws, _, window, cx| ws.open_settings(window, cx)))
            .when_some(tab, |d, ix| {
                d.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |ws, ev: &gpui::MouseDownEvent, window, cx| {
                        ws.open_tab_menu(ix, ev.position, window, cx)
                    }),
                )
            })
            .into_any_element()
    }

    /// The sidebar with nothing open yet: the few ways to start, each with
    /// its shortcut, instead of empty space.
    fn render_sidebar_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let row = |id: &'static str, glyph: Icon, label: &'static str, key: Option<SharedString>| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(u(ROW_GAP))
                .h(u(32.))
                .px(u(theme::ROW_PAD))
                .rounded(u(theme::ROW_RADIUS))
                .text_size(u(13.))
                .text_color(rgb(theme::text_2()))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                .child(icon(glyph, 14., theme::text_2()))
                .child(div().flex_1().child(label))
                .children(key.and_then(keycap))
        };
        let keys = &self.settings.keys;
        div()
            .flex()
            .flex_col()
            .gap(u(2.))
            .mt(u(18.))
            .child(
                div()
                    .px(u(theme::ROW_PAD))
                    .pb(u(8.))
                    .text_size(u(12.))
                    .line_height(u(17.))
                    .text_color(rgb(theme::dim()))
                    .child("Chats, terminals and pages you open show up here, grouped by project."),
            )
            .child(row("empty-term", Icon::Terminal, "New terminal", Some(hint("new-terminal", keys))).on_click(cx.listener(
                |ws, _, w, cx| {
                    let what = ws.shell_here(cx);
                    let _ = ws.new_tab(what, None, w, cx);
                },
            )))
            .child(row("empty-files", Icon::Code, "Open the editor", Some(hint("files", keys))).on_click(cx.listener(
                |ws, _, w, cx| {
                    let _ = ws.open_editor("files", None, None, w, cx);
                },
            )))
            .child(row("empty-web", Icon::Globe, "New browser tab", Some(hint("new-browser", keys))).on_click(cx.listener(
                |ws, _, w, cx| {
                    if ws.new_tab(Open::Browser { url: "about:blank".into() }, None, w, cx).is_ok() {
                        ws.focus_address(w, cx);
                    }
                },
            )))
            .child(
                row("empty-agents", Icon::Plus, "Start an agent…", Some(hint("agents", keys))).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|ws, ev: &gpui::MouseDownEvent, w, cx| {
                        cx.stop_propagation();
                        ws.open_agent_menu(ev.position, w, cx);
                    }),
                ),
            )
            .into_any_element()
    }

    /// A project's header: its name, whether anything in it works or needs
    /// you, its ⌘ number, and (on hover) a new chat or terminal there. A
    /// click folds it.
    /// A project's header: the fold chevron in the icon column and the name
    /// on the same edge as every row's title, so the list keeps two edges.
    /// Folded it says how many tabs it holds; hovered, it offers a new chat
    /// or terminal there.
    /// Projects or Recent: two marks in one small track, the one in use
    /// lifted like an open row.
    /// The list's header: "Recent ⌄" or "Projects ⌄", a click switches
    /// between them; the + for a new agent shows while the pointer is over
    /// the header, so at rest it is just a label like the others.
    fn render_view_header(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("view-head")
            .group("view-head")
            .flex()
            .items_center()
            .h(u(30.))
            .mt(u(16.))
            .pl(u(theme::ROW_PAD))
            .pr(u(ROW_END))
            .text_size(u(HEAD_TEXT))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::dim()))
            .child(
                div()
                    .id("view-toggle")
                    .flex()
                    .items_center()
                    .gap(u(4.))
                    .h(u(22.))
                    .px(u(6.))
                    .ml(u(-6.))
                    .rounded(u(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text_2())))
                    .child(if self.recent { "Recent" } else { "Projects" })
                    .child(icon(Icon::ChevronDown, 9., theme::dim()))
                    .on_click(cx.listener(|ws, _, _, cx| {
                        let recent = !ws.recent;
                        if recent {
                            ws.settled.clear();
                            ws.settle(cx);
                        }
                        ws.recent = recent;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                icon_button("agents-plus", Icon::Plus)
                    .size(u(22.))
                    .opacity(0.)
                    .group_hover("view-head", |s| s.opacity(1.))
                    .on_mouse_down(MouseButton::Left, cx.listener(|ws, ev: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        ws.open_agent_menu(ev.position, window, cx);
                    })),
            )
            .into_any_element()
    }

    #[allow(dead_code)]
    fn render_view_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let seg = |id: &'static str, glyph: Icon, recent: bool| {
            let on = self.recent == recent;
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(u(20.))
                .rounded(u(5.))
                .border_1()
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(theme::row_active())).border_color(rgba(theme::hairline())))
                .when(!on, |d| {
                    d.border_color(gpui::transparent_black())
                        .hover(|s| s.bg(rgb(theme::row_hover())))
                })
                .child(icon(glyph, 12., if on { theme::text() } else { theme::dim() }))
                .on_click(cx.listener(move |ws, _, _, cx| {
                    if recent && !ws.recent {
                        ws.settled.clear();
                        ws.settle(cx);
                    }
                    ws.recent = recent;
                    cx.notify();
                }))
        };
        let projects = seg("view-projects", Icon::Folder, false);
        let recent = seg("view-recent", Icon::Clock, true);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(u(1.))
            .p(u(1.))
            .mr(u(2.))
            .rounded(u(6.))
            .bg(rgba(theme::tint()))
            .child(projects)
            .child(recent)
            .into_any_element()
    }

    /// The sidebar by time: each row under the day it last did something,
    /// newest first. Its place is fixed once shown (see `settle`), so an
    /// agent starting or finishing changes its row, never where it is.
    fn render_recent(&self, groups: &[ProjectGroup], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (_, (h, m, sec)) = crate::retry::now();
        let since_midnight = u64::from(h * 3600 + m * 60 + sec);
        let now = crate::local::now();
        let mut rows: Vec<(Bucket, i64, usize, bool, &str)> = Vec::new();
        for g in groups {
            for ix in g.tabs() {
                let Some(tab) = self.tabs.get(ix) else { continue };
                let at = self.settled_at(tab, cx);
                let bucket = Bucket::of((now - at).max(0) as u64, since_midnight);
                let thread = g.threads.iter().any(|t| t.0 == ix);
                rows.push((bucket, at, ix, thread, g.name.as_str()));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let mut out = Vec::new();
        let mut last = None;
        for &(bucket, _, ix, thread, project) in &rows {
            if last != Some(bucket) {
                let first = last.is_none();
                last = Some(bucket);
                let n = rows.iter().filter(|r| r.0 == bucket).count();
                // The first day sits right under "Recent ⌄", not a header
                // after a header.
                out.push(
                    sidebar_header(bucket.label())
                        .mt(u(if first { 2. } else { 16. }))
                        .child(header_count(n))
                        .into_any_element(),
                );
            }
            out.push(if thread {
                self.render_thread_row(ix, Some(project), cx)
            } else {
                self.render_tab_row(ix, &self.tabs[ix], Some(project), cx)
            });
        }
        out
    }

    /// The rows you pinned, above every project, in the order they opened.
    fn render_pinned(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let pins: Vec<usize> = (0..self.tabs.len()).filter(|&ix| self.tabs[ix].on_top).collect();
        if pins.is_empty() {
            return Vec::new();
        }
        let mut out = vec![sidebar_header("Pinned").into_any_element()];
        for ix in pins {
            let tab = &self.tabs[ix];
            let project = self.tab_project(tab, cx).0;
            out.push(if self.group(tab) == Group::Threads {
                self.render_thread_row(ix, Some(&project), cx)
            } else {
                self.render_tab_row(ix, tab, Some(&project), cx)
            });
        }
        out
    }

    fn render_project_header(&self, g: &ProjectGroup, gi: usize, gap: f32, cx: &mut Context<Self>) -> AnyElement {
        let name = g.name.clone();
        let loose = name.is_empty();
        let folded = self.folded.contains(&name);
        let statuses: Vec<Option<Status>> = g
            .tabs()
            .filter_map(|ix| self.tabs.get(ix))
            .flat_map(|t| t.leaves())
            .map(|id| self.status(id, cx))
            .collect();
        let working = statuses.contains(&Some(Status::Working));
        let wants = statuses.contains(&Some(Status::Attention));
        // The dot takes the colour of the agent at work, as its row does.
        let busy_color = g
            .tabs()
            .filter_map(|ix| self.tabs.get(ix))
            .flat_map(|t| t.leaves())
            .find(|id| self.status(*id, cx) == Some(Status::Working))
            .map(|id| live(self.tab_icon(id, cx).0))
            .unwrap_or_else(theme::accent);
        let here = g.tabs().any(|ix| ix == self.active);
        let count = g.threads.len() + g.others.len();
        let hover = SharedString::from(format!("proj-{gi}"));
        let root = g.root.clone();
        let (r1, r2) = (root.clone(), root);
        let label: SharedString = if !loose {
            name.clone().into()
        } else if self.loose_is_web(g, cx) {
            "Web".into()
        } else {
            "Other".into()
        };
        let toggle = name.clone();
        div()
            .id(ElementId::NamedInteger("project".into(), gi as u64))
            .group(hover.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(u(ROW_GAP))
            .h(u(30.))
            .mt(u(gap))
            .pl(u(theme::ROW_PAD))
            .pr(u(ROW_END))
            .rounded(u(theme::ROW_RADIUS))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::row_hover())))
            .on_click(cx.listener(move |ws, _, _, cx| {
                if !ws.folded.remove(&toggle) {
                    ws.folded.insert(toggle.clone());
                }
                cx.notify();
            }))
            .child(lead(icon(
                if folded { Icon::ChevronRight } else { Icon::ChevronDown },
                12.,
                theme::dim(),
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .text_size(u(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(if here { theme::text() } else { theme::text_2() }))
                    .child(label),
            )
            // What's going on inside, readable even when folded.
            .when(wants, |d| d.child(div().flex_none().size(u(6.)).rounded_full().bg(rgb(theme::DANGER))))
            .when(working && !wants, |d| {
                d.child(div().flex_none().size(u(6.)).rounded_full().bg(rgb(busy_color)))
            })
            .child(
                // The count while folded, swapped for actions on hover.
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .min_w(u(22.))
                    .relative()
                    .child(
                        div()
                            .when(!folded, |d| d.invisible())
                            .when(!loose, |d| d.group_hover(hover.clone(), |s| s.invisible()))
                            .pr(u(6.))
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child(count.to_string()),
                    )
                    .when(!loose, |d| {
                        d.child(
                            div()
                                .absolute()
                                .right_0()
                                .flex()
                                .gap(u(2.))
                                .invisible()
                                .group_hover(hover.clone(), |s| s.visible())
                                .child(
                                    icon_button(SharedString::from(format!("proj-chat-{gi}")), Icon::Plus)
                                        .size(u(22.))
                                        .on_click(cx.listener(move |ws, _, w, cx| {
                                            cx.stop_propagation();
                                            let what = Open::Chat { agent: None, cwd: r1.clone() };
                                            if ws.new_tab(what, None, w, cx).is_err() {
                                                let at = ws.agent_menu_at();
                                                ws.open_agent_menu(at, w, cx);
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    icon_button(SharedString::from(format!("proj-term-{gi}")), Icon::Terminal)
                                        .size(u(22.))
                                        .on_click(cx.listener(move |ws, _, w, cx| {
                                            cx.stop_propagation();
                                            let what = Open::Terminal { cmd: None, cwd: r2.clone() };
                                            let _ = ws.new_tab(what, None, w, cx);
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The loose group holds only pages: call it Web, else Other.
    fn loose_is_web(&self, g: &ProjectGroup, _cx: &App) -> bool {
        g.tabs().filter_map(|ix| self.tabs.get(ix)).all(|t| {
            t.leaves()
                .iter()
                .all(|id| matches!(self.panes.get(id).map(|p| &p.kind), Some(PaneKind::Browser(_))))
        })
    }

    /// A shell's row: what runs in it, the program's own title, or the
    /// folder; then where it is in the project. Never the same thing twice.
    fn terminal_row_text(&self, id: PaneId, cx: &App) -> Option<(SharedString, SharedString)> {
        let pane = self.panes.get(&id)?;
        if pane.agent.is_some() {
            return None;
        }
        let PaneKind::Terminal(t) = &pane.kind else { return None };
        let t = t.read(cx);
        let place = self.places.get(&id);
        let folder = place.map(|p| if p.sub.is_empty() { p.project.clone() } else { p.sub.clone() });
        let running = t
            .job()
            .filter(|b| b.running())
            .and_then(|b| b.command)
            .map(|c| c.lines().next().unwrap_or("").chars().take(60).collect::<String>());
        let own = clean_title(&t.title).filter(|ti| {
            !is_shell(ti)
                && place.and_then(|p| p.front.as_deref()).is_none_or(|f| !is_shell(f) || !ti.eq_ignore_ascii_case(f))
                && !ti.starts_with('~')
                && !ti.starts_with('/')
        });
        let own = own.map(|t| self.own_agent_title(id, t, cx));
        let title = running
            .clone()
            .or(own)
            .or_else(|| folder.clone())
            .unwrap_or_else(|| "Terminal".into());
        // An agent typed into the shell: say which one.
        let agent = place
            .and_then(|p| p.agent.as_ref())
            .and_then(|(cmd, _)| AGENTS.iter().find(|a| a.cmd == *cmd))
            .map(|a| a.name.to_string());
        let sub = match (&running, place) {
            _ if agent.is_some() => agent.unwrap_or_default(),
            (Some(_), Some(p)) => p.label(),
            (None, Some(p)) if !p.sub.is_empty() && title != p.sub => p.label(),
            _ => "Terminal".into(),
        };
        let sub = if sub == title { "Terminal".to_string() } else { sub };
        Some((title.into(), sub.into()))
    }

    /// When a pane last did something, in seconds since the epoch.
    fn last_active(&self, id: PaneId, cx: &App) -> i64 {
        let touched = self.touched.get(&id).copied();
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Chat(c)) => touched.unwrap_or(0).max(c.read(cx).updated / 1000),
            _ => touched.unwrap_or_else(crate::local::now),
        }
    }

    /// When a tab last did something: the latest of its panes.
    fn tab_touched(&self, tab: &Tab, cx: &App) -> i64 {
        tab.leaves()
            .into_iter()
            .map(|id| self.last_active(id, cx))
            .max()
            .unwrap_or_else(crate::local::now)
    }

    /// Where a tab sits in the sidebar's Recent view: the time it had when
    /// the list first showed it, or its own while it is that new.
    fn settled_at(&self, tab: &Tab, cx: &App) -> i64 {
        self.settled
            .get(&tab.leaves()[0])
            .copied()
            .unwrap_or_else(|| self.tab_touched(tab, cx))
    }

    /// Fix each new tab's place in the Recent view, and forget closed ones.
    /// A row never moves on its own: an agent starting or finishing, or
    /// you looking at it, leaves it where it is. Choosing Recent again
    /// sorts afresh.
    fn settle(&mut self, cx: &mut Context<Self>) {
        let firsts: Vec<(PaneId, i64)> = self
            .tabs
            .iter()
            .map(|tab| (tab.leaves()[0], self.settled_at(tab, cx)))
            .collect();
        let changed = firsts.len() != self.settled.len()
            || firsts.iter().any(|(id, _)| !self.settled.contains_key(id));
        if changed {
            self.settled = firsts.into_iter().collect();
            cx.notify();
        }
    }

    /// What a thread needs from you, in a few words, or how long ago it ran.
    fn thread_state(&self, id: PaneId, cx: &App) -> (SharedString, bool) {
        let unseen = self.unseen.contains(&id);
        match self.status(id, cx) {
            Some(Status::Working) => {
                let secs = match self.panes.get(&id).map(|p| &p.kind) {
                    Some(PaneKind::Chat(c)) => c.read(cx).working_for().map(|d| d.as_secs()),
                    _ => None,
                };
                (
                    secs.map_or("Working".into(), |s| {
                        format!("Working {}", short_secs(s)).into()
                    }),
                    true,
                )
            }
            Some(Status::Attention) => ("Needs you".into(), true),
            _ if unseen => ("Done".into(), true),
            _ => (ago(self.last_active(id, cx)).into(), false),
        }
    }

    /// + in the titlebar: a new chat with the default agent in the focused
    /// folder, or the agent menu when none can start.
    fn new_chat_button(&self, id: &'static str, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        icon_button(id, Icon::Plus).on_click(cx.listener(|ws, _, window, cx| {
            let what = Open::Chat { agent: None, cwd: ws.focused_cwd(cx) };
            if ws.new_tab(what, None, window, cx).is_err() {
                let at = ws.agent_menu_at();
                ws.open_agent_menu(at, window, cx);
                cx.notify();
            }
        }))
    }

    /// A thread: its title, then who is doing it and what it needs from you.
    /// A chat or agent's row. `place` is its project, for the Recent view,
    /// where rows stand flush and say which project they are in.
    fn render_thread_row(&self, ix: usize, place: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        if !self.row_matches(ix, cx) {
            return div().into_any_element();
        }
        let indent = if place.is_some() { 0. } else { ROW_INDENT };
        let Some(tab) = self.tabs.get(ix) else {
            return div().into_any_element();
        };
        let active = ix == self.active;
        let first = tab.leaves()[0];
        let (glyph, _) = self.tab_icon(first, cx);
        let project = self.places.get(&first).map(|p| p.project.clone());
        let who: SharedString = match self.panes.get(&first) {
            Some(Pane {
                kind: PaneKind::Chat(c),
                ..
            }) => {
                let _ = &project;
                c.read(cx).agent_name()
            }
            Some(Pane { agent: Some(a), .. }) => format!("{} · terminal", a.name).into(),
            _ => "".into(),
        };
        let working = self.status(first, cx) == Some(Status::Working);
        let wants = self.status(first, cx) == Some(Status::Attention);
        // While it works or waits, the second line says on what.
        let who: SharedString = match (working || wants)
            .then(|| self.agent_activity(first, cx))
            .flatten()
        {
            Some(doing) => doing.into(),
            None => who,
        };
        let (_, loud) = self.thread_state(first, cx);
        let group = SharedString::from(format!("tab-{ix}"));
        sidebar_row(
            div().id(ElementId::NamedInteger("tab".into(), ix as u64)),
            indent,
            active,
            !loud,
        )
        .when(self.row_picked(ix), picked_row)
        .group(group.clone())
        .h(u(ROW_H))
            .child(lead(if working {
                thinking(
                    ElementId::NamedInteger("thread-think".into(), ix as u64),
                    glyph,
                    15.,
                    live(glyph),
                )
            } else {
                icon(
                    glyph,
                    15.,
                    if wants {
                        live(glyph)
                    } else if active || loud {
                        theme::text()
                    } else {
                        theme::text_2()
                    },
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .flex_row()
                    .items_baseline()
                    .gap(u(6.))
                    .child(
                        div()
                            .flex_shrink()
                            .min_w_0()
                            .one_line()
                            .text_size(u(ROW_TEXT))
                            .when(loud && !working && !wants, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .text_color(rgb(if active || loud {
                                theme::text()
                            } else {
                                theme::text_2()
                            }))
                            .child(self.pane_title(tab.focused, cx)),
                    )
                    // A quiet tag: what it is doing while it works or waits;
                    // else, in Recent where no header names it, the project.
                    .children(
                        if working || wants { Some(who.to_string()) } else { place.map(str::to_string) }
                            .filter(|p| !p.is_empty())
                            .map(|p| {
                                div().flex_shrink().min_w_0().max_w(u(150.)).one_line().text_size(u(12.)).text_color(rgb(theme::dim())).child(p)
                            }),
                    ),
            )
            .child(
                // The state, swapped for the close button while hovered.
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .min_w(u(TRAIL_W))
                    .relative()
                    .child(
                        div()
                            .group_hover(group.clone(), |s| s.opacity(0.))
                            .child(row_trailer(
                                ElementId::NamedInteger("thread-mark".into(), ix as u64),
                                if working { RowState::Working } else if wants { RowState::Wants } else if loud { RowState::Done } else { RowState::Idle },
                                ago(self.last_active(first, cx)),
                            )),
                    )
                    .child(
                        icon_button(
                            ElementId::NamedInteger("close".into(), ix as u64),
                            Icon::Close,
                        )
                        .absolute()
                        .right_0()
                        .size(u(22.))
                        .opacity(0.)
                        .group_hover(group, |s| s.opacity(1.))
                        .on_click(cx.listener(move |ws, _, window, cx| {
                            cx.stop_propagation();
                            ws.close_tab(ix, window, cx);
                        })),
                    ),
            )
            .on_click(cx.listener(move |ws, ev: &gpui::ClickEvent, window, cx| ws.row_click(ix, ev.modifiers(), window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |ws, ev: &gpui::MouseDownEvent, window, cx| {
                    ws.open_tab_menu(ix, ev.position, window, cx)
                }),
            )
            .into_any_element()
    }

    /// New agents as a menu: a chat with each one that has one, its own
    /// terminal UI for the rest; ones not installed are listed, greyed.
    fn open_agent_menu(&mut self, at: gpui::Point<gpui::Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::menu::{Entry, Item};
        let ws = cx.entity().downgrade();
        let mut chats: Vec<Entry> = Vec::new();
        let mut tuis: Vec<Entry> = Vec::new();
        let mut missing: Vec<Entry> = Vec::new();
        for agent in AGENTS {
            let installed = self.installed.iter().any(|a| a.cmd == agent.cmd);
            let chat = self
                .chats
                .as_ref()
                .and_then(|env| env.registry.get(&agent_core::AgentId::new(agent.cmd)))
                .is_some_and(|e| e.is_ready());
            if !installed {
                missing.push(
                    Item::new(agent.name, |_, _| {})
                        .icon(Icon::agent(agent.cmd))
                        .shortcut("not installed")
                        .disabled(true)
                        .into(),
                );
                continue;
            }
            let open = |terminal: bool| {
                let w = ws.clone();
                move |window: &mut Window, cx: &mut App| {
                    let _ = w.update(cx, |ws, cx| {
                        let cwd = ws.focused_cwd(cx);
                        let what = if terminal {
                            Open::Terminal { cmd: Some(agent.cmd.into()), cwd }
                        } else {
                            Open::Chat { agent: Some(agent.cmd.into()), cwd }
                        };
                        let _ = ws.new_tab(what, terminal.then_some(agent), window, cx);
                    });
                }
            };
            if chat {
                chats.push(Item::new(agent.name, open(false)).icon(Icon::agent(agent.cmd)).shortcut("chat").into());
            }
            tuis.push(Item::new(agent.name, open(true)).icon(Icon::agent(agent.cmd)).shortcut("terminal").into());
        }
        let mut entries = chats;
        if !tuis.is_empty() {
            if !entries.is_empty() {
                entries.push(Entry::Separator);
            }
            entries.extend(tuis);
        }
        if !missing.is_empty() {
            entries.push(Entry::Separator);
            entries.extend(missing);
        }
        let restore = window.focused(cx);
        self.menu = Some(crate::menu::open(entries, at, restore, window, cx, |ws: &mut Self, _, cx| {
            ws.menu = None;
            cx.notify();
        }));
        cx.notify();
    }

    /// Where the agents menu opens when no click placed it: under the
    /// sidebar's New chat button.
    fn agent_menu_at(&self) -> gpui::Point<gpui::Pixels> {
        let z = self.settings.zoom;
        gpui::point(px(theme::GUTTER * z), px(theme::TITLEBAR_PX * z + 44. * z))
    }

    fn render_tab_row(&self, ix: usize, tab: &Tab, place: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        if !self.row_matches(ix, cx) {
            return div().into_any_element();
        }
        let indent = if place.is_some() { 0. } else { ROW_INDENT };
        let active = ix == self.active;
        let first = tab.leaves()[0];
        let (glyph, tint) = self.tab_icon(first, cx);
        let status = self.status(first, cx);
        let count = tab.leaves().len();
        let group = SharedString::from(format!("tab-{ix}"));
        // Every row has its second line, so they all stand the same height.
        let shell = self.terminal_row_text(tab.focused, cx);
        let subtitle = Some(match &shell {
            Some((_, sub)) => sub.clone(),
            None => self.row_detail(tab.focused, cx).unwrap_or_else(|| self.kind_label(tab.focused)),
        });
        // A split says so on its second line, not as one more thing on the
        // row's edge, where the time and close button line up.
        let _subtitle: Option<SharedString> = match (subtitle, count) {
            (Some(s), n) if n > 1 => Some(format!("{s} · {n} panes").into()),
            (s, _) => s,
        };
        let row_title = shell.map(|(t, _)| t).unwrap_or_else(|| self.pane_title(tab.focused, cx));
        let is_page = matches!(
            self.panes.get(&first).map(|p| &p.kind),
            Some(PaneKind::Browser(_))
        );
        let tone = if active { tint } else { theme::text_2() };
        // A loading page spins where its icon goes, as tab strips do; a busy
        // terminal spins at the end of its row.
        let loading = match self.panes.get(&first).map(|p| &p.kind) {
            Some(PaneKind::Browser(b)) => {
                let b = b.read(cx);
                b.loading && !b.is_blank()
            }
            _ => false,
        };
        let busy = status == Some(Status::Working) || self.busy.contains(&first);
        let wants = status == Some(Status::Attention);
        // An agent typed into a shell is shown as that agent, like a thread.
        let agent = self
            .places
            .get(&first)
            .and_then(|p| p.agent.as_ref())
            .map(|(cmd, _)| Icon::agent(cmd));
        // An agent you have not looked at since it finished says so; any
        // row in the Recent view, and an agent's anywhere, says how long
        // ago it last did something.
        let when = if busy || wants {
            None
        } else if self.unseen.contains(&first) {
            Some(pill("Done".into()))
        } else if place.is_some() || agent.is_some() {
            Some(
                div()
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child(ago(self.tab_touched(tab, cx))),
            )
        } else {
            None
        };
        let timed = when.is_some();
        // Activity always shows in the icon's place: the agent's mark turns,
        // a busy command or a loading page spins.
        let mark = match (loading, busy, agent, self.tab_favicon(first, cx)) {
            (true, ..) => spinner(
                ElementId::NamedInteger("tab-load".into(), ix as u64),
                14.,
                theme::text_2(),
            ),
            (_, true, Some(a), _) => thinking(
                ElementId::NamedInteger("tab-think".into(), ix as u64),
                a,
                15.,
                live(a),
            ),
            (_, true, None, _) if !is_page => spinner(
                ElementId::NamedInteger("tab-busy".into(), ix as u64),
                14.,
                theme::accent(),
            ),
            (_, _, Some(a), _) => icon(a, 15., tone),
            (_, _, _, Some(image)) => crate::browser::site_image(image, 15.),
            _ => icon(glyph, 15., tone),
        };

        sidebar_row(
            div().id(ElementId::NamedInteger("tab".into(), ix as u64)),
            indent,
            active,
            !busy && !wants,
        )
        .when(self.row_picked(ix), picked_row)
        .group(group.clone())
        .h(u(ROW_H))
        .text_size(u(ROW_TEXT))
        .text_color(rgb(if active || busy || wants {
            theme::text()
        } else {
            theme::text_2()
        }))
            .child(lead(mark))
            // Definite-width boxes, so gpui knows where to put the ellipsis.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .flex_row()
                    .items_baseline()
                    .gap(u(6.))
                    .child(div().flex_shrink().min_w_0().one_line().child(row_title))
                    .children(place.filter(|p| !p.is_empty()).map(|p| {
                        div().flex_none().max_w(u(90.)).one_line().text_size(u(12.)).text_color(rgb(theme::dim())).child(p.to_string())
                    })),
            )
            .child(
                // Done or how long ago, swapped for the close button while
                // hovered. One width for every row, so times line up.
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .min_w(u(TRAIL_W))
                    .relative()
                    .when_some(when, |d, _| {
                        d.child(div().group_hover(group.clone(), |s| s.opacity(0.)).child(row_trailer(
                            ElementId::NamedInteger("tab-mark".into(), ix as u64),
                            if busy { RowState::Working } else if wants { RowState::Wants } else { RowState::Idle },
                            ago(self.last_active(first, cx)),
                        )))
                    })
                    .child(
                        icon_button(
                            ElementId::NamedInteger("close".into(), ix as u64),
                            Icon::Close,
                        )
                        .when(timed, |b| b.absolute().right_0())
                        .size(u(22.))
                        .opacity(0.)
                        .group_hover(group, |s| s.opacity(1.))
                        .on_click(cx.listener(move |ws, _, window, cx| {
                            cx.stop_propagation();
                            ws.close_tab(ix, window, cx);
                        })),
                    ),
            )
            .on_click(cx.listener(move |ws, ev: &gpui::ClickEvent, window, cx| ws.row_click(ix, ev.modifiers(), window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |ws, ev: &gpui::MouseDownEvent, window, cx| {
                    ws.open_tab_menu(ix, ev.position, window, cx)
                }),
            )
            .into_any_element()
    }

    /// The status bar: one strip along the window's bottom edge. Where you
    /// are (branch, project) on the left; on the right what runs in the
    /// background (commands in any tab, pages), the dev servers on this Mac
    /// and the AI usage meter. Each item is a click away from its pane.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.focused();
        let names: HashMap<PaneId, &str> = self.named.iter().map(|(n, id)| (*id, n.as_str())).collect();
        let green = theme::palette().ansi[2];
        let red = theme::DANGER;
        let item = |id: ElementId, on: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(u(5.))
                .h(u(20.))
                .px(u(6.))
                .rounded(u(5.))
                .cursor_pointer()
                .text_color(rgb(if on { theme::text() } else { theme::text_2() }))
                .when(on, |d| d.bg(rgb(theme::surface())))
                .when(!on, |d| d.hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text()))))
        };
        let dot = |color: u32| div().flex_none().size(u(6.)).rounded_full().bg(rgb(color));
        let ring = |color: u32| {
            div()
                .flex_none()
                .size(u(6.))
                .rounded_full()
                .border_1()
                .border_color(rgb(color))
        };
        let sep = || div().flex_none().w(px(1.)).h(u(12.)).mx(u(6.)).bg(rgba(theme::hairline()));

        // Running commands and pages, in tab order.
        let mut jobs: Vec<AnyElement> = Vec::new();
        // Which pane shows each local port, so a port is listed once.
        let mut port_pane: HashMap<u16, PaneId> = HashMap::new();
        for tab in &self.tabs {
            for id in tab.leaves() {
                let on = focused == Some(id);
                match self.panes.get(&id).map(|p| &p.kind) {
                    Some(PaneKind::Terminal(t)) => {
                        let name = names.get(&id).copied();
                        // An agent has its own row in the sidebar.
                        if name.is_none() && self.places.get(&id).is_some_and(|p| p.agent.is_some()) {
                            continue;
                        }
                        let job = t.read(cx).job();
                        // Editors, pagers and other full-screen programs are
                        // where you are, not something running for you.
                        let program = job
                            .as_ref()
                            .and_then(|b| b.command.as_deref())
                            .and_then(|c| c.split_whitespace().find(|w| !w.contains('=') && *w != "sudo"))
                            .map(|w| w.rsplit('/').next().unwrap_or(w).to_string())
                            .unwrap_or_default();
                        if name.is_none() && (AGENTS.iter().any(|a| a.cmd == program) || interactive(&program)) {
                            continue;
                        }
                        let running = job.as_ref().is_some_and(|b| b.running());
                        let failed = job.as_ref().is_some_and(|b| !b.running() && b.exit.is_some_and(|c| c != 0));
                        // Idle shells stay out; named panes and anything
                        // running or just failed show.
                        if name.is_none() && !running && !failed {
                            continue;
                        }
                        let cmd = job.as_ref().and_then(|b| b.command.clone()).unwrap_or_default();
                        let cmd: String = cmd.lines().next().unwrap_or("").chars().take(28).collect();
                        let label = match name {
                            Some(n) if !cmd.is_empty() => format!("{n} · {cmd}"),
                            Some(n) => n.to_string(),
                            None => cmd,
                        };
                        let state = if running { green } else if failed { red } else { theme::dim() };
                        jobs.push(
                            item(ElementId::NamedInteger("bar-pane".into(), id), on)
                                .child(dot(state))
                                .child(div().max_w(u(200.)).one_line().child(label))
                                .on_click(cx.listener(move |ws, _, w, cx| ws.focus_pane(id, w, cx)))
                                .into_any_element(),
                        );
                    }
                    Some(PaneKind::Browser(b)) => {
                        let b = b.read(cx);
                        if b.is_blank() || b.is_editor() {
                            continue;
                        }
                        // A local page is its port's item, on the right.
                        if let Some(port) = url_port(&b.url) {
                            port_pane.entry(port).or_insert(id);
                            continue;
                        }
                        let host = crate::history::host(&b.url).to_string();
                        let mark = match b.favicon() {
                            Some(img) => crate::browser::site_image(img, 12.),
                            None => icon(Icon::Globe, 12., theme::dim()),
                        };
                        jobs.push(
                            item(ElementId::NamedInteger("bar-pane".into(), id), on)
                                .child(mark)
                                .child(div().max_w(u(180.)).one_line().child(host))
                                .when(b.loading, |d| d.child(dot(theme::accent())))
                                .on_click(cx.listener(move |ws, _, w, cx| ws.focus_pane(id, w, cx)))
                                .into_any_element(),
                        );
                    }
                    _ => {}
                }
            }
        }

        // Dev servers: a filled dot when a pane shows it (a click goes
        // there), a ring when none does (a click opens it beside you).
        let mut ports: Vec<(u16, String)> = self.bar_ports.clone();
        for &port in port_pane.keys() {
            if !ports.iter().any(|(p, _)| *p == port) {
                ports.push((port, String::new()));
            }
        }
        ports.sort_by_key(|(p, _)| *p);
        let ports: Vec<AnyElement> = ports
            .into_iter()
            .take(5)
            .map(|(port, process)| {
                let pane = port_pane.get(&port).copied();
                let on = pane.is_some() && pane == focused;
                item(ElementId::NamedInteger("bar-port".into(), port as u64), on)
                    .child(if pane.is_some() { dot(green).into_any_element() } else { ring(green).into_any_element() })
                    .child(format!("localhost:{port}"))
                    .when(!process.is_empty(), |d| d.child(div().text_color(rgb(theme::dim())).child(process)))
                    .on_click(cx.listener(move |ws, _, w, cx| match pane {
                        Some(id) => ws.focus_pane(id, w, cx),
                        None => ws.open_url_beside(&format!("http://localhost:{port}"), w, cx),
                    }))
                    .into_any_element()
            })
            .collect();

        // Two targets: the branch switches branches, the project opens
        // its changes in Git.
        let chip = |id: &'static str| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .gap(u(5.))
                .h(u(20.))
                .px(u(6.))
                .rounded(u(6.))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
        };
        let place = self.bar_git.clone().map(|(project, branch)| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(u(2.))
                .children(branch.map(|b| {
                    chip("bar-branch")
                        .when(self.branch_picker.is_some(), |d| d.bg(rgb(theme::surface())))
                        .child(icon(Icon::Git, 12., theme::text_2()))
                        .child(div().max_w(u(160.)).one_line().text_color(rgb(theme::text())).child(b))
                        .child(icon(Icon::ChevronUp, 9., theme::dim()))
                        .on_click(cx.listener(|ws, _, w, cx| {
                            if let Some(root) = ws.focused_cwd(cx).map(|c| crate::inspect::project_root(&c)) {
                                let at = gpui::point(px(8.), w.viewport_size().height - px(30.));
                                ws.open_branch_picker(root, at, w, cx);
                            }
                        }))
                }))
                .child(
                    chip("bar-project")
                        .child(icon(Icon::Folder, 12., theme::dim()))
                        .child(div().max_w(u(160.)).one_line().child(project))
                        .on_click(cx.listener(|ws, _, w, cx| {
                            if let Err(e) = ws.open_editor("changes", None, None, w, cx) {
                                tracing::warn!("could not open the editor: {e}");
                            }
                        })),
                )
        });
        // The chats in this window as one item: their marks, how many, what
        // they spent. Hovering opens the list, busiest first, a click from
        // each.
        let chats: Option<AnyElement> = self.render_bar_chats(cx);
        // Background jobs and pages: a tab each, opening the drawer above the
        // bar. A few show inline, in their order (the open one always among
        // them); the rest fold into "+N", whose list holds every one.
        let rows: Vec<BgRow> = self.jobs.iter().filter_map(|&id| self.bg_row(id, &names, cx)).collect();
        let shown: Vec<usize> = if rows.len() <= BG_INLINE + 1 {
            (0..rows.len()).collect()
        } else {
            let mut v: Vec<usize> = (0..BG_INLINE).collect();
            if let Some(i) = rows.iter().position(|r| r.open).filter(|&i| i >= BG_INLINE) {
                v[BG_INLINE - 1] = i;
            }
            v
        };
        let hidden: Vec<usize> = (0..rows.len()).filter(|i| !shown.contains(i)).collect();
        let mut bg: Vec<AnyElement> = shown
            .iter()
            .map(|&i| {
                let r = &rows[i];
                let id = r.id;
                item(ElementId::NamedInteger("bar-job".into(), id), r.open)
                    .child(self.bg_mark(id, 12., cx))
                    .child(div().max_w(u(120.)).one_line().child(r.label.clone()))
                    .child(dot(r.state))
                    .on_click(cx.listener(move |ws, _, w, cx| {
                        if ws.job_open == Some(id) {
                            ws.job_open = None;
                            cx.notify();
                        } else {
                            ws.show_job(id, w, cx);
                        }
                    }))
                    .into_any_element()
            })
            .collect();
        if !hidden.is_empty() {
            bg.push(self.render_bar_more(&rows, &hidden, cx));
        }
        let meter = self.render_usage_meter(cx);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(u(2.))
            .h(u(26.))
            .px(u(6.))
            .border_t_1()
            .border_color(rgba(theme::hairline()))
            .bg(theme::ground())
            .text_size(u(11.5))
            .text_color(rgb(theme::text_2()))
            .children(place)
            .when(chats.is_some(), |d| d.child(sep()).children(chats))
            .when(!bg.is_empty(), |d| d.child(sep()).children(bg))
            .child(div().flex_1().min_w(u(12.)))
            .child(
                div()
                    .id("bar-jobs")
                    .flex()
                    .flex_shrink()
                    .min_w_0()
                    .items_center()
                    .gap(u(2.))
                    .overflow_x_scroll()
                    .children(jobs),
            )
            .when(!ports.is_empty(), |d| d.child(sep()).children(ports))
            .children(meter.map(|m| div().flex().flex_none().items_center().child(sep()).child(m)))
            .into_any_element()
    }

    fn render_bar_chats(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let spend = self.chat_spend(cx);
        if spend.is_empty() {
            return None;
        }
        let focused = self.focused();
        let working = spend.iter().filter(|c| self.status(c.pane, cx) == Some(Status::Working)).count();
        let cost: f64 = spend.iter().filter_map(|c| c.usage.cost_usd).sum();
        let tokens_all: u64 = spend.iter().map(|c| c.usage.tokens()).sum();
        let figure = if cost >= 0.01 { format!("${cost:.2}") } else { tokens(tokens_all) };
        // Up to three marks, overlapping a little, like a stack.
        let mut marks = div().flex().items_center();
        let mut seen: Vec<Icon> = Vec::new();
        for c in &spend {
            if !seen.contains(&c.mark) && seen.len() < 3 {
                seen.push(c.mark);
                let tone = c.mark.brand().unwrap_or(theme::text_2());
                marks = marks.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(u(16.))
                        .when(seen.len() > 1, |d| d.ml(u(-4.)))
                        .rounded_full()
                        .bg(rgb(theme::sidebar()))
                        .child(icon(c.mark, 11., tone)),
                );
            }
        }
        let label = match (spend.len(), working) {
            (1, 0) => "1 chat".to_string(),
            (n, 0) => format!("{n} chats"),
            (n, w) => format!("{n} chats · {w} working"),
        };
        let open = self.bar_chats_open;
        let mut list = div()
            .id("bar-chats-list")
            .absolute()
            .bottom(u(32.))
            .left_0()
            .w(u(340.))
            .p(u(4.))
            .flex()
            .flex_col()
            .gap(u(2.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .occlude()
            .on_hover(cx.listener(|ws, on: &bool, _, cx| {
                ws.bar_chats_over_list = *on;
                ws.bar_chats_close_soon(cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(u(26.))
                    .px(u(8.))
                    .text_size(u(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::dim()))
                    .child(div().flex_1().child("Chats in this window"))
                    .child(div().font_weight(FontWeight::NORMAL).child("by spend")),
            );
        // The conversation you are in comes first, marked: a chat here, or
        // the agent in the focused terminal (read from its own transcript).
        let mut spend = spend;
        if let Some(i) = spend.iter().position(|c| Some(c.pane) == focused) {
            let c = spend.remove(i);
            spend.insert(0, c);
        }
        let here_terminal = focused
            .filter(|id| !spend.iter().any(|c| c.pane == *id))
            .and(self.usage.session.as_ref());
        if let Some((title, su)) = here_terminal {
            let figure = su.cost_usd.map_or_else(|| tokens(su.input + su.output + su.cache_read), |c| format!("${c:.2}"));
            let pct = su.context_used as f32 / su.context_window.max(1) as f32 * 100.;
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .h(u(40.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .bg(rgb(theme::surface()))
                    .text_size(u(12.5))
                    .child(icon(Icon::Claude, 13., Icon::Claude.brand().unwrap_or(theme::text_2())))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(u(1.))
                            .child(div().w_full().one_line().line_height(u(16.)).text_color(rgb(theme::text())).child(title.clone()))
                            .child(
                                div()
                                    .w_full()
                                    .one_line()
                                    .line_height(u(14.))
                                    .text_size(u(11.))
                                    .text_color(rgb(theme::dim()))
                                    .child(format!("This terminal · context {}", pct_text(pct))),
                            ),
                    )
                    .child(current_tag())
                    .child(div().flex_none().min_w(u(48.)).flex().justify_end().text_size(u(12.)).text_color(rgb(theme::text_2())).child(figure)),
            );
        }
        for c in spend.iter().take(10) {
            let pane = c.pane;
            let busy = self.status(pane, cx) == Some(Status::Working);
            let figure = match c.usage.cost_usd.filter(|v| *v >= 0.01) {
                Some(v) => format!("${v:.2}"),
                None => tokens(c.usage.tokens()),
            };
            list = list.child(
                div()
                    .id(ElementId::NamedInteger("bar-chat".into(), pane))
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .h(u(40.))
                    .px(u(8.))
                    // Concentric: the list's 10 radius less its 4 padding.
                    .rounded(u(6.))
                    .cursor_pointer()
                    .text_size(u(12.5))
                    .when(focused == Some(pane), |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::row_hover())))
                    .child(if busy {
                        thinking(ElementId::NamedInteger("bar-chat-think".into(), pane), c.mark, 13., live(c.mark))
                    } else {
                        icon(c.mark, 13., theme::text_2())
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(u(1.))
                            .child(div().w_full().one_line().line_height(u(16.)).text_color(rgb(theme::text())).child(c.title.clone()))
                            .child(
                                div()
                                    .w_full()
                                    .one_line()
                                    .line_height(u(14.))
                                    .text_size(u(11.))
                                    .text_color(rgb(theme::dim()))
                                    .child(match &c.model {
                                        Some(m) => format!("{} · {m}", c.agent),
                                        None => c.agent.to_string(),
                                    }),
                            ),
                    )
                    .when(focused == Some(pane), |d| d.child(current_tag()))
                    .child(
                        div()
                            .flex_none()
                            .min_w(u(48.))
                            .flex()
                            .justify_end()
                            .text_size(u(12.))
                            .text_color(rgb(theme::text_2()))
                            .child(figure),
                    )
                    .on_click(cx.listener(move |ws, _, w, cx| {
                        ws.bar_chats_open = false;
                        ws.focus_pane(pane, w, cx);
                    })),
            );
        }
        Some(
            div()
                .relative()
                .flex()
                .flex_none()
                .child(
                    div()
                        .id("bar-chats")
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .h(u(20.))
                        .px(u(6.))
                        .rounded(u(5.))
                        .cursor_pointer()
                        .text_color(rgb(if open { theme::text() } else { theme::text_2() }))
                        .when(open, |d| d.bg(rgb(theme::surface())))
                        .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                        .child(marks)
                        .child(label)
                        .child(div().text_color(rgb(theme::dim())).child(figure))
                        .on_hover(cx.listener(|ws, on: &bool, _, cx| {
                            ws.bar_chats_over_item = *on;
                            if *on {
                                ws.bar_chats_open = true;
                                cx.notify();
                            } else {
                                ws.bar_chats_close_soon(cx);
                            }
                        }))
                        .on_click(cx.listener(|ws, _, _, cx| {
                            ws.bar_chats_open = !ws.bar_chats_open;
                            cx.notify();
                        })),
                )
                .when(open, |d| {
                    d.child(gpui::deferred(list.with_animation(
                        "bar-chats-in",
                        Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                        |d, t| d.opacity(t).bottom(u(28. + 4. * t)),
                    )))
                })
                .into_any_element(),
        )
    }

    /// A background job or page as the status bar shows it.
    fn bg_row(&self, id: PaneId, names: &HashMap<PaneId, &str>, cx: &App) -> Option<BgRow> {
        let label = names.get(&id).map_or_else(|| format!("job {id}"), |n| n.to_string());
        let open = self.job_open == Some(id);
        let (detail, state, failed) = match &self.panes.get(&id)?.kind {
            PaneKind::Terminal(_) => {
                let j = self.daemon_job(id);
                let running = j.is_some_and(|j| j.running);
                let failed = j.is_some_and(|j| !j.running && j.exit.is_some_and(|c| c != 0));
                let state = if running { theme::palette().ansi[2] } else if failed { theme::DANGER } else { theme::dim() };
                (j.map(|j| j.cmd.lines().next().unwrap_or("").to_string()).unwrap_or_default(), state, failed)
            }
            PaneKind::Browser(b) => {
                let b = b.read(cx);
                let state = if b.loading { theme::accent() } else { theme::dim() };
                (crate::history::host(&b.url).to_string(), state, false)
            }
            _ => return None,
        };
        Some(BgRow { id, label, detail, state, failed, open })
    }

    /// A background job's mark (its agent's, or a terminal) or a page's
    /// favicon.
    fn bg_mark(&self, id: PaneId, size: f32, cx: &App) -> AnyElement {
        match self.panes.get(&id).map(|p| &p.kind) {
            Some(PaneKind::Browser(b)) => match b.read(cx).favicon() {
                Some(img) => crate::browser::site_image(img, size),
                None => icon(Icon::Globe, size, theme::dim()),
            },
            _ => match self.daemon_job(id).and_then(|j| j.agent.as_deref()) {
                Some(a) => icon(Icon::agent(a), size, theme::text_2()),
                None => icon(Icon::Terminal, size, theme::dim()),
            },
        }
    }

    /// "+N": the background items past the inline ones, as stacked marks
    /// and a count (red when one of them failed). Hovering opens the list
    /// of every job and page, a click on a row opens it in the drawer.
    fn render_bar_more(&self, rows: &[BgRow], hidden: &[usize], cx: &mut Context<Self>) -> AnyElement {
        let open = self.bar_more_open;
        let failed = hidden.iter().any(|&i| rows[i].failed);
        let mut marks = div().flex().items_center();
        for (n, &i) in hidden.iter().take(3).enumerate() {
            marks = marks.child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(u(16.))
                    .when(n > 0, |d| d.ml(u(-5.)))
                    .rounded_full()
                    .bg(theme::ground())
                    .child(self.bg_mark(rows[i].id, 11., cx)),
            );
        }
        let mut list = div()
            .id("bar-more-scroll")
            .flex()
            .flex_col()
            .gap(u(1.))
            .max_h(u(360.))
            .overflow_y_scroll();
        for r in rows {
            let id = r.id;
            list = list.child(
                div()
                    .id(ElementId::NamedInteger("bar-more-row".into(), id))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(u(10.))
                    .h(u(34.))
                    .px(u(8.))
                    .rounded(u(6.))
                    .cursor_pointer()
                    .text_size(u(12.5))
                    .when(r.open, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::row_hover())))
                    .child(div().flex_none().flex().justify_center().w(u(14.)).child(self.bg_mark(id, 13., cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().w_full().one_line().line_height(u(15.)).text_color(rgb(theme::text())).child(r.label.clone()))
                            .when(!r.detail.is_empty() && r.detail != r.label, |d| {
                                d.child(
                                    div()
                                        .w_full()
                                        .one_line()
                                        .line_height(u(13.))
                                        .text_size(u(11.))
                                        .text_color(rgb(theme::dim()))
                                        .child(r.detail.clone()),
                                )
                            }),
                    )
                    .child(div().flex_none().size(u(6.)).rounded_full().bg(rgb(r.state)))
                    .on_click(cx.listener(move |ws, _, w, cx| {
                        ws.bar_more_open = false;
                        ws.show_job(id, w, cx);
                    })),
            );
        }
        let panel = div()
            .id("bar-more-list")
            .absolute()
            .bottom(u(32.))
            .left_0()
            .w(u(300.))
            .p(u(4.))
            .flex()
            .flex_col()
            .gap(u(2.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .occlude()
            .on_hover(cx.listener(|ws, on: &bool, _, cx| {
                ws.bar_more_over_list = *on;
                ws.bar_more_close_soon(cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(u(26.))
                    .px(u(8.))
                    .text_size(u(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::dim()))
                    .child(div().flex_1().child("In the background"))
                    .child(div().font_weight(FontWeight::NORMAL).child(rows.len().to_string())),
            )
            .child(list);
        div()
            .relative()
            .flex()
            .flex_none()
            .child(
                div()
                    .id("bar-more")
                    .flex()
                    .items_center()
                    .gap(u(5.))
                    .h(u(20.))
                    .px(u(6.))
                    .rounded(u(5.))
                    .cursor_pointer()
                    .text_color(rgb(if open { theme::text() } else { theme::text_2() }))
                    .when(open, |d| d.bg(rgb(theme::surface())))
                    .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                    .child(marks)
                    .child(format!("+{}", hidden.len()))
                    .when(failed, |d| d.child(div().flex_none().size(u(6.)).rounded_full().bg(rgb(theme::DANGER))))
                    .on_hover(cx.listener(|ws, on: &bool, _, cx| {
                        ws.bar_more_over_item = *on;
                        if *on {
                            ws.bar_more_open = true;
                            cx.notify();
                        } else {
                            ws.bar_more_close_soon(cx);
                        }
                    }))
                    .on_click(cx.listener(|ws, _, _, cx| {
                        ws.bar_more_open = !ws.bar_more_open;
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                d.child(gpui::deferred(panel.with_animation(
                    "bar-more-in",
                    Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint()),
                    |d, t| d.opacity(t).bottom(u(28. + 4. * t)),
                )))
            })
            .into_any_element()
    }

    fn bar_more_close_soon(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(220)).await;
            let _ = this.update(cx, |ws, cx| {
                if !ws.bar_more_over_item && !ws.bar_more_over_list && ws.bar_more_open {
                    ws.bar_more_open = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The chats list shuts a moment after the pointer leaves both the
    /// item and the list, so moving from one to the other keeps it open.
    fn bar_chats_close_soon(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(220)).await;
            let _ = this.update(cx, |ws, cx| {
                if !ws.bar_chats_over_item && !ws.bar_chats_over_list && ws.bar_chats_open {
                    ws.bar_chats_open = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The open background job's terminal, docked above the status bar:
    /// its name and command, open as a tab, restart, stop, hide.
    fn render_job_drawer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.job_open?;
        if let PaneKind::Browser(b) = &self.panes.get(&id)?.kind {
            return Some(self.render_page_drawer(id, b.clone(), cx));
        }
        let PaneKind::Terminal(t) = &self.panes.get(&id)?.kind else { return None };
        let name = self.named.iter().find(|(_, v)| **v == id).map(|(n, _)| n.clone());
        let dj = self.daemon_job(id).cloned();
        let cmd = dj.as_ref().map(|j| j.cmd.clone()).unwrap_or_default();
        let status = match dj.as_ref() {
            Some(j) if j.running => "running · keeps going when Deck quits".to_string(),
            Some(crate::daemon::JobInfo { exit: Some(c), .. }) => format!("exited {c}"),
            _ => "starting".to_string(),
        };
        let restart_name = dj.as_ref().map(|j| j.name.clone());
        let button = |id: &'static str, glyph: Icon| icon_button(id, glyph).size(u(22.));
        Some(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .h(u(300.))
                .mx(u(theme::GUTTER))
                .mb(u(theme::GUTTER))
                .rounded(u(theme::CARD_RADIUS))
                .overflow_hidden()
                .border_1()
                .border_color(rgba(theme::hairline()))
                .bg(rgb(theme::pane()))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(u(8.))
                        .h(u(32.))
                        .pl(u(12.))
                        .pr(u(4.))
                        .border_b_1()
                        .border_color(rgba(theme::hairline()))
                        .text_size(u(12.))
                        .child(icon(Icon::Terminal, 13., theme::text_2()))
                        .child(div().font_weight(FontWeight::MEDIUM).child(name.unwrap_or_else(|| format!("job {id}"))))
                        .child(div().flex_1().min_w_0().one_line().text_color(rgb(theme::dim())).child(cmd))
                        .child(div().text_size(u(11.)).text_color(rgb(theme::dim())).child(status))
                        .child(
                            button("job-restart", Icon::Refresh)
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    if let Some(n) = restart_name.clone() {
                                        cx.background_executor().spawn(async move { crate::daemon::restart(&n) }).detach();
                                    }
                                })),
                        )
                        .child(
                            button("job-tab", Icon::Maximize)
                                .on_click(cx.listener(move |ws, _, w, cx| ws.job_to_tab(id, w, cx))),
                        )
                        .child(
                            button("job-stop", Icon::Stop)
                                .on_click(cx.listener(move |ws, _, w, cx| ws.stop_job(id, w, cx))),
                        )
                        .child(button("job-hide", Icon::ChevronDown).on_click(cx.listener(|ws, _, _, cx| {
                            ws.job_open = None;
                            cx.notify();
                        }))),
                )
                .child(div().flex_1().min_h_0().child(t.clone()))
                .into_any_element(),
        )
    }

    /// A background page in the drawer: the page itself, taller than a
    /// job's terminal, to see what the agent is reading.
    fn render_page_drawer(&self, id: PaneId, b: Entity<BrowserPane>, cx: &mut Context<Self>) -> AnyElement {
        let name = self.named.iter().find(|(_, v)| **v == id).map(|(n, _)| n.clone()).unwrap_or_else(|| format!("page {id}"));
        let button = |eid: &'static str, glyph: Icon| icon_button(eid, glyph).size(u(22.));
        div()
            .flex()
            .flex_col()
            .flex_none()
            .h(u(440.))
            .mx(u(theme::GUTTER))
            .mb(u(theme::GUTTER))
            .rounded(u(theme::CARD_RADIUS))
            .overflow_hidden()
            .border_1()
            .border_color(rgba(theme::hairline()))
            .bg(rgb(theme::pane()))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(u(8.))
                    .h(u(32.))
                    .pl(u(12.))
                    .pr(u(4.))
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(12.))
                    .child(icon(Icon::Globe, 13., theme::text_2()))
                    .child(div().font_weight(FontWeight::MEDIUM).child(name))
                    .child(div().flex_1().min_w_0().one_line().text_color(rgb(theme::dim())).child("Background page, read by your agents"))
                    .child(button("page-tab", Icon::Maximize).on_click(cx.listener(move |ws, _, w, cx| ws.job_to_tab(id, w, cx))))
                    .child(button("page-close", Icon::Close).on_click(cx.listener(move |ws, _, w, cx| ws.stop_job(id, w, cx))))
                    .child(button("page-hide", Icon::ChevronDown).on_click(cx.listener(|ws, _, _, cx| {
                        ws.job_open = None;
                        cx.notify();
                    }))),
            )
            .child(div().flex_1().min_h_0().child(b))
            .into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.probed {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(u(13.))
                .text_color(rgb(theme::dim()))
                .child("Starting agents…")
                .into_any_element();
        }
        let mut col = div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(u(8.))
            .size_full()
            .text_color(rgb(theme::text_2()))
            .child(
                div()
                    .pb(u(8.))
                    .text_size(u(15.))
                    .text_color(rgb(theme::text()))
                    .child("Nothing open"),
            );
        let actions: [(&'static str, &'static str, Icon, u8); 2] = [
            ("New Terminal", "empty-term", Icon::Terminal, 0),
            ("New Browser", "empty-web", Icon::Globe, 1),
        ];
        for (label, id, glyph, kind) in actions {
            col = col.child(
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .w(u(220.))
                    .h(u(34.))
                    .px(u(12.))
                    .rounded(u(8.))
                    .bg(rgb(theme::surface()))
                    .text_size(u(13.))
                    .text_color(rgb(theme::text()))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::surface_hover())))
                    .child(icon(glyph, 15., theme::text_2()))
                    .child(label)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(u(12.))
                            .text_color(rgb(theme::dim()))
                            .child(if kind == 0 { "⌘T" } else { "⌘⇧T" }),
                    )
                    .on_click(cx.listener(move |ws, _, window, cx| {
                        let what = if kind == 0 {
                            ws.shell_here(cx)
                        } else {
                            Open::Browser {
                                url: "about:blank".into(),
                            }
                        };
                        if ws.new_tab(what, None, window, cx).is_ok() && kind == 1 {
                            ws.focus_address(window, cx);
                        }
                    })),
            );
        }
        col.into_any_element()
    }
}

pub fn icon_button(id: impl Into<ElementId>, glyph: Icon) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(u(26.))
        .rounded(u(6.))
        .text_color(rgb(theme::text_2()))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
        .child(icon(glyph, 15., theme::text_2()))
}

/// The content area never gets narrower than this to make room for chrome:
/// past it the inspector folds away (and comes back when there is room).
const MIN_CONTENT_PX: f32 = 520.;
/// How far the side panels can be dragged.
const SIDEBAR_MIN: f32 = 200.;
const SIDEBAR_MAX: f32 = 440.;
const INSPECTOR_MIN: f32 = 260.;
const INSPECTOR_MAX: f32 = 560.;
/// Narrower than this, even the inspector alone would crowd the panes out.
const MIN_NARROW_PX: f32 = 360.;

#[derive(Clone, Copy, PartialEq)]
enum Finish {
    Merge,
    Discard,
    Pr,
}

/// A word for the shell, single-quoted.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A divider held down: the two panes either side (by the pane each
/// subtree starts with), where the drag began, their weights then, and the
/// pixels they share.
struct Resize {
    row: bool,
    a: PaneId,
    b: PaneId,
    start: f32,
    wa: f32,
    wb: f32,
    span: f32,
}

/// Sidebar rows share their edges: the leading mark sits in one slot at the
/// row inset, every title starts on the same column after it, and trailing
/// buttons end on one edge, headers' included.
/// Presses of ⌃Tab this close together walk further back.
const MRU_WALK: Duration = Duration::from_millis(1200);

/// How far back a relaunch brings tabs and threads: three days.
const RESTORE_SECS: i64 = 3 * 86_400;
const ROW_SLOT: f32 = 16.;
/// The trailing column of every row (time, state, close), right-aligned.
const TRAIL_W: f32 = 30.;
/// Every list row: one line, this tall, titles this size.
const ROW_H: f32 = 34.;
const ROW_TEXT: f32 = 14.;
const HEAD_TEXT: f32 = 12.5;
const ROW_GAP: f32 = 10.;
const ROW_END: f32 = 4.;
/// A project's rows sit one step in from its header: their icons on the
/// header's name, so the list reads as a tree.
const ROW_INDENT: f32 = ROW_SLOT + ROW_GAP;
/// Every sidebar row is this tall, a title and a line under it, so the list
/// keeps one rhythm whatever is in it.
const ROW_TWO_LINE: f32 = 44.;
/// The sidebar footer's rows, Usage and Settings.
const ROW_ONE_LINE: f32 = 32.;
/// Rows with nothing going on step back; working ones keep full strength
/// and their agent's colour.
const IDLE_FADE: f32 = 0.9;

/// The colour of a mark that is working: its agent's, else the accent.
fn live(glyph: Icon) -> u32 {
    glyph.brand().unwrap_or_else(theme::accent)
}

/// A usage bar: the share used, coloured as it nears the cap. `width` 0
/// fills the row.
fn meter_bar(pct: f32, width: f32) -> gpui::Div {
    let track = div()
        .h(u(if width > 0. { 4. } else { 5. }))
        .rounded_full()
        .overflow_hidden()
        .bg(rgba(theme::alpha(theme::text(), 0x1c)));
    let track = if width > 0. { track.w(u(width)).flex_none() } else { track.w_full() };
    track.child(
        div()
            .h_full()
            .w(gpui::relative((pct / 100.).clamp(0.02, 1.)))
            .rounded_full()
            .bg(rgb(level_color(pct))),
    )
}

/// A share as Claude Code prints it: whole percent, rounded down, so the
/// meter and its own status line agree.
fn pct_text(pct: f32) -> String {
    format!("{}%", pct.max(0.).floor() as u32)
}

/// Calm until a limit gets close: amber from 75%, red from 90%.
fn level_color(pct: f32) -> u32 {
    let p = theme::palette();
    if pct >= 90. {
        theme::DANGER
    } else if pct >= 75. {
        p.ansi[3]
    } else {
        theme::text_2()
    }
}

/// "5-hour" → "5h", "Weekly" → "wk", for the meter.
fn short_limit(label: &str) -> SharedString {
    let l = label.to_lowercase();
    if l.starts_with("5") {
        "5h".into()
    } else if l.contains("week") {
        "wk".into()
    } else {
        label.to_string().into()
    }
}

/// How long until `at`, compactly: "42m", "2h 14m", "3d 4h"; `None` once
/// it has passed.
fn until(at: std::time::SystemTime) -> Option<String> {
    let s = at.duration_since(std::time::SystemTime::now()).ok()?.as_secs();
    Some(match s {
        0..=3599 => format!("{}m", s.div_ceil(60).max(1)),
        3600..=86_399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    })
}

/// "resets in 2h 14m", or in days past a day.
fn resets_in(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(std::time::SystemTime::now()).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0 => "resets now".into(),
        s if s < 3600 => format!("resets in {}m", s.div_ceil(60)),
        s if s < 86_400 => format!("resets in {}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("resets in {}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// 1234 → "1.2k", 3_400_000 → "3.4M".
fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Programs you work inside rather than wait on.
fn interactive(program: &str) -> bool {
    matches!(
        program,
        "vim" | "nvim" | "vi" | "nano" | "emacs" | "hx" | "helix" | "micro" | "less" | "more" | "man" | "top" | "htop"
            | "btop" | "ssh" | "mosh" | "tmux" | "zellij" | "screen" | "lazygit" | "tig" | "fzf" | "watch" | "zsh"
            | "bash" | "fish" | "python" | "python3" | "node" | "irb" | "psql" | "mysql" | "sqlite3" | "redis-cli"
    )
}

/// Whether something on localhost:`port` answers like a web server, asked
/// with a plain request and a short wait.
fn speaks_http(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)) else {
        // Some servers listen on ::1 only.
        let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
        let Ok(mut s) = std::net::TcpStream::connect_timeout(&v6, Duration::from_millis(250)) else {
            return false;
        };
        return ask_http(&mut s);
    };
    ask_http(&mut s)
}

/// A plain HEAD request; the answer starts "HTTP/" from a web server.
fn ask_http(s: &mut std::net::TcpStream) -> bool {
    use std::io::{Read, Write};
    let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(250)));
    if s.write_all(b"HEAD / HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 8];
    matches!(s.read(&mut buf), Ok(n) if n >= 5 && &buf[..5] == b"HTTP/")
}

/// Whether a listening socket under one of our terminals could be a dev
/// server: not an app a terminal happened to start (Chrome's debugging
/// port, an editor's helper), nor a port the system handed out at random.
/// It still has to answer HTTP (`speaks_http`) to be shown.
fn dev_server(port: u16, process: &str) -> bool {
    let p = process.to_lowercase();
    const APPS: &[&str] = &[
        "chrome", "safari", "firefox", "arc", "brave", "edge", "electron", "code helper", "cursor", "slack", "discord",
        "spotify", "figma", "zoom", "controlcenter", "rapportd", "claude", "codex", "ollama",
    ];
    if APPS.iter().any(|a| p.contains(a)) {
        return false;
    }
    // The system hands out 49152 and up at random; a dev server picks a
    // port people type. Those still have to answer HTTP to be shown.
    port < 49152
}

/// The port of a local address (`http://localhost:3000/x` → 3000).
fn url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let local = host.starts_with("localhost") || host.starts_with("127.0.0.1") || host.starts_with("[::1]") || host.starts_with("0.0.0.0");
    local.then(|| host.rsplit(':').next()?.parse().ok()).flatten()
}

/// The branch checked out in `root`, from `.git/HEAD` (a worktree's `.git`
/// file points at its own); a short hash when detached.
fn git_head(root: &std::path::Path) -> Option<String> {
    let dot = root.join(".git");
    let dir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        let p = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    } else {
        dot
    };
    let head = std::fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        None => head.chars().take(7).collect(),
    })
}

/// The leading mark of a sidebar row, centred in the shared slot, so icons,
/// favicons and spinners of different sizes line up.
/// A sidebar row's frame, the same on every row. The open row is lifted
/// onto its own tone with a hairline edge (and a soft shadow on a light
/// sidebar), so it never reads as merely hovered. The edge is there at rest
/// too, only transparent, so picking a row moves nothing by a pixel. An
/// idle row (`faded`) steps back until the pointer is on it. `indent` moves
/// a project's rows in under its header.
/// A small heading in the sidebar's list, with room after the label for
/// its own controls.
/// A group's header: its name, a step down from the rows, in semibold,
/// the same for days, projects and the view switch.
fn sidebar_header(label: &'static str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(u(6.))
        .h(u(30.))
        .mt(u(16.))
        .pl(u(theme::ROW_PAD))
        .pr(u(ROW_END))
        .text_size(u(HEAD_TEXT))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::dim()))
        .child(div().flex_none().child(label))
}

/// How many rows a header stands over, beside its name.
fn header_count(n: usize) -> gpui::Div {
    div().flex_1().font_weight(FontWeight::NORMAL).text_color(rgb(theme::dim())).opacity(0.8).child(n.to_string())
}

/// A row's second line. In the Recent view no header says the project, so
/// the line leads with it.
#[allow(dead_code)]
fn second_line(place: Option<&str>, text: SharedString) -> gpui::Div {
    let line = div().w_full().text_size(u(11.)).text_color(rgb(theme::dim()));
    let Some(project) = place.filter(|p| !p.is_empty()) else {
        return line.one_line().child(text);
    };
    let text: SharedString = if text.is_empty() {
        project.to_string().into()
    } else if text.starts_with(project) {
        text
    } else {
        format!("{project} · {text}").into()
    };
    line.flex()
        .items_center()
        .gap(u(4.))
        .child(div().flex_none().child(icon(Icon::Folder, 10., theme::dim())))
        .child(div().flex_1().min_w_0().one_line().child(text))
}

/// Where a row sits in the sidebar's Recent view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Bucket {
    Today,
    Yesterday,
    Week,
    Earlier,
}

impl Bucket {
    /// By how long ago it was against how far into today it is, both in
    /// seconds: Yesterday is the calendar day, not the last 24 hours.
    fn of(age: u64, since_midnight: u64) -> Bucket {
        match age.saturating_sub(since_midnight) {
            0 => Bucket::Today,
            1..=86_400 => Bucket::Yesterday,
            86_401..=518_400 => Bucket::Week,
            _ => Bucket::Earlier,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Bucket::Today => "Today",
            Bucket::Yesterday => "Yesterday",
            Bucket::Week => "This week",
            Bucket::Earlier => "Earlier",
        }
    }
}

fn sidebar_row(
    row: gpui::Stateful<gpui::Div>,
    indent: f32,
    active: bool,
    faded: bool,
) -> gpui::Stateful<gpui::Div> {
    row.flex()
        .items_center()
        .gap(u(ROW_GAP))
        // The edge takes a pixel; text stays on the headers' line.
        .pl(u(theme::ROW_PAD + indent - 1.))
        .pr(u(ROW_END - 1.))
        .rounded(u(theme::ROW_RADIUS))
        .border_1()
        .cursor_pointer()
        // The open row: a faint fill inside a hairline, flat (no shadow).
        .when(active, |d| {
            d.bg(rgba(theme::alpha(theme::text(), 0x0a)))
                .border_color(rgba(theme::hairline()))
        })
        .when(!active, |d| {
            d.border_color(gpui::transparent_black())
                .when(faded, |d| d.opacity(IDLE_FADE))
                .hover(move |s| {
                    let s = s.bg(rgb(theme::row_hover()));
                    if faded {
                        s.opacity(1.)
                    } else {
                        s
                    }
                })
        })
}

/// A shortcut from Settings › Shortcuts as a keycap, or nothing when it
/// has none.
fn keycap(keys: SharedString) -> Option<gpui::Div> {
    (!keys.is_empty()).then(|| crate::kbd::key(keys))
}

fn lead(mark: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .justify_center()
        .w(u(ROW_SLOT))
        .child(mark)
        .into_any_element()
}

/// A part of the window chrome drawn as its own cached view.
///
/// gpui redraws every ancestor of a view that changed, and the workspace is
/// the root, so each frame of a streaming agent in a terminal used to rebuild
/// the whole sidebar and inspector too. As islands they draw again only when
/// the workspace itself notifies, not whenever a pane inside it repaints.
pub struct Island {
    ws: WeakEntity<Workspace>,
    draw: fn(&mut Workspace, &mut Window, &mut Context<Workspace>) -> AnyElement,
    _observe: Subscription,
}

impl Island {
    fn new(
        ws: &Entity<Workspace>,
        draw: fn(&mut Workspace, &mut Window, &mut Context<Workspace>) -> AnyElement,
        cx: &mut Context<Workspace>,
    ) -> Entity<Island> {
        let weak = ws.downgrade();
        let ws = ws.clone();
        cx.new(|cx| Island {
            ws: weak,
            draw,
            _observe: cx.observe(&ws, |_, _, cx| cx.notify()),
        })
    }
}

impl Render for Island {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draw = self.draw;
        self.ws
            .update(cx, |ws, cx| draw(ws, window, cx))
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// What `refresh_places` looks up for one pane, on a background thread.
enum PlaceJob {
    /// A terminal, by its shell's pid, and the folder the shell last
    /// reported (OSC 7) for when its processes can't be read (`sudo`).
    Shell(u32, Option<PathBuf>),
    /// A chat, by its folder.
    Dir(PathBuf),
}

impl PlaceJob {
    fn place(self) -> Option<Place> {
        match self {
            PlaceJob::Dir(cwd) => Some(place_of(&cwd)),
            PlaceJob::Shell(shell, said) => {
                // What is in front is the terminal's foreground process
                // group, not the deepest process: an agent's helpers (MCP
                // servers, `caffeinate`) sit below it.
                let fg = inspect::foreground(shell).unwrap_or(shell);
                let mut front = (fg, inspect::process_name(fg));
                let mut agent = agent_of(fg, &front.1);
                // An agent tab runs its CLI under `sh -c`, which has no job
                // control: the CLI shares the shell's group, so the group
                // leader is the shell. Look below it for the agent.
                if agent.is_none() && is_shell(&front.1) {
                    if let Some((p, a)) = inspect::descendants(fg)
                        .into_iter()
                        .skip(1)
                        .find_map(|p| agent_of(p.pid, &p.name).map(|a| (p, a)))
                    {
                        front = (p.pid, p.name);
                        agent = Some(a);
                    }
                }
                let cwd = inspect::cwd(front.0).or_else(|| inspect::cwd(shell)).or(said)?;
                let session = agent
                    .as_ref()
                    .filter(|(a, _)| a.cmd == "claude")
                    .and_then(|(_, argv)| crate::session::claude_live_session(front.0, argv));
                Some(Place {
                    session,
                    agent: agent.map(|(a, argv)| (a.cmd, argv)),
                    front: Some(front.1),
                    ..place_of(&cwd)
                })
            }
        }
    }
}

/// Where a pane is working: its project (the git root's name, else the
/// folder's) and the folder below that root.
#[derive(Debug, Clone, Default, PartialEq)]
struct Place {
    cwd: PathBuf,
    project: String,
    /// The path inside the project, `""` at its root.
    sub: String,
    /// What a terminal is running in front ("zsh", "nvim", "cargo").
    front: Option<String>,
    /// An agent CLI in front, and the arguments it was started with.
    agent: Option<(&'static str, Vec<String>)>,
    /// The Claude Code conversation that process holds, as Claude Code
    /// itself records it: exact, where guessing from transcripts is not.
    session: Option<String>,
}

impl Place {
    fn label(&self) -> String {
        if self.sub.is_empty() {
            self.project.clone()
        } else {
            format!("{} / {}", self.project, self.sub)
        }
    }
}

/// The agent CLI process `pid` (named `name`) is, with its arguments: by
/// its name, or for one run by a script interpreter (Gemini CLI under node)
/// by the script it runs. Arguments are read only when it could be one.
fn agent_of(pid: u32, name: &str) -> Option<(&'static Agent, Vec<String>)> {
    if let Some(a) = AGENTS.iter().find(|a| a.cmd == name) {
        return Some((a, inspect::argv(pid)));
    }
    if !matches!(name, "node" | "bun" | "deno" | "python3" | "python") {
        return None;
    }
    let argv = inspect::argv(pid);
    let script = argv.get(1)?.rsplit('/').next()?.to_string();
    AGENTS.iter().find(|a| a.cmd == script).map(|a| (a, argv))
}

fn is_shell(name: &str) -> bool {
    let name = name.trim_start_matches('-');
    matches!(name, "zsh" | "bash" | "sh" | "fish" | "dash" | "login")
}

fn place_of(cwd: &std::path::Path) -> Place {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.as_deref() == Some(cwd) {
        return Place {
            cwd: cwd.to_path_buf(),
            project: "~".into(),
            ..Default::default()
        };
    }
    let name = |p: &std::path::Path| {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string())
    };
    // Stop at home: a dotfiles repo there would swallow every project.
    let root = cwd
        .ancestors()
        .take_while(|d| Some(*d) != home.as_deref())
        .find(|d| d.join(".git").exists());
    match root {
        Some(root) => Place {
            cwd: cwd.to_path_buf(),
            project: name(root),
            sub: cwd
                .strip_prefix(root)
                .map(|r| r.display().to_string())
                .unwrap_or_default(),
            ..Default::default()
        },
        None => Place {
            cwd: cwd.to_path_buf(),
            project: name(cwd),
            ..Default::default()
        },
    }
}

/// A terminal title worth showing: a program's own ("✳ Fix tab titles" from
/// Claude Code, "vim main.rs"), without the spinner glyph agents put in
/// front. The shell's default `user@host:dir` says less than the tab already
/// does, so it is dropped.
fn clean_title(t: &str) -> Option<String> {
    let t = t.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '~' && c != '/');
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    if let Some((user, rest)) = t.split_once('@') {
        if !user.contains(' ') && rest.contains(':') && !rest.contains(' ') {
            return None;
        }
    }
    Some(t.to_string())
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let screen = self.locked || !crate::account::allowed();
        // Pages are native views drawn above everything gpui paints: while a
        // menu, the palette or a whole-window screen is up, they step aside.
        let cover = screen || self.menu.is_some() || self.palette.is_some() || self.usage_open;
        if cover != self.screen_cover {
            self.screen_cover = cover;
            self.cover_pages(cover, cx);
        }
        if self.locked {
            window.set_rem_size(px(theme::REM_PX * self.settings.zoom));
            return self.lock_screen(cx);
        }
        if screen {
            window.set_rem_size(px(theme::REM_PX * self.settings.zoom));
            return self.sign_in_screen(cx);
        }
        let (sidebar, inspector) = self.islands(cx);
        window.set_rem_size(px(theme::REM_PX * self.settings.zoom));
        let width = f32::from(window.viewport_size().width) / self.settings.zoom;
        // The inspector, once asked for, shows. In a window too narrow for
        // both side panels the left one steps aside while it's open, as an
        // IDE does, rather than the inspector silently not appearing.
        let inspector_shown =
            self.inspector.open && width - self.inspector_px >= MIN_NARROW_PX;
        // A glass theme blurs what is behind the window; the others paint
        // it solid. Told to the window only when it changes.
        let glass = theme::glass();
        if self.glass_on != Some(glass) {
            self.glass_on = Some(glass);
            window.set_background_appearance(if glass {
                gpui::WindowBackgroundAppearance::Blurred
            } else {
                gpui::WindowBackgroundAppearance::Opaque
            });
        }
        let sidebar_shown = self.sidebar
            && (!inspector_shown
                || width - self.sidebar_px - self.inspector_px >= MIN_CONTENT_PX);
        let code_shown = self
            .focused_browser_pane()
            .is_some_and(|b| b.read(cx).is_editor());
        // Background pages show only in the drawer.
        for &id in &self.jobs {
            if let Some(PaneKind::Browser(b)) = self.panes.get(&id).map(|p| &p.kind) {
                let open = self.job_open == Some(id);
                b.update(cx, |b, _| b.set_visible(open));
            }
        }
        // Native webviews in hidden tabs must be hidden explicitly.
        for (ix, tab) in self.tabs.iter().enumerate() {
            for id in tab.leaves() {
                if let Some(Pane {
                    kind: PaneKind::Browser(b),
                    ..
                }) = self.panes.get(&id)
                {
                    b.update(cx, |b, _| b.set_visible(ix == self.active));
                }
            }
        }

        let title = self
            .focused()
            .map(|id| self.pane_title(id, cx))
            .unwrap_or_default();
        let project: Option<String> = self
            .focused()
            .and_then(|id| self.places.get(&id))
            .map(|p| p.project.clone())
            .filter(|p| !p.is_empty() && *p != title.as_ref());
        // One pane of a split zoomed to fill its tab (⇧⌘↩).
        let zoomed = self.zoomed.filter(|z| self.tabs.get(self.active).is_some_and(|t| t.root.contains(*z)));
        let multi = zoomed.is_none()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|t| matches!(t.root, Node::Split(..)));
        let content = match (self.tabs.get(self.active), zoomed) {
            (Some(_), Some(z)) => self.render_node(&Node::Leaf(z), false, cx),
            (Some(tab), None) => {
                let root = tab.root.clone();
                self.render_node(&root, multi, cx)
            }
            (None, _) => self.render_empty(cx),
        };

        let left = if sidebar_shown { 0. } else { theme::GUTTER };
        let topbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(u(2.))
            .h(px(theme::TITLEBAR_PX))
            .pr(u(theme::GUTTER))
            // The title sits on the same edge as the text in the card below.
            .when(sidebar_shown, |d| d.pl(u(theme::PANE_PAD_X + 1.)))
            // Clear of the traffic lights, which do not zoom.
            .when(!sidebar_shown, |d| d.pl(px(80.)))
            .on_mouse_down(MouseButton::Left, |ev, window, _| {
                if ev.click_count == 2 {
                    window.titlebar_double_click();
                }
            })
            .when(!sidebar_shown, |d| {
                d.child(self.new_chat_button("new-chat-bar", cx)).child(
                    icon_button("show-sidebar", Icon::Sidebar)
                        .mr(u(6.))
                        .on_click(cx.listener(|ws, _, w, cx| {
                            ws.sidebar = true;
                            // Too narrow for both: the one asked for wins.
                            let width = f32::from(w.viewport_size().width) / ws.settings.zoom;
                            if width - ws.sidebar_px - ws.inspector_px < MIN_CONTENT_PX {
                                ws.inspector.open = false;
                            }
                            cx.notify();
                        })),
                )
            })
            .child(
                // project / title: where you are, then what.
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(13.))
                    .children(project.map(|p| {
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(u(6.))
                            .text_color(rgb(theme::text_2()))
                            .child(icon(Icon::Folder, 13., theme::dim()))
                            .child(div().max_w(u(180.)).one_line().child(p))
                            .child(div().text_color(rgb(theme::dim())).child("/"))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .one_line()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::text()))
                            .child(title.clone()),
                    ),
            )
            .child(
                icon_button("split-right", Icon::Split).on_click(cx.listener(|ws, _, w, cx| {
                    if let Some(p) = ws.focused() {
                        let what = ws.shell_here(cx);
                        let _ = ws.split(p, what, Axis::Row, w, cx);
                    }
                })),
            )
            .child(
                icon_button("open-code", Icon::Code)
                    .when(code_shown, |d| d.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                    .on_click(cx.listener(|ws, _, w, cx| {
                        if let Err(e) = ws.open_editor("files", None, None, w, cx) {
                            tracing::warn!("could not open the editor: {e}");
                        }
                    })),
            )
            .child(
                icon_button("open-web", Icon::Globe)
                    .on_click(cx.listener(|ws, _, w, cx| ws.focus_address(w, cx))),
            )
            .child(
                icon_button("inspector", Icon::PanelRight)
                    .when(inspector_shown, |d| {
                        d.bg(rgb(theme::surface())).text_color(rgb(theme::text()))
                    })
                    .on_click(cx.listener(|ws, _, w, cx| ws.toggle_inspector(w, cx))),
            )
            .children(self.update_pill(cx))
            .child(self.avatar_button(cx));

        let main = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(topbar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .ml(u(left))
                    .mr(u(if inspector_shown { 0. } else { theme::GUTTER }))
                    .mb(u(theme::GUTTER))
                    // A split draws a card per pane instead.
                    .when(!multi, |d| {
                        d.rounded(u(theme::CARD_RADIUS))
                            .overflow_hidden()
                            .border_1()
                            .border_color(rgba(theme::hairline()))
                            .bg(theme::card())
                    })
                    .child(content),
            );
        // The panels side by side, with the status bar under all of them.
        let body = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .when(sidebar_shown, |d| d.child(self.side_handle(true, cx)))
            .when(inspector_shown, |d| d.child(self.side_handle(false, cx)))
            .when(sidebar_shown, |d| {
                d.child(
                    AnyView::from(sidebar).cached(
                        StyleRefinement::default()
                            .flex_none()
                            .w(u(self.sidebar_px))
                            .h_full(),
                    ),
                )
            })
            .child(main)
            .when(inspector_shown, |d| {
                d.child(
                    AnyView::from(inspector).cached(
                        StyleRefinement::default()
                            .flex_none()
                            .w(u(self.inspector_px))
                            .h_full(),
                    ),
                )
            });
        let bar = self.settings.status_bar.then(|| self.render_status_bar(cx));

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::ground())
            .font_family(theme::UI)
            .on_mouse_move(cx.listener(Self::drag_divider))
            .on_mouse_move(cx.listener(Self::drag_side))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|ws, _, _, cx| {
                    if ws.resizing.take().is_some() | ws.side_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .text_color(rgb(theme::text()))
            .on_action(cx.listener(|ws, _: &CommandPalette, w, cx| ws.toggle_palette(false, w, cx)))
            .on_action(cx.listener(|ws, _: &ActionPalette, w, cx| {
                ws.toggle_scoped_palette(crate::palette::Scope::Actions, w, cx)
            }))
            .on_action(cx.listener(|ws, _: &TabSwitcher, w, cx| {
                ws.toggle_scoped_palette(crate::palette::Scope::Tabs, w, cx)
            }))
            .on_action(cx.listener(|ws, _: &QuickOpen, w, cx| {
                // In the editor ⌘P is its own Quick Open; ⌘K is still Deck's.
                match ws.focused_browser_pane().filter(|b| b.read(cx).is_editor()) {
                    Some(b) => b.read(cx).editor_key("quickopen"),
                    None => ws.toggle_scoped_palette(crate::palette::Scope::Tabs, w, cx),
                }
            }))
            .on_action(cx.listener(|ws, _: &LockDeck, _, cx| ws.lock(cx)))
            .on_action(cx.listener(|ws, _: &InsertSnippet, w, cx| ws.toggle_palette(true, w, cx)))
            .on_action(cx.listener(|ws, _: &SaveSnippet, w, cx| ws.save_snippet(w, cx)))
            .on_action(cx.listener(|ws, _: &EditSnippets, w, cx| ws.edit_snippets(w, cx)))
            .on_action(cx.listener(|ws, _: &NewControl, w, cx| ws.new_control(w, cx)))
            .on_action(cx.listener(|ws, _: &RestartControl, w, cx| ws.restart_control(w, cx)))
            .on_action(cx.listener(|ws, _: &NewTab, w, cx| {
                let _ = ws.new_tab(Open::Launcher, None, w, cx);
            }))
            .on_action(cx.listener(|ws, _: &NewTerminal, w, cx| {
                let what = ws.shell_here(cx);
                let _ = ws.new_tab(what, None, w, cx);
            }))
            .on_action(cx.listener(|ws, _: &NewChat, w, cx| {
                let what = Open::Chat {
                    agent: None,
                    cwd: ws.focused_cwd(cx),
                };
                if let Err(e) = ws.new_tab(what, None, w, cx) {
                    tracing::warn!("new chat: {e}");
                    let at = ws.agent_menu_at();
                    ws.open_agent_menu(at, w, cx);
                    ws.sidebar = true;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|ws, _: &OpenFiles, w, cx| {
                if let Err(e) = ws.open_editor("files", None, None, w, cx) {
                    tracing::warn!("could not open the editor: {e}");
                }
            }))
            .on_action(cx.listener(|ws, _: &OpenChanges, w, cx| {
                if let Err(e) = ws.open_editor("changes", None, None, w, cx) {
                    tracing::warn!("could not open the editor: {e}");
                }
            }))
            .on_action(cx.listener(|ws, _: &NewBrowser, w, cx| {
                if ws
                    .new_tab(
                        Open::Browser {
                            url: "about:blank".into(),
                        },
                        None,
                        w,
                        cx,
                    )
                    .is_ok()
                {
                    ws.focus_address(w, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &OpenSettings, w, cx| ws.open_settings(w, cx)))
            .on_action(cx.listener(|ws, _: &OpenUsage, w, cx| ws.open_page(PageKind::Usage, w, cx)))
            .on_action(cx.listener(|ws, _: &CloseIdleTabs, w, cx| ws.close_idle_tabs(w, cx)))
            .on_action(cx.listener(|ws, _: &NextNeedsYou, w, cx| ws.next_needs_you(w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomPane, _, cx| {
                ws.zoomed = match (ws.zoomed, ws.focused()) {
                    (Some(_), _) => None,
                    (None, f) => f,
                };
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &FilterSidebar, w, cx| ws.open_filter(w, cx)))
            .on_key_down(cx.listener(|ws, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && !ws.picked.is_empty() {
                    ws.picked.clear();
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|ws, _: &OpenPackages, w, cx| ws.open_page(PageKind::Packages, w, cx)))
            .on_action(cx.listener(|ws, _: &ImportBrowserData, w, cx| ws.open_settings_at(crate::settings_pane::Page::Browser, w, cx)))
            .on_action(cx.listener(|ws, _: &ToggleAgents, w, cx| {
                ws.sidebar = true;
                let at = ws.agent_menu_at();
                ws.open_agent_menu(at, w, cx);
            }))
            .on_action(cx.listener(|ws, _: &SplitRight, w, cx| {
                if let Some(p) = ws.focused() {
                    let what = ws.shell_here(cx);
                    let _ = ws.split(p, what, Axis::Row, w, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &SplitDown, w, cx| {
                if let Some(p) = ws.focused() {
                    let what = ws.shell_here(cx);
                    let _ = ws.split(p, what, Axis::Column, w, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &ClosePane, w, cx| {
                if let Some(p) = ws.focused() {
                    ws.close_pane(p, w, cx);
                }
            }))
            .on_action(cx.listener(|ws, _: &NextTab, w, cx| ws.cycle_tab(1, w, cx)))
            .on_action(cx.listener(|ws, _: &RecentTab, w, cx| ws.recent_tab(1, w, cx)))
            .on_action(cx.listener(|ws, _: &MoveToTab, w, cx| ws.move_to_tab(w, cx)))
            .on_action(cx.listener(|ws, _: &ConnectHost, w, cx| ws.open_remote_picker(w, cx)))
            .on_action(cx.listener(|ws, _: &ShowTurnDiff, w, cx| ws.show_turn_diff(w, cx)))
            .on_action(cx.listener(|ws, _: &RecentTabBack, w, cx| ws.recent_tab(-1, w, cx)))
            .on_action(cx.listener(|ws, _: &PrevTab, w, cx| ws.cycle_tab(-1, w, cx)))
            .on_action(cx.listener(|ws, _: &NextPane, w, cx| ws.cycle_pane(1, w, cx)))
            .on_action(cx.listener(|ws, _: &FocusLeft, w, cx| ws.focus_toward(Axis::Row, false, w, cx)))
            .on_action(cx.listener(|ws, _: &FocusRight, w, cx| ws.focus_toward(Axis::Row, true, w, cx)))
            .on_action(cx.listener(|ws, _: &FocusUp, w, cx| ws.focus_toward(Axis::Column, false, w, cx)))
            .on_action(cx.listener(|ws, _: &FocusDown, w, cx| ws.focus_toward(Axis::Column, true, w, cx)))
            .on_action(cx.listener(|ws, _: &PrevPane, w, cx| ws.cycle_pane(-1, w, cx)))
            .on_action(cx.listener(|ws, _: &FocusAddress, w, cx| ws.focus_address(w, cx)))
            .on_action(cx.listener(|ws, _: &Reload, _, cx| {
                if let Some((_, b)) = ws.browser_in_tab(ws.active) {
                    b.update(cx, |b, cx| {
                        b.reload();
                        cx.notify();
                    });
                }
            }))
            .on_action(cx.listener(|ws, _: &Find, w, cx| {
                if let Some(b) = ws.focused_browser_pane() {
                    if b.read(cx).is_editor() {
                        // The editor searches its own files.
                        b.read(cx).editor_key("find");
                    } else {
                        b.update(cx, |b, cx| b.open_find(w, cx));
                    }
                }
            }))
            .on_action(cx.listener(|ws, _: &FindNext, _, cx| {
                if let Some(b) = ws.focused_browser_pane() {
                    b.update(cx, |b, cx| b.find_step(false, cx));
                }
            }))
            .on_action(cx.listener(|ws, _: &FindPrevious, _, cx| {
                if let Some(b) = ws.focused_browser_pane() {
                    b.update(cx, |b, cx| b.find_step(true, cx));
                }
            }))
            .on_action(cx.listener(|ws, _: &ToggleDevTools, _, cx| {
                if let Some((_, b)) = ws.browser_in_tab(ws.active) {
                    b.read(cx).toggle_devtools();
                }
            }))
            .on_action(cx.listener(|ws, _: &ToggleSidebar, _, cx| {
                ws.sidebar = !ws.sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ToggleSidebarView, _, cx| {
                ws.recent = !ws.recent;
                ws.sidebar = true;
                cx.notify();
            }))
            .on_action(cx.listener(|ws, _: &ToggleInspector, w, cx| ws.toggle_inspector(w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomIn, w, cx| ws.zoom(1, w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomOut, w, cx| ws.zoom(-1, w, cx)))
            .on_action(cx.listener(|ws, _: &ZoomReset, w, cx| ws.zoom(0, w, cx)))
            .on_action(cx.listener(|ws, _: &ClearTerminal, _, cx| {
                if let Some(PaneKind::Terminal(t)) = ws
                    .focused()
                    .and_then(|id| ws.panes.get(&id))
                    .map(|p| &p.kind)
                {
                    t.update(cx, |t, cx| t.clear(cx));
                }
            }))
            // ⌘1 is Control; ⌘2–9 are the projects, as the sidebar lists them.
            .on_action(cx.listener(|ws, _: &Tab1, w, cx| ws.go_to_place(1, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab2, w, cx| ws.go_to_place(2, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab3, w, cx| ws.go_to_place(3, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab4, w, cx| ws.go_to_place(4, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab5, w, cx| ws.go_to_place(5, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab6, w, cx| ws.go_to_place(6, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab7, w, cx| ws.go_to_place(7, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab8, w, cx| ws.go_to_place(8, w, cx)))
            .on_action(cx.listener(|ws, _: &Tab9, w, cx| ws.go_to_place(9, w, cx)))
            // The side panels' edges (in `body`): drag to size them,
            // double-click for the usual width.
            .child(body)
            .children(self.render_job_drawer(cx))
            .children(bar)
            .children(self.menu.as_ref().map(crate::menu::Open::render))
            // The palette floats over everything, a little above centre,
            // on a scrim that closes it when clicked.
            .children(self.palette.as_ref().map(|(p, ..)| {
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .justify_center()
                    .pt(gpui::relative(0.12))
                    .bg(rgba(if theme::is_dark() {
                        0x00000059
                    } else {
                        0x00000026
                    }))
                    // Nothing under the scrim gets the wheel or the hover,
                    // so scrolling moves the palette's list, not the pane.
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|ws, _, window, cx| ws.close_palette(window, cx)),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(p.clone()),
                    )
            }))
            .when(self.usage_open, |d| d.child(self.render_usage_panel(cx)))
            .children(self.render_branch_picker(window, cx))
            .children(self.render_remote_picker(cx))
            .children(self.notice.as_ref().map(|n| self.render_notice(n, cx)))
            .children(self.toast.as_ref().map(|(text, _)| {
                div()
                    .absolute()
                    .bottom(u(20.))
                    .left_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .max_w(u(520.))
                            .px(u(14.))
                            .py(u(8.))
                            .rounded(u(10.))
                            .bg(rgb(theme::menu()))
                            .border_1()
                            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
                            .text_size(u(12.))
                            .text_color(rgb(theme::text()))
                            .child(text.clone()),
                    )
            }))
            .into_any_element()
    }
}

/// Wear `theme`: deck's own colours, and the appearance native views take
/// (traffic lights, menus, scrollbars, web pages), so nothing is left in
/// the other mode. System leaves the appearance to macOS.
fn wear(theme: crate::theme::Theme, window: &Window) {
    use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
    let system_dark = matches!(
        window.appearance(),
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    );
    crate::theme::set(theme, system_dark);
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let named = match theme {
        crate::theme::Theme::System => None,
        // SAFETY: AppKit constants.
        _ if crate::theme::is_dark() => Some(unsafe { NSAppearanceNameDarkAqua }),
        _ => Some(unsafe { NSAppearanceNameAqua }),
    };
    let appearance = named.and_then(NSAppearance::appearanceNamed);
    app.setAppearance(appearance.as_deref());
}

/// The Dock icon's badge: how many panes want you, or none.
fn set_badge(n: usize) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let label = (n > 0).then(|| objc2_foundation::NSString::from_str(&n.to_string()));
    app.dockTile().setBadgeLabel(label.as_deref());
}

/// "Needs you", with the bell: the one loud state, the same in every row.
fn alert_pill() -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(u(4.))
        .pl(u(6.))
        .pr(u(7.))
        .h(u(20.))
        .rounded(u(10.))
        .bg(rgb(theme::text()))
        .text_size(u(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::sidebar()))
        .child(icon(Icon::Bell, 11., theme::sidebar()))
        .child("Needs you")
}

/// A status in a quiet pill: what a thread needs, without colour.
/// The host picker: the folder to start in, the hosts (↑↓ to choose, ⏎ for
/// a terminal there), and the selected one's agents.
pub struct RemotePicker {
    hosts: Vec<crate::remote::Host>,
    field: Entity<crate::field::Field>,
    sel: usize,
    _sub: Subscription,
}

#[derive(Clone, Copy)]
pub enum RemoteOpen {
    Terminal,
    /// An agent's own interface, in a terminal on the host.
    Tui(&'static Agent),
    /// A Deck chat with the agent (by registry id) running on the host.
    Chat(&'static str),
}

pub struct BranchPicker {
    field: Entity<crate::field::Field>,
    _sub: Subscription,
    /// (branch, when it last had a commit)
    branches: Vec<(String, String)>,
    current: Option<String>,
    sel: usize,
    root: PathBuf,
    at: gpui::Point<gpui::Pixels>,
}

/// A row picked with ⌘-click: the accent's tint and edge, full strength.
fn picked_row(d: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    d.relative()
        .bg(rgba(theme::alpha(theme::accent(), 0x1f)))
        .border_color(rgba(theme::alpha(theme::accent(), 0x59)))
        .opacity(1.)
        // A check on the row's leading edge: picked, not just open.
        .child(
            div()
                .absolute()
                .left(u(-3.))
                .top(u(-3.))
                .size(u(14.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(theme::accent()))
                .child(icon(Icon::Check, 9., theme::pane())),
        )
}

#[derive(Clone, Copy, PartialEq)]
enum RowState {
    Working,
    Wants,
    Done,
    Idle,
}

/// A row's end: how long ago, then a small mark for its state: a turning
/// ring while it works, a bell when it needs you, a filled check when it
/// finished and you haven't looked.
fn row_trailer(id: ElementId, state: RowState, when: String) -> AnyElement {
    let mark: Option<AnyElement> = match state {
        RowState::Working => Some(spinner(id, 13., theme::text_2())),
        RowState::Wants => Some(icon(Icon::Bell, 13., theme::accent())),
        RowState::Done => Some(
            div()
                .size(u(14.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(theme::text()))
                .child(icon(Icon::Check, 9., theme::pane()))
                .into_any_element(),
        ),
        RowState::Idle => None,
    };
    div()
        .flex()
        .items_center()
        .gap(u(8.))
        .child(div().text_size(u(12.5)).text_color(rgb(theme::dim())).child(when))
        .children(mark)
        .into_any_element()
}

/// "Current", on the row you are in.
/// Background jobs and pages shown inline in the status bar before the
/// rest fold into "+N".
const BG_INLINE: usize = 4;

/// A background job or page, as the status bar shows it.
struct BgRow {
    id: PaneId,
    label: String,
    /// The command a job runs, or the page's host.
    detail: String,
    state: u32,
    failed: bool,
    open: bool,
}

fn current_tag() -> gpui::Div {
    div()
        .flex_none()
        .h(u(18.))
        .px(u(6.))
        .flex()
        .items_center()
        .rounded(u(4.))
        .bg(rgba(theme::alpha(theme::accent(), 0x26)))
        .text_size(u(10.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::accent()))
        .child("Current")
}

fn pill(label: SharedString) -> gpui::Div {
    let green = theme::palette().ansi[2];
    div()
        .flex_none()
        .h(u(20.))
        .flex()
        .items_center()
        .gap(u(3.))
        .text_size(u(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::text_2()))
        .when(label.as_ref() == "Done", |d| d.child(icon(Icon::Check, 11., green)))
        .child(label)
}

fn short_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m", s / 60)
    }
}

/// "now", "5m", "2h", "3d": how long since something happened, given
/// in seconds since the epoch.
fn ago(at: i64) -> String {
    let s = (crate::local::now() - at).max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The bytes a terminal gets for a named key: `enter`, `escape`, `tab`,
/// `up`, `ctrl+c`, `alt+b`, or a single character.
fn key_bytes(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let (mods, key) = match lower.rsplit_once('+') {
        Some((m, k)) if !k.is_empty() => (m.to_string(), k.to_string()),
        _ => (String::new(), lower.clone()),
    };
    let base = match key.as_str() {
        "enter" | "return" => "\r".to_string(),
        "escape" | "esc" => "\u{1b}".to_string(),
        "tab" => "\t".to_string(),
        "backspace" => "\u{7f}".to_string(),
        "space" => " ".to_string(),
        "up" => "\u{1b}[A".to_string(),
        "down" => "\u{1b}[B".to_string(),
        "right" => "\u{1b}[C".to_string(),
        "left" => "\u{1b}[D".to_string(),
        "home" => "\u{1b}[H".to_string(),
        "end" => "\u{1b}[F".to_string(),
        "delete" => "\u{1b}[3~".to_string(),
        "pageup" => "\u{1b}[5~".to_string(),
        "pagedown" => "\u{1b}[6~".to_string(),
        k if k.chars().count() == 1 => k.to_string(),
        _ => return None,
    };
    if mods.contains("ctrl") {
        let c = base.chars().next().filter(|c| c.is_ascii_alphabetic() && base.len() == 1)?;
        return Some(((c.to_ascii_lowercase() as u8 - b'a' + 1) as char).to_string());
    }
    if mods.contains("alt") || mods.contains("opt") {
        return Some(format!("\u{1b}{base}"));
    }
    if mods.contains("shift") && key == "tab" {
        return Some("\u{1b}[Z".to_string());
    }
    Some(base)
}

/// What a terminal agent's screen says it waits on you for, from the end
/// of its output: Claude Code's permission prompts and numbered choices,
/// Codex's approval, the others' yes/no questions. `None`: it doesn't.
fn agent_blocked(agent: &str, screen: &str) -> Option<&'static str> {
    // Only the bottom of the screen: a prompt answered long ago scrolls up.
    let tail: String = screen.lines().rev().take(14).collect::<Vec<_>>().join("\n").to_lowercase();
    let has = |s: &str| tail.contains(s);
    match agent {
        "claude" => {
            if has("do you want to proceed?") || has("do you want to make this edit") || has("do you want to create") {
                Some("Claude Code asks to go ahead")
            } else if has("❯ 1. yes") || (has("1. yes") && has("esc to cancel")) {
                Some("Claude Code is waiting for your choice")
            } else {
                None
            }
        }
        "codex" => (has("allow command?") || has("approve") && has("(y)") || has("▌ yes") || has("would you like to run")).then_some("Codex asks to run a command"),
        "gemini" => (has("allow execution") || has("apply this change?")).then_some("Gemini asks to go ahead"),
        _ => (has("(y/n)") || has("[y/n]") || has("continue?")).then_some("Asks you to confirm"),
    }
}

/// A job's name from its command: `npm run dev` → dev, `cargo watch -x
/// test` → cargo, `python -m http.server` → python.
fn job_name(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split_whitespace().filter(|w| !w.contains('=') && !w.starts_with('-')).collect();
    let pick = match words.as_slice() {
        [pm, "run", script, ..] if matches!(*pm, "npm" | "pnpm" | "yarn" | "bun") => *script,
        [pm, script, ..] if matches!(*pm, "pnpm" | "yarn" | "bun") && !matches!(*script, "x" | "dlx") => *script,
        [first, ..] => first.rsplit('/').next().unwrap_or(first),
        [] => "job",
    };
    pick.to_string()
}

fn str_param<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

/// zsh's default title is `user@host:~/dir`; the directory is the useful part.
fn short_title(t: &str) -> String {
    match t.split_once('@').and_then(|(_, rest)| rest.split_once(':')) {
        Some((_, dir)) if !dir.is_empty() => dir.to_string(),
        _ => t.to_string(),
    }
}

fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    // /dev/urandom cannot run dry; failing to open it means a broken system,
    // and an all-zero token would be guessable, so that is fatal.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Your snippets and the project's as palette rows.
fn snippet_items(root: Option<&std::path::Path>) -> Vec<Item> {
    crate::snippets::load_all(root)
        .into_iter()
        .filter(|s| !s.body.trim().is_empty())
        .map(|s| Item {
            kind: Kind::Snippet,
            detail: crate::snippets::detail(&s).into(),
            title: if s.name.trim().is_empty() {
                crate::snippets::preview(&s.body).into()
            } else {
                s.name.into()
            },
            icon: Icon::Snippet,
            hint: None,
            pick: crate::palette::Pick::Snippet(s.body),
        })
        .collect()
}

/// A question in the corner of the window, with its answers as buttons.
struct Notice {
    title: SharedString,
    detail: Option<SharedString>,
    /// The first is the main answer. Each closes the notice, then runs.
    buttons: Vec<(SharedString, NoticeAction)>,
}

type NoticeAction = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

impl Workspace {
    fn render_notice(&self, n: &Notice, cx: &mut Context<Self>) -> AnyElement {
        let mut buttons = div().flex().gap(u(8.)).pt(u(4.));
        for (ix, (label, action)) in n.buttons.iter().enumerate() {
            let action = action.clone();
            let primary = ix == 0;
            buttons = buttons.child(
                div()
                    .id(ElementId::NamedInteger("notice".into(), ix as u64))
                    .flex()
                    .items_center()
                    .h(u(26.))
                    .px(u(12.))
                    .rounded(u(7.))
                    .text_size(u(12.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(primary, |d| {
                        d.bg(rgb(theme::text()))
                            .text_color(rgb(theme::pane()))
                            .hover(|s| s.opacity(0.9))
                    })
                    .when(!primary, |d| {
                        d.border_1()
                            .border_color(rgba(theme::alpha(theme::text(), 0x26)))
                            .text_color(rgb(theme::text()))
                            .hover(|s| s.bg(rgb(theme::surface())))
                    })
                    .child(label.clone())
                    .on_click(cx.listener(move |ws, _, window, cx| {
                        ws.notice = None;
                        action(ws, window, cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .absolute()
            .bottom(u(18.))
            .right(u(18.))
            .w(u(340.))
            .flex()
            .flex_col()
            .gap(u(6.))
            .p(u(14.))
            .rounded(u(12.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .occlude()
            .child(
                div()
                    .text_size(u(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::text()))
                    .child(n.title.clone()),
            )
            .children(n.detail.clone().map(|d| {
                div()
                    .text_size(u(12.))
                    .line_height(u(17.))
                    .text_color(rgb(theme::text_2()))
                    .child(d)
            }))
            .child(buttons)
            .into_any_element()
    }
}

// -- account, updates and the lock ------------------------------------------------------

/// A button on the sign-in and lock screens: filled for the main action.
fn screen_button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(u(36.))
        .min_w(u(240.))
        .px(u(18.))
        .rounded(u(9.))
        .text_size(u(13.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .when(primary, |d| {
            d.bg(rgb(theme::text()))
                .text_color(rgb(theme::pane()))
                .hover(|s| s.opacity(0.9))
        })
        .when(!primary, |d| {
            d.border_1()
                .border_color(rgba(theme::alpha(theme::text(), 0x26)))
                .text_color(rgb(theme::text()))
                .hover(|s| s.bg(rgb(theme::surface())))
        })
        .child(label.into())
}

impl Workspace {
    fn refresh_settings_panes(&self, cx: &mut Context<Self>) {
        for pane in self.panes.values() {
            if let PaneKind::Settings(s) = &pane.kind {
                s.update(cx, |_, cx| cx.notify());
            }
        }
        cx.notify();
    }

    // -- account --------------------------------------------------------------------

    fn account_action(&mut self, a: crate::settings_pane::AccountAction, cx: &mut Context<Self>) {
        use crate::settings_pane::AccountAction as A;
        match a {
            A::SignIn => self.sign_in(cx),
            A::SignOut => {
                cx.spawn(async move |this, cx| {
                    crate::account::sign_out().await;
                    let _ = this.update(cx, |ws, cx| {
                        ws.flash("Signed out.", cx);
                        ws.refresh_settings_panes(cx);
                    });
                })
                .detach();
            }
            A::Checkout => {
                if let Some(u) = crate::account::checkout_url() {
                    cx.open_url(&u);
                }
            }
            A::Billing => {
                if let Some(u) = crate::account::billing_url() {
                    cx.open_url(&u);
                }
            }
        }
    }

    /// Open the sign-in page in the browser; it comes back as a `deck://`
    /// link.
    fn sign_in(&mut self, cx: &mut Context<Self>) {
        match crate::account::sign_in_url() {
            Some(url) => {
                self.signing_in = true;
                self.account_note = None;
                cx.open_url(&url);
            }
            None => self.flash("This build of Deck has no account server.", cx),
        }
        cx.notify();
    }

    /// `deck://auth/callback?…` after signing in, or `deck://refresh` after
    /// paying: load the account again.
    fn deck_url(&mut self, url: &str, cx: &mut Context<Self>) {
        if url.starts_with("deck://refresh") {
            self.refresh_account(cx);
            return;
        }
        match crate::account::callback(url) {
            Some(Ok(code)) => {
                cx.spawn(async move |this, cx| {
                    let r = crate::account::exchange(&code).await;
                    let _ = this.update(cx, |ws, cx| {
                        ws.signing_in = false;
                        match r {
                            Ok(a) => {
                                ws.account_note = None;
                                ws.flash(format!("Signed in as {}.", a.display_name()), cx);
                            }
                            Err(e) => ws.account_note = Some(e.into()),
                        }
                        ws.refresh_settings_panes(cx);
                    });
                })
                .detach();
            }
            Some(Err(e)) => {
                self.signing_in = false;
                self.account_note = Some(e.into());
                cx.notify();
            }
            None => tracing::info!("nothing handles {url}"),
        }
    }

    fn refresh_account(&mut self, cx: &mut Context<Self>) {
        if !crate::account::required() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let r = crate::account::refresh().await;
            let _ = this.update(cx, |ws, cx| {
                ws.account_note = r.err().map(Into::into);
                ws.refresh_settings_panes(cx);
            });
        })
        .detach();
    }

    /// With an account server: check the account now and every six hours,
    /// so a lapsed plan or a session ended elsewhere is noticed.
    fn start_account(&mut self, cx: &mut Context<Self>) {
        if !crate::account::required() {
            return;
        }
        cx.spawn(async move |this, cx| loop {
            if this.update(cx, |ws, cx| ws.refresh_account(cx)).is_err() {
                return;
            }
            cx.background_executor()
                .timer(Duration::from_secs(6 * 3600))
                .await;
        })
        .detach();
    }

    fn sign_in_screen(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let account = crate::account::current();
        let note = self.account_note.clone();
        let mut col = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(u(12.))
            .w(u(360.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(u(64.))
                    .mb(u(8.))
                    .rounded(u(16.))
                    .bg(rgb(theme::text()))
                    .text_size(u(30.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(theme::pane()))
                    .child("D"),
            );
        let title = |t: &str| {
            div()
                .text_size(u(22.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::text()))
                .child(t.to_string())
        };
        let words = |t: String| {
            div()
                .text_size(u(13.))
                .line_height(u(19.))
                .text_center()
                .text_color(rgb(theme::text_2()))
                .child(t)
        };
        match account {
            Some(a) if !a.active || a.trial_days_left() == Some(0) => {
                let (head, why) = if a.trial_ends.is_some() {
                    (
                        "Your free trial has ended",
                        "Choose a plan to keep using Deck. Your chats, tabs and settings are all still here.".to_string(),
                    )
                } else {
                    (
                        "Choose a plan",
                        format!("{} has no active plan. Pick one to keep using Deck.", a.email),
                    )
                };
                col = col.child(title(head)).child(words(why))
                    .child(div().h(u(8.)))
                    .child(
                        screen_button("gate-checkout", "Choose a Plan", true).on_click(
                            cx.listener(|ws, _, _, cx| {
                                ws.account_action(crate::settings_pane::AccountAction::Checkout, cx)
                            }),
                        ),
                    )
                    .child(
                        screen_button("gate-refresh", "I've Paid, Check Again", false)
                            .on_click(cx.listener(|ws, _, _, cx| ws.refresh_account(cx))),
                    )
                    .child(
                        screen_button("gate-out", "Sign Out", false).on_click(cx.listener(
                            |ws, _, _, cx| {
                                ws.account_action(crate::settings_pane::AccountAction::SignOut, cx)
                            },
                        )),
                    );
            }
            Some(_) => {
                // Signed in and paid, but not checked for a week: offline.
                col = col
                    .child(title("Can't reach Deck"))
                    .child(words(
                        "Deck needs to check your plan once a week, and couldn't. Check your connection and try again."
                            .into(),
                    ))
                    .child(div().h(u(8.)))
                    .child(
                        screen_button("gate-retry", "Try Again", true)
                            .on_click(cx.listener(|ws, _, _, cx| ws.refresh_account(cx))),
                    );
            }
            None => {
                col = col
                    .child(title("Sign in to Deck"))
                    .child(words(
                        "Terminals, a browser and your coding agents in one window. Sign in to start."
                            .into(),
                    ))
                    .child(div().h(u(8.)))
                    .child(
                        screen_button(
                            "gate-sign-in",
                            if self.signing_in {
                                "Open the Sign-in Page Again"
                            } else {
                                "Continue with Google"
                            },
                            true,
                        )
                        .on_click(cx.listener(|ws, _, _, cx| ws.sign_in(cx))),
                    )
                    .child(
                        div()
                            .text_size(u(12.))
                            .text_color(rgb(theme::dim()))
                            .child(if self.signing_in {
                                "Waiting for your browser…"
                            } else {
                                "Opens your browser; you come back here when you're done."
                            }),
                    );
            }
        }
        col = col.children(note.map(|n| {
            div()
                .pt(u(6.))
                .text_size(u(12.))
                .text_color(rgb(theme::DANGER))
                .child(n)
        }));
        self.screen(col)
    }

    /// A full window with `content` in the middle, under a strip that still
    /// moves the window.
    fn screen(&self, content: gpui::Div) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::pane()))
            .font_family(theme::UI)
            .child(div().flex_none().h(px(theme::TITLEBAR_PX)).on_mouse_down(
                MouseButton::Left,
                |ev, window, _| {
                    if ev.click_count == 2 {
                        window.titlebar_double_click();
                    }
                },
            ))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .pb(u(60.))
                    .child(content),
            )
            .into_any_element()
    }

    // -- lock -----------------------------------------------------------------------

    fn lock(&mut self, cx: &mut Context<Self>) {
        self.locked = true;
        self.menu = None;
        cx.notify();
    }

    fn unlock(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            if crate::lock::authenticate("unlock Deck").await {
                let _ = this.update(cx, |ws, cx| {
                    ws.locked = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn lock_screen(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let col = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(u(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(u(64.))
                    .mb(u(8.))
                    .rounded_full()
                    .bg(rgb(theme::surface()))
                    .child(icon(Icon::Lock, 28., theme::text())),
            )
            .child(
                div()
                    .text_size(u(22.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::text()))
                    .child("Deck is locked"),
            )
            .child(
                div()
                    .text_size(u(13.))
                    .text_color(rgb(theme::text_2()))
                    .child("Your terminals and agents keep running."),
            )
            .child(div().h(u(8.)))
            .child(
                screen_button("unlock", "Unlock", true)
                    .on_click(cx.listener(|ws, _, _, cx| ws.unlock(cx))),
            )
            .child(
                div()
                    .text_size(u(12.))
                    .text_color(rgb(theme::dim()))
                    .child("With Touch ID or your Mac's password"),
            );
        self.screen(col)
    }

    // -- the avatar menu --------------------------------------------------------

    fn avatar_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let account = crate::account::current();
        let disc = div()
            .flex()
            .items_center()
            .justify_center()
            .size(u(24.))
            .rounded_full();
        let disc = match &account {
            Some(a) => disc
                .bg(rgb(theme::accent()))
                .text_size(u(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::on_accent()))
                .child(a.initial()),
            None => disc
                .bg(rgb(theme::surface()))
                .child(icon(Icon::User, 13., theme::text_2())),
        };
        div()
            .id("account")
            .ml(u(6.))
            .rounded_full()
            .cursor_pointer()
            .hover(|s| s.opacity(0.85))
            .child(disc)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|ws, ev: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    ws.open_account_menu(ev.position, window, cx);
                }),
            )
            .into_any_element()
    }

    fn open_account_menu(
        &mut self,
        mut at: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::menu::{Entry, Item};
        let ws = cx.entity().downgrade();
        let account = crate::account::current();
        let mut entries: Vec<Entry> = Vec::new();
        let who = match &account {
            Some(a) => a.display_name().to_string(),
            None if crate::account::required() => "Sign In…".to_string(),
            None => "Account".to_string(),
        };
        let w = ws.clone();
        let signed_out = account.is_none() && crate::account::required();
        entries.push(
            Item::new(who, move |window, cx| {
                let _ = w.update(cx, |ws, cx| {
                    if signed_out {
                        ws.sign_in(cx);
                    } else {
                        ws.open_settings_at(crate::settings_pane::Page::General, window, cx);
                    }
                });
            })
            .icon(Icon::User)
            .into(),
        );
        if let Some(n) = account.as_ref().and_then(|a| a.trial_days_left()) {
            let left = match n {
                0 => "Free trial ended · Choose a Plan".to_string(),
                1 => "Free trial: last day · Choose a Plan".to_string(),
                n => format!("Free trial: {n} days left · Choose a Plan"),
            };
            let w = ws.clone();
            entries.push(
                Item::new(left, move |_, cx| {
                    let _ = w.update(cx, |ws, cx| {
                        ws.account_action(crate::settings_pane::AccountAction::Checkout, cx)
                    });
                })
                .into(),
            );
        }
        entries.push(Entry::Separator);
        entries.push(
            Item::new("Settings", |window, cx| {
                window.dispatch_action(Box::new(OpenSettings), cx)
            })
            .icon(Icon::Settings)
            .shortcut(hint("settings", &self.settings.keys))
            .into(),
        );
        let w = ws.clone();
        entries.push(
            Item::new("Theme", move |window, cx| {
                let _ = w.update(cx, |ws, cx| {
                    ws.open_settings_at(crate::settings_pane::Page::Appearance, window, cx)
                });
            })
            .icon(Icon::Theme)
            .into(),
        );
        entries.push(Entry::Separator);
        entries.push(
            Item::new("Lock", move |_, cx| {
                let _ = ws.update(cx, |ws, cx| ws.lock(cx));
            })
            .icon(Icon::Lock)
            .shortcut(hint("lock", &self.settings.keys))
            .danger()
            .into(),
        );
        // Under the avatar, its right edge on the window's.
        // Hung from the avatar, its right edge just past the button: the
        // avatar is not always at the window's edge (the inspector can sit
        // to its right).
        let zoom = self.settings.zoom;
        let width = window.viewport_size().width;
        let menu_w = px(crate::menu::MIN_W * zoom);
        at.x = (at.x + px(18. * zoom) - menu_w)
            .min(width - menu_w - px(8.))
            .max(px(8.));
        at.y = px(theme::TITLEBAR_PX * zoom);
        let restore = window.focused(cx);
        self.menu = Some(crate::menu::open(
            entries,
            at,
            restore,
            window,
            cx,
            |ws: &mut Self, _, _| ws.menu = None,
        ));
        cx.notify();
    }

    fn open_settings_at(
        &mut self,
        page: crate::settings_pane::Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(window, cx);
        if let Some(s) = self.panes.values().find_map(|p| match &p.kind {
            PaneKind::Settings(s) => Some(s.clone()),
            _ => None,
        }) {
            s.update(cx, |s, cx| s.show(page, cx));
        }
    }

    // -- updates --------------------------------------------------------------

    fn update_action(&mut self, a: crate::settings_pane::UpdateAction, cx: &mut Context<Self>) {
        use crate::settings_pane::UpdateAction as A;
        use crate::updates::State;
        match a {
            A::Check => self.check_updates(true, cx),
            A::Download => {
                let State::Available(r) = crate::updates::state() else {
                    return;
                };
                crate::updates::set(State::Downloading(r.clone()));
                self.refresh_settings_panes(cx);
                cx.spawn(async move |this, cx| {
                    let got = crate::updates::download(&r).await;
                    crate::updates::set(match got {
                        Ok(path) => State::Ready(r, path),
                        Err(e) => State::Failed(e),
                    });
                    let _ = this.update(cx, |ws, cx| ws.refresh_settings_panes(cx));
                })
                .detach();
            }
            A::Restart => {
                let State::Ready(_, path) = crate::updates::state() else {
                    return;
                };
                match crate::updates::install_on_quit(&path, true) {
                    Ok(()) => cx.quit(),
                    Err(e) => self.flash(format!("Could not update: {e}"), cx),
                }
            }
            A::Notes => match crate::updates::state() {
                State::Available(r) | State::Downloading(r) | State::Ready(r, _) => {
                    cx.open_url(&r.notes_url)
                }
                _ => {}
            },
        }
    }

    fn check_updates(&mut self, by_hand: bool, cx: &mut Context<Self>) {
        use crate::updates::State;
        if matches!(
            crate::updates::state(),
            State::Checking | State::Downloading(_) | State::Ready(..)
        ) {
            return;
        }
        crate::updates::set(State::Checking);
        self.refresh_settings_panes(cx);
        cx.spawn(async move |this, cx| {
            let r = crate::updates::check().await;
            let found = matches!(r, Ok(Some(_)));
            crate::updates::set(match r {
                Ok(Some(rel)) => State::Available(rel),
                Ok(None) => State::UpToDate,
                // A failed check nobody asked for stays quiet.
                Err(e) if by_hand => State::Failed(e),
                Err(_) => State::Idle,
            });
            let _ = this.update(cx, |ws, cx| {
                // Quiet updates: fetched now, put in place at the next quit.
                if found && ws.settings.install_updates {
                    ws.update_action(crate::settings_pane::UpdateAction::Download, cx);
                }
                ws.refresh_settings_panes(cx);
            });
        })
        .detach();
    }

    /// When Settings allows it: a check a minute after launch, then daily.
    fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut wait = Duration::from_secs(60);
            loop {
                cx.background_executor().timer(wait).await;
                wait = Duration::from_secs(24 * 3600);
                let alive = this.update(cx, |ws, cx| {
                    if ws.settings.check_updates {
                        ws.check_updates(false, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// In the title bar while there is news: an update to get, coming, or
    /// ready for a restart.
    fn update_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::settings_pane::UpdateAction as A;
        use crate::updates::State;
        let (label, action, busy): (&str, Option<A>, bool) = match crate::updates::state() {
            State::Available(_) => ("Update available", Some(A::Download), false),
            State::Downloading(_) => ("Downloading update", None, true),
            State::Ready(..) => ("Restart to update", Some(A::Restart), false),
            _ => return None,
        };
        Some(
            div()
                .id("update-pill")
                .flex()
                .items_center()
                .gap(u(6.))
                .h(u(24.))
                .ml(u(6.))
                .px(u(10.))
                .rounded(u(7.))
                .border_1()
                .border_color(rgba(theme::alpha(theme::text(), 0x26)))
                .text_size(u(12.))
                .text_color(rgb(theme::text()))
                .when(busy, |d| {
                    d.child(crate::icon::spinner("update-spin", 12., theme::text_2()))
                })
                .when(!busy, |d| {
                    d.child(icon(Icon::ArrowUp, 12., theme::text_2()))
                })
                .child(label)
                .when_some(action, |d, a| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::surface())))
                        .on_click(cx.listener(move |ws, _, _, cx| ws.update_action(a, cx)))
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_agents_are_read_off_the_screen() {
        let claude = "Bash command\n  rm -rf build\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n";
        assert!(agent_blocked("claude", claude).is_some());
        assert!(agent_blocked("claude", "✻ Thinking…\n> ").is_none());
        assert!(agent_blocked("codex", "Allow command?\n▌ Yes  No").is_some());
        assert!(agent_blocked("aider", "Create new file? (Y)es/(N)o [y/n]").is_some());
    }

    #[test]
    fn job_names_and_keys() {
        assert_eq!(job_name("npm run dev"), "dev");
        assert_eq!(job_name("pnpm dev --port 3001"), "dev");
        assert_eq!(job_name("PORT=3000 cargo watch -x test"), "cargo");
        assert_eq!(job_name("/usr/bin/python3 -m http.server"), "python3");
        assert_eq!(key_bytes("ctrl+c").as_deref(), Some("\u{3}"));
        assert_eq!(key_bytes("enter").as_deref(), Some("\r"));
        assert_eq!(key_bytes("up").as_deref(), Some("\u{1b}[A"));
        assert_eq!(key_bytes("alt+b").as_deref(), Some("\u{1b}b"));
        assert_eq!(key_bytes("shift+tab").as_deref(), Some("\u{1b}[Z"));
        assert_eq!(key_bytes("nonsense"), None);
    }

    #[test]
    fn recent_buckets_follow_the_calendar_not_rolling_days() {
        let ten_am = 10 * 3600;
        assert_eq!(Bucket::of(60, ten_am), Bucket::Today);
        assert_eq!(Bucket::of(ten_am, ten_am), Bucket::Today);
        // A minute before midnight, seen at 10:00, is yesterday.
        assert_eq!(Bucket::of(ten_am + 60, ten_am), Bucket::Yesterday);
        assert_eq!(Bucket::of(ten_am + 86_400, ten_am), Bucket::Yesterday);
        assert_eq!(Bucket::of(ten_am + 86_401, ten_am), Bucket::Week);
        assert_eq!(Bucket::of(ten_am + 6 * 86_400, ten_am), Bucket::Week);
        assert_eq!(Bucket::of(ten_am + 6 * 86_400 + 1, ten_am), Bucket::Earlier);
        // Just after midnight, an hour ago is already yesterday.
        assert_eq!(Bucket::of(3600, 60), Bucket::Yesterday);
    }

    #[test]
    fn only_ports_that_answer_http_are_web_servers() {
        use std::io::{Read, Write};
        let web = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (wp, op) = (web.local_addr().unwrap().port(), other.local_addr().unwrap().port());
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = web.accept() {
                let mut b = [0u8; 64];
                let _ = c.read(&mut b);
                let _ = c.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            }
        });
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = other.accept() {
                let _ = c.write_all(b"\x00\x01binary");
            }
        });
        assert!(speaks_http(wp));
        assert!(!speaks_http(op));
    }

    #[test]
    fn status_bar_lists_dev_servers_only() {
        assert!(dev_server(5173, "node"));
        assert!(dev_server(8000, "python3.12"));
        assert!(!dev_server(49755, "Google Chrome"));
        assert!(!dev_server(9222, "Google Chrome Helper"));
        assert!(!dev_server(61234, "someapp"));
        assert!(!dev_server(52974, "java"));
        assert!(dev_server(17565, "java"), "not ephemeral; the HTTP check decides");
        assert!(interactive("nvim") && interactive("htop") && !interactive("npm"));
    }

    #[test]
    fn status_bar_reads_ports_and_branches() {
        assert_eq!(url_port("http://localhost:3000/app"), Some(3000));
        assert_eq!(url_port("http://127.0.0.1:5173"), Some(5173));
        assert_eq!(url_port("https://example.com:8443/"), None);
        assert_eq!(url_port("http://localhost/"), None);
        let root = std::env::temp_dir().join(format!("deck-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(git_head(&root).as_deref(), Some("feat/x"));
        let wt = root.join("wt");
        std::fs::create_dir_all(root.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", root.join(".git/worktrees/wt").display())).unwrap();
        std::fs::write(root.join(".git/worktrees/wt/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_head(&wt).as_deref(), Some("0123456"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn leaves(n: &Node) -> Vec<PaneId> {
        let mut v = Vec::new();
        n.leaves(&mut v);
        v
    }

    #[test]
    fn splits_join_same_axis_runs_and_nest_across_axes() {
        let mut t = Node::Leaf(1);
        assert!(t.split(1, 2, Axis::Row));
        assert!(t.split(2, 3, Axis::Row));
        assert_eq!(
            t,
            Node::Split(
                Axis::Row,
                vec![Node::Leaf(1), Node::Leaf(2), Node::Leaf(3)],
                vec![1., 0.5, 0.5]
            )
        );
        assert!(t.split(2, 4, Axis::Column));
        assert_eq!(leaves(&t), vec![1, 2, 4, 3]);
        assert!(!t.split(99, 5, Axis::Row));
        assert!(t.contains(4) && !t.contains(5));
    }

    #[test]
    fn removing_collapses_single_child_splits() {
        let mut t = Node::Leaf(1);
        t.split(1, 2, Axis::Row);
        t.split(2, 3, Axis::Column);
        assert!(t.remove(3));
        assert_eq!(
            t,
            Node::Split(Axis::Row, vec![Node::Leaf(1), Node::Leaf(2)], vec![1., 1.])
        );
        assert!(t.remove(1));
        assert_eq!(t, Node::Leaf(2));
        assert!(!t.remove(7));
    }

    #[test]
    fn neighbours_by_direction() {
        // 1 | 2
        //     -
        //     4 | 5      (and 3 to the right of everything)
        let mut t = Node::Leaf(1);
        t.split(1, 2, Axis::Row);
        t.split(2, 3, Axis::Row);
        t.split(2, 4, Axis::Column);
        t.split(4, 5, Axis::Row);
        let go = |id, axis, fwd| t.neighbor(id, axis, fwd).flatten();
        assert_eq!(go(1, Axis::Row, true), Some(2));
        assert_eq!(go(2, Axis::Row, false), Some(1));
        assert_eq!(go(2, Axis::Column, true), Some(4));
        assert_eq!(go(4, Axis::Column, false), Some(2));
        assert_eq!(go(4, Axis::Row, true), Some(5));
        assert_eq!(go(5, Axis::Row, true), Some(3));
        // Left from 3 lands on the stack beside it, at its top.
        assert_eq!(go(3, Axis::Row, false), Some(2));
        assert_eq!(go(1, Axis::Row, false), None);
        assert_eq!(go(1, Axis::Column, true), None);
        assert_eq!(t.neighbor(42, Axis::Row, true), None);
    }

    #[test]
    fn titles_and_hosts_shorten() {
        assert_eq!(short_title("nees@MacBook-Pro:~/deck"), "~/deck");
        assert_eq!(short_title("vim"), "vim");
        assert_eq!(host("https://example.com/a?b"), "example.com");
        assert_eq!(host("http://localhost:5173/"), "localhost:5173");
    }
}
