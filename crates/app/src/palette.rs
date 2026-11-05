//! The command palette (⌘K): one box for everything. It finds open tabs,
//! actions, files of the project you are in and pages you have visited, by
//! fuzzy match, and starts a task from whatever you typed.
//!
//! The palette only lists and picks; the workspace gives it the items when it
//! opens and runs the one picked.

use crate::field::{Field, FieldEvent};
use crate::fuzzy;
use crate::icon::{icon, Icon};
use crate::theme::{self, u, OneLine};
use gpui::prelude::*;
use gpui::{
    div, relative, rgb, rgba, Action, Context, ElementId, Entity, EventEmitter, FocusHandle,
    FontWeight, HighlightStyle, SharedString, StyledText, Subscription, Window,
};
use std::path::PathBuf;

/// What a row does when picked.
pub enum Pick {
    Pane(u64),
    Action(Box<dyn Action>),
    File(PathBuf),
    Page(String),
    /// Start a task with the typed text as its brief.
    NewTask(String),
    /// Open the review of the task with this id.
    Review(String),
    /// Put this snippet's text into the pane the palette came from.
    Snippet(String),
    /// Keep the typed text as a new snippet.
    SaveSnippet(String),
    /// Type this command at the terminal's prompt, as it was (not run).
    Command(String),
}

impl Clone for Pick {
    fn clone(&self) -> Self {
        match self {
            Pick::Pane(p) => Pick::Pane(*p),
            Pick::Action(a) => Pick::Action(a.boxed_clone()),
            Pick::File(f) => Pick::File(f.clone()),
            Pick::Page(u) => Pick::Page(u.clone()),
            Pick::NewTask(t) => Pick::NewTask(t.clone()),
            Pick::Review(t) => Pick::Review(t.clone()),
            Pick::Snippet(t) => Pick::Snippet(t.clone()),
            Pick::SaveSnippet(t) => Pick::SaveSnippet(t.clone()),
            Pick::Command(t) => Pick::Command(t.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Tab,
    Action,
    Snippet,
    File,
    Page,
    Command,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Tab => "Tabs",
            Kind::Action => "Actions",
            Kind::Snippet => "Snippets",
            Kind::File => "Files",
            Kind::Page => "Pages",
            Kind::Command => "History",
        }
    }
}

pub struct Item {
    pub kind: Kind,
    pub title: SharedString,
    pub detail: SharedString,
    pub icon: Icon,
    /// Its shortcut, as the UI shows it.
    pub hint: Option<SharedString>,
    pub pick: Pick,
}

pub enum PaletteEvent {
    Run(Pick),
    Close,
    /// The text changed; the workspace may add pages that match it.
    Query(String),
}

/// Rows shown at most; past this a sharper query is quicker than scrolling.
const MAX_ROWS: usize = 60;

/// What a palette offers: everything (⌘K), only actions (⌃K), or only
/// the open tabs to switch to (⌘P / ⌃P).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    All,
    Actions,
    Tabs,
}

pub struct Palette {
    pub field: Entity<Field>,
    scope: Scope,
    items: Vec<Item>,
    /// Pages matched by history for the current text, kept apart from the
    /// items given at open.
    pages: Vec<Item>,
    /// Whether "Start a task" is offered (the focused pane is in a repo).
    tasks: bool,
    /// Opened with ⌘;: only snippets, all of them shown before anything is
    /// typed, and what you type can be kept as a new one.
    snippets_only: bool,
    /// Opened with ⌃R in a terminal: the commands run there and in your
    /// shell's history, newest first, all shown before anything is typed.
    history_only: bool,
    /// (item index, matched byte positions), best first.
    shown: Vec<(Ref, Vec<usize>)>,
    selected: usize,
    /// Each row's place among the list's children, which section labels
    /// share, so the selection can be scrolled into view.
    child_of: Vec<usize>,
    scroll: gpui::ScrollHandle,
    _sub: Subscription,
}

#[derive(Clone, Copy, PartialEq)]
enum Ref {
    Item(usize),
    Page(usize),
    NewTask,
    NewSnippet,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    pub fn new(items: Vec<Item>, tasks: bool, cx: &mut Context<Self>) -> Self {
        let hint = if tasks {
            "Search tabs, actions, snippets, files and pages, or describe a task…"
        } else {
            "Search tabs, actions, snippets, files and pages…"
        };
        Self::build(items, tasks, false, hint, cx)
    }

    /// Only actions, or only tabs: the same palette, narrowed.
    pub fn scoped(items: Vec<Item>, scope: Scope, cx: &mut Context<Self>) -> Self {
        let hint = match scope {
            Scope::Actions => "Run an action…",
            Scope::Tabs => "Switch to a chat, terminal or page…",
            Scope::All => "Search tabs, actions, snippets, files and pages…",
        };
        let mut p = Self::build(items, false, false, hint, cx);
        p.scope = scope;
        p
    }

    /// Commands to run again (⌃R in a terminal), newest first.
    pub fn history(items: Vec<Item>, cx: &mut Context<Self>) -> Self {
        let mut p = Self::build(items, false, false, "Search commands you've run…", cx);
        p.history_only = true;
        p.refilter(cx);
        p
    }

    /// The snippets alone (⌘;).
    pub fn snippets(items: Vec<Item>, cx: &mut Context<Self>) -> Self {
        Self::build(
            items,
            false,
            true,
            "Insert a snippet, or type a new one to keep…",
            cx,
        )
    }

    fn build(
        items: Vec<Item>,
        tasks: bool,
        snippets_only: bool,
        hint: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = cx.new(|cx| {
            let mut f = Field::new(hint, cx);
            f.bare = true;
            f.popup = true;
            f
        });
        let sub = cx.subscribe(&field, |p, _, ev: &FieldEvent, cx| match ev {
            FieldEvent::Changed => {
                let q = p.query(cx);
                cx.emit(PaletteEvent::Query(q));
                p.refilter(cx);
            }
            FieldEvent::PopupKey("up") => p.step(-1, cx),
            FieldEvent::PopupKey("down") | FieldEvent::PopupKey("tab") => p.step(1, cx),
            FieldEvent::PopupKey("enter") => p.run(cx),
            FieldEvent::PopupKey("escape") | FieldEvent::Cancel => cx.emit(PaletteEvent::Close),
            _ => {}
        });
        let mut p = Self {
            field,
            scope: Scope::All,
            items,
            pages: Vec::new(),
            tasks,
            snippets_only,
            history_only: false,
            shown: Vec::new(),
            selected: 0,
            child_of: Vec::new(),
            scroll: gpui::ScrollHandle::new(),
            _sub: sub,
        };
        p.refilter(cx);
        p
    }

    pub fn focus(&self, cx: &gpui::App) -> FocusHandle {
        self.field.read(cx).focus.clone()
    }

    fn query(&self, cx: &gpui::App) -> String {
        self.field.read(cx).text().trim().to_string()
    }

    /// Files arrive a moment after the palette opens (listing the project
    /// runs in the background).
    pub fn add(&mut self, mut more: Vec<Item>, cx: &mut Context<Self>) {
        self.items.append(&mut more);
        self.refilter(cx);
    }

    pub fn set_pages(&mut self, pages: Vec<Item>, cx: &mut Context<Self>) {
        self.pages = pages;
        self.refilter(cx);
    }

    fn item(&self, r: Ref) -> Option<&Item> {
        match r {
            Ref::Item(i) => self.items.get(i),
            Ref::Page(i) => self.pages.get(i),
            Ref::NewTask | Ref::NewSnippet => None,
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.query(cx);
        let mut hits: Vec<(i32, Kind, Ref, Vec<usize>)> = Vec::new();
        if q.is_empty() {
            // Nothing typed: the tabs, then what you can do. Files and pages
            // only once there is something to look for.
            for (i, it) in self.items.iter().enumerate() {
                if (!self.history_only && matches!(it.kind, Kind::Tab | Kind::Action))
                    || (self.snippets_only && it.kind == Kind::Snippet)
                    || (self.history_only && it.kind == Kind::Command)
                {
                    hits.push((0, it.kind, Ref::Item(i), Vec::new()));
                }
            }
        } else {
            let all = self
                .items
                .iter()
                .enumerate()
                .map(|(i, it)| (Ref::Item(i), it))
                .chain(
                    self.pages
                        .iter()
                        .enumerate()
                        .map(|(i, it)| (Ref::Page(i), it)),
                );
            for (r, it) in all {
                if it.kind == Kind::File && q.chars().count() < 2 {
                    continue;
                }
                // The title counts most; the detail (a path, a host) may
                // carry the match at a discount.
                let by_title = fuzzy::score(&q, &it.title);
                let by_detail = fuzzy::score(&q, &it.detail).map(|(s, _)| (s - 15, Vec::new()));
                let best = match (by_title, by_detail) {
                    (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
                    (a, b) => a.or(b),
                };
                if let Some((s, at)) = best {
                    // Tabs and actions are what people reach for most.
                    let kind_bonus = match it.kind {
                        Kind::Tab => 6,
                        Kind::Action => 4,
                        Kind::Snippet => 3,
                        Kind::Page => 0,
                        Kind::File => -2,
                        Kind::Command => 0,
                    };
                    hits.push((s + kind_bonus, it.kind, r, at));
                }
            }
            hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        }
        hits.truncate(MAX_ROWS);
        self.shown = hits.into_iter().map(|(_, _, r, at)| (r, at)).collect();
        if self.tasks && !q.is_empty() {
            self.shown.push((Ref::NewTask, Vec::new()));
        }
        if self.snippets_only && !q.is_empty() {
            self.shown.push((Ref::NewSnippet, Vec::new()));
        }
        // Labels sit between kinds only while nothing is typed.
        self.child_of.clear();
        let mut last: Option<Kind> = None;
        let mut child = 0;
        for (r, _) in &self.shown {
            let kind = self.item(*r).map(|it| it.kind);
            if q.is_empty() && kind.is_some() && kind != last {
                child += 1;
                last = kind;
            }
            self.child_of.push(child);
            child += 1;
        }
        self.selected = 0;
        self.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        cx.notify();
    }

    fn step(&mut self, dir: isize, cx: &mut Context<Self>) {
        let n = self.shown.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + dir).rem_euclid(n) as usize;
            if let Some(&child) = self.child_of.get(self.selected) {
                self.scroll.scroll_to_item(child);
            }
            cx.notify();
        }
    }

    fn run_at(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some((r, _)) = self.shown.get(ix) else {
            return;
        };
        let pick = match r {
            Ref::NewTask => Pick::NewTask(self.query(cx)),
            Ref::NewSnippet => Pick::SaveSnippet(self.field.read(cx).text().to_string()),
            r => match self.item(*r) {
                Some(it) => it.pick.clone(),
                None => return,
            },
        };
        cx.emit(PaletteEvent::Run(pick));
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        self.run_at(self.selected, cx);
    }
}

/// `text` with the matched characters in full strength, so you can see why
/// a row came up.
fn lit(text: &SharedString, at: &[usize]) -> StyledText {
    let lights = at
        .iter()
        .filter_map(|&i| {
            let ch = text.get(i..)?.chars().next()?;
            Some((
                i..i + ch.len_utf8(),
                HighlightStyle {
                    color: Some(rgb(theme::text()).into()),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            ))
        })
        .collect::<Vec<_>>();
    let lights = theme::safe_highlights(&text, lights);
    StyledText::new(text.clone()).with_highlights(lights)
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let q = self.query(cx);
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .gap(u(2.))
            .max_h(u(400.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p(u(6.));
        let mut last_kind: Option<Kind> = None;
        for (row, (r, at)) in self.shown.iter().enumerate() {
            let selected = row == self.selected;
            let (title, detail, glyph, hint, kind): (
                StyledText,
                SharedString,
                Icon,
                Option<SharedString>,
                Option<Kind>,
            ) = match r {
                Ref::NewTask => (
                    StyledText::new(SharedString::from(format!("Start a task: {q}"))),
                    "A new branch and worktree, with an agent working on it".into(),
                    Icon::Git,
                    Some("⏎".into()),
                    None,
                ),
                Ref::NewSnippet => (
                    StyledText::new(SharedString::from(format!("Keep as a snippet: {q}"))),
                    "Saved for ⌘; to put in again".into(),
                    Icon::Plus,
                    None,
                    None,
                ),
                r => {
                    let Some(it) = self.item(*r) else { continue };
                    (
                        lit(&it.title, at),
                        it.detail.clone(),
                        it.icon,
                        it.hint.clone(),
                        Some(it.kind),
                    )
                }
            };
            // A quiet label where the kind changes, when nothing is typed
            // (a narrowed palette is one kind already).
            if q.is_empty() && kind != last_kind && self.scope == Scope::All {
                if let Some(k) = kind {
                    list = list.child(
                        div()
                            .px(u(10.))
                            .pt(u(if row == 0 { 6. } else { 14. }))
                            .pb(u(6.))
                            .text_size(u(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::dim()))
                            .child(k.label()),
                    );
                }
                last_kind = kind;
            }
            list = list.child(
                div()
                    .id(ElementId::NamedInteger("pal".into(), row as u64))
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .h(u(32.))
                    .px(u(10.))
                    // Concentric with the palette: its 12 less the list's 6.
                    .rounded(u(6.))
                    .text_size(u(13.))
                    .cursor_pointer()
                    // The sidebar's own row states, so the palette reads as
                    // part of the window in every theme.
                    .when(selected, |d| d.bg(rgb(theme::surface())))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(theme::row_hover()))))
                    .child(
                        div()
                            .flex_none()
                            .w(u(18.))
                            .flex()
                            .justify_center()
                            .child(icon(glyph, 15., if selected { theme::text() } else { theme::dim() })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .gap(u(10.))
                            .child(
                                div()
                                    .flex_shrink()
                                    .min_w_0()
                                    .one_line()
                                    .text_color(rgb(if selected { theme::text() } else { theme::text_2() }))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(u(200.))
                                    .one_line()
                                    .text_size(u(12.))
                                    .text_color(rgb(theme::dim()))
                                    .child(detail),
                            ),
                    )
                    .when(
                        !q.is_empty() && kind.is_some() && !self.snippets_only && self.scope == Scope::All,
                        |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(u(11.))
                                    .text_color(rgb(theme::dim()))
                                    .child(kind.map(Kind::label).unwrap_or_default()),
                            )
                        },
                    )
                    .children(hint.map(|h| {
                        div()
                            .flex_none()
                            .text_size(u(11.5))
                            .text_color(rgb(if selected { theme::text_2() } else { theme::dim() }))
                            .child(h)
                    }))
                    .on_click(cx.listener(move |p, _, _, cx| p.run_at(row, cx))),
            );
        }
        if self.shown.is_empty() {
            list = list.child(
                div()
                    .px(u(12.))
                    .py(u(22.))
                    .text_size(u(13.))
                    .text_color(rgb(theme::dim()))
                    .child(if self.snippets_only {
                        "No snippets yet. Type one and press ⏎ to keep it."
                    } else if self.history_only {
                        "No commands yet"
                    } else {
                        "Nothing matches"
                    }),
            );
        }

        div()
            .id("palette")
            .w(u(560.))
            .max_w(relative(0.9))
            .flex()
            .flex_col()
            .rounded(u(12.))
            // The panes' own colour with a firmer edge, not a menu grey.
            .bg(rgb(theme::pane()))
            .border_1()
            .border_color(rgba(theme::strong_edge()))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(10.))
                    .h(u(46.))
                    .pl(u(16.))
                    .pr(u(12.))
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(14.))
                    .child(icon(
                        match self.scope {
                            Scope::Tabs => Icon::Sidebar,
                            _ => Icon::Search,
                        },
                        15.,
                        theme::dim(),
                    ))
                    // A narrowed palette says what it holds.
                    .children((self.scope != Scope::All).then(|| {
                        div()
                            .flex_none()
                            .px(u(7.))
                            .h(u(20.))
                            .flex()
                            .items_center()
                            .rounded(u(5.))
                            .bg(rgba(theme::tint()))
                            .text_size(u(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::text_2()))
                            .child(if self.scope == Scope::Tabs { "Tabs" } else { "Actions" })
                    }))
                    .child(div().flex_1().min_w_0().child(self.field.clone()))
                    .child(
                        div()
                            .flex_none()
                            .px(u(6.))
                            .h(u(20.))
                            .flex()
                            .items_center()
                            .rounded(u(5.))
                            .bg(rgba(theme::tint()))
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child("esc"),
                    ),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(16.))
                    .h(u(32.))
                    .px(u(16.))
                    .border_t_1()
                    .border_color(rgba(theme::hairline()))
                    .text_size(u(11.))
                    .text_color(rgb(theme::dim()))
                    .child(
                        crate::kbd::hints(match self.scope {
                            Scope::All => "↑↓ choose · ⏎ open · ⌃K actions · ⌘P tabs · esc close",
                            Scope::Actions => "↑↓ choose · ⏎ run · ⌘K everything · esc close",
                            Scope::Tabs => "↑↓ choose · ⏎ switch · ⌘K everything · esc close",
                        })
                        .gap_x(u(18.)),
                    ),
            )
            .into_any_element()
    }
}
