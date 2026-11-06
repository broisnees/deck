//! A task's review: everything it changed against where it started, file by
//! file, and what to do with it: merge it back, open a pull request, or
//! throw it away. The workspace runs those; this pane shows and asks.

use crate::icon::{icon, spinner, Icon};
use crate::tasks::{self, FileDiff, Line};
use crate::theme::{self, u, OneLine};
use gpui::prelude::*;
use gpui::{
    div, rgb, rgba, AnyElement, Context, ElementId, EventEmitter, FocusHandle, FontWeight,
    ScrollHandle, SharedString, Window,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub enum ReviewEvent {
    Merge,
    OpenPr,
    Discard,
    /// A terminal in the task's worktree, beside the review.
    Terminal,
    /// The task's app, run from its worktree, open beside the review.
    Preview,
}

/// Lines drawn per file before the rest is summed up; a lockfile churn
/// should not bury the change that matters.
const MAX_LINES: usize = 800;

pub struct ReviewPane {
    pub task: tasks::Task,
    pub focus: FocusHandle,
    files: Vec<FileDiff>,
    loading: bool,
    error: Option<String>,
    /// What is running ("Merging…"), while a button's work is under way.
    working: Option<&'static str>,
    /// Discard asks twice: armed by the first click, for a few seconds.
    discard_armed: Option<Instant>,
    collapsed: HashSet<usize>,
    scroll: ScrollHandle,
    _load: Option<gpui::Task<()>>,
}

impl EventEmitter<ReviewEvent> for ReviewPane {}

impl ReviewPane {
    pub fn new(task: tasks::Task, cx: &mut Context<Self>) -> Self {
        let mut pane = Self {
            task,
            focus: cx.focus_handle(),
            files: Vec::new(),
            loading: true,
            error: None,
            working: None,
            discard_armed: None,
            collapsed: HashSet::new(),
            scroll: ScrollHandle::new(),
            _load: None,
        };
        pane.reload(cx);
        pane
    }

    /// Read the diff again (the agent may still be working).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let task = self.task.clone();
        self.loading = true;
        self._load = Some(cx.spawn(async move |this, cx| {
            let diff = cx
                .background_executor()
                .spawn(async move { tasks::diff(&task) })
                .await;
            let _ = this.update(cx, |p, cx| {
                p.loading = false;
                match diff {
                    Ok(text) => {
                        p.files = tasks::parse_diff(&text);
                        p.error = None;
                    }
                    Err(e) => p.error = Some(e),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn set_working(&mut self, what: Option<&'static str>, cx: &mut Context<Self>) {
        self.working = what;
        if what.is_some() {
            self.error = None;
        }
        cx.notify();
    }

    pub fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        self.working = None;
        self.error = Some(error);
        cx.notify();
    }

    fn button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        glyph: Icon,
        primary: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let idle = self.working.is_none();
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .gap(u(6.))
            .h(u(28.))
            .px(u(10.))
            .rounded(u(7.))
            .text_size(u(12.))
            .when(primary, |d| {
                d.bg(rgb(theme::text()))
                    .text_color(rgb(theme::pane()))
                    .font_weight(FontWeight::MEDIUM)
            })
            .when(!primary, |d| {
                d.bg(rgb(theme::surface()))
                    .border_1()
                    .border_color(rgba(theme::hairline()))
                    .text_color(rgb(theme::text()))
            })
            .when(idle, |d| d.cursor_pointer().hover(|s| s.opacity(0.85)))
            .when(!idle, |d| d.opacity(0.5))
            .child(icon(
                glyph,
                13.,
                if primary {
                    theme::pane()
                } else {
                    theme::text_2()
                },
            ))
            .child(label.into())
            .on_click(cx.listener(move |p, _, _, cx| {
                if p.working.is_none() {
                    on(p, cx);
                }
            }))
            .into_any_element()
    }

    fn render_file(&self, ix: usize, f: &FileDiff, cx: &mut Context<Self>) -> AnyElement {
        let open = !self.collapsed.contains(&ix);
        let ansi = theme::current().ansi;
        let (green, red) = (ansi[2], ansi[1]);
        let header = div()
            .id(ElementId::NamedInteger("file".into(), ix as u64))
            .flex()
            .items_center()
            .gap(u(8.))
            .h(u(34.))
            .px(u(12.))
            .bg(rgb(theme::raised()))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::row_hover())))
            .child(icon(
                if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                12.,
                theme::dim(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .one_line()
                    .font_family(theme::mono())
                    .text_size(u(12.))
                    .text_color(rgb(theme::text()))
                    .child(f.path.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap(u(6.))
                    .font_family(theme::mono())
                    .text_size(u(11.))
                    .when(f.added > 0, |d| {
                        d.child(div().text_color(rgb(green)).child(format!("+{}", f.added)))
                    })
                    .when(f.removed > 0, |d| {
                        d.child(div().text_color(rgb(red)).child(format!("−{}", f.removed)))
                    }),
            )
            .on_click(cx.listener(move |p, _, _, cx| {
                if !p.collapsed.remove(&ix) {
                    p.collapsed.insert(ix);
                }
                cx.notify();
            }));
        let mut body = div()
            .flex()
            .flex_col()
            .py(u(4.))
            .font_family(theme::mono())
            .text_size(u(12.))
            .line_height(u(18.));
        if open {
            for line in f.lines.iter().take(MAX_LINES) {
                let (mark, text, bg, fg) = match line {
                    Line::Hunk(h) => (" ", h.as_str(), Some(theme::tint()), theme::dim()),
                    Line::Add(t) => (
                        "+",
                        t.as_str(),
                        Some(theme::alpha(green, 0x24)),
                        theme::text(),
                    ),
                    Line::Del(t) => (
                        "−",
                        t.as_str(),
                        Some(theme::alpha(red, 0x24)),
                        theme::text_2(),
                    ),
                    Line::Same(t) => (" ", t.as_str(), None, theme::text_2()),
                    Line::Note(n) => (" ", n.as_str(), None, theme::dim()),
                };
                body = body.child(
                    div()
                        .flex()
                        .px(u(12.))
                        .when_some(bg, |d, bg| d.bg(rgba(bg)))
                        .text_color(rgb(fg))
                        .child(
                            div()
                                .flex_none()
                                .w(u(16.))
                                .text_color(rgb(theme::dim()))
                                .child(mark),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .child(if text.is_empty() {
                                    " ".to_string()
                                } else {
                                    text.replace('\t', "    ")
                                }),
                        ),
                );
            }
            if f.lines.len() > MAX_LINES {
                body = body.child(
                    div()
                        .px(u(12.))
                        .py(u(6.))
                        .text_color(rgb(theme::dim()))
                        .child(format!("{} more lines", f.lines.len() - MAX_LINES)),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .rounded(u(10.))
            .overflow_hidden()
            .border_1()
            .border_color(rgba(theme::hairline()))
            .child(header)
            .when(open, |d| d.child(body))
            .into_any_element()
    }
}

impl Render for ReviewPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let added: usize = self.files.iter().map(|f| f.added).sum();
        let removed: usize = self.files.iter().map(|f| f.removed).sum();
        let armed = self
            .discard_armed
            .is_some_and(|t| t.elapsed() < Duration::from_secs(4));
        let summary = if self.loading && self.files.is_empty() {
            "Reading changes…".to_string()
        } else {
            format!(
                "{} → {} · {} file{} · +{added} −{removed}",
                self.task.branch,
                self.task.base,
                self.files.len(),
                if self.files.len() == 1 { "" } else { "s" }
            )
        };

        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(u(8.))
            .child(
                self.button("preview", "Preview", Icon::Globe, false, cx, |_, cx| {
                    cx.emit(ReviewEvent::Preview)
                }),
            )
            .child(self.button(
                "terminal",
                "Terminal",
                Icon::Terminal,
                false,
                cx,
                |_, cx| cx.emit(ReviewEvent::Terminal),
            ))
            .child(
                self.button("refresh", "Refresh", Icon::Refresh, false, cx, |p, cx| {
                    p.reload(cx)
                }),
            )
            .child(div().flex_1())
            .child(self.button(
                "discard",
                if armed {
                    "Click again to discard"
                } else {
                    "Discard"
                },
                Icon::Close,
                false,
                cx,
                |p, cx| {
                    let armed = p
                        .discard_armed
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(4));
                    if armed {
                        p.discard_armed = None;
                        cx.emit(ReviewEvent::Discard);
                    } else {
                        p.discard_armed = Some(Instant::now());
                        cx.notify();
                    }
                },
            ))
            .child(
                self.button("pr", "Open PR", Icon::ArrowUpRight, false, cx, |_, cx| {
                    cx.emit(ReviewEvent::OpenPr)
                }),
            )
            .child(self.button(
                "merge",
                format!("Merge into {}", self.task.base),
                Icon::Check,
                true,
                cx,
                |_, cx| cx.emit(ReviewEvent::Merge),
            ));

        let mut col = div()
            .flex()
            .flex_col()
            .gap(u(14.))
            .w_full()
            .max_w(u(980.))
            .px(u(24.))
            .pt(u(24.))
            .pb(u(48.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(4.))
                    .child(
                        div()
                            .text_size(u(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::text()))
                            .child(self.task.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .text_size(u(12.))
                            .text_color(rgb(theme::dim()))
                            .child(icon(Icon::Git, 12., theme::dim()))
                            .child(summary),
                    ),
            )
            .child(actions);
        if let Some(what) = self.working {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text_2()))
                    .child(spinner("review-work", 12., theme::text_2()))
                    .child(what),
            );
        }
        if let Some(e) = &self.error {
            col = col.child(
                div()
                    .px(u(12.))
                    .py(u(10.))
                    .rounded(u(10.))
                    .border_1()
                    .border_color(rgba(theme::strong_edge()))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text()))
                    .child(e.clone()),
            );
        }
        if !self.loading && self.files.is_empty() && self.error.is_none() {
            col = col.child(
                div()
                    .py(u(20.))
                    .text_size(u(13.))
                    .text_color(rgb(theme::dim()))
                    .child("No changes yet. The agent may still be working on it."),
            );
        }
        let files: Vec<FileDiff> = self.files.clone();
        for (ix, f) in files.iter().enumerate() {
            col = col.child(self.render_file(ix, f, cx));
        }

        div()
            .id("review")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(rgb(theme::pane()))
            .child(div().flex().justify_center().child(col))
    }
}
