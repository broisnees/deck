//! The new tab page (⌘N): pick what the tab becomes. A terminal, a web page,
//! or any coding agent deck knows, as a chat or in its own terminal. Enter
//! takes the highlighted card, so ⌘N ⏎ is a shell too.

use crate::icon::{icon, Icon};
use crate::theme::{self, u, OneLine};
use crate::workspace::Agent;
use gpui::prelude::*;
use gpui::{
    div, relative, rgb, rgba, AnyElement, Context, ElementId, EventEmitter, FocusHandle,
    FontWeight, KeyDownEvent, SharedString, Window,
};
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub enum Pick {
    Terminal,
    Browser,
    /// deck's own chat with this agent.
    Chat(&'static Agent),
    /// The agent's own interface, in a terminal.
    Tui(&'static Agent),
}

pub enum LauncherEvent {
    Pick(Pick),
    Cancel,
}

pub struct Choice {
    pub pick: Pick,
    pub label: SharedString,
    pub detail: SharedString,
    pub glyph: Icon,
    /// Not installed: shown, so you know it exists, but not offered.
    pub enabled: bool,
    /// A chat agent that can also run in a terminal (⌥⏎, or its button).
    pub tui: Option<&'static Agent>,
}

pub struct LauncherPane {
    pub focus: FocusHandle,
    /// Where whatever is picked starts.
    pub cwd: Option<PathBuf>,
    /// The project that is, for the heading.
    place: Option<String>,
    choices: Vec<Choice>,
    selected: usize,
}

impl EventEmitter<LauncherEvent> for LauncherPane {}

impl LauncherPane {
    pub fn new(
        choices: Vec<Choice>,
        cwd: Option<PathBuf>,
        place: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus: cx.focus_handle(),
            cwd,
            place,
            choices,
            selected: 0,
        }
    }

    fn pick(&mut self, ix: usize, terminal: bool, cx: &mut Context<Self>) {
        let Some(c) = self.choices.get(ix).filter(|c| c.enabled) else {
            return;
        };
        let pick = match (terminal, c.tui) {
            (true, Some(agent)) => Pick::Tui(agent),
            _ => c.pick,
        };
        cx.emit(LauncherEvent::Pick(pick));
    }

    /// The next enabled card in `dir`, wrapping.
    fn step(&self, dir: isize) -> usize {
        let n = self.choices.len() as isize;
        let mut at = self.selected as isize;
        for _ in 0..n {
            at = (at + dir).rem_euclid(n);
            if self.choices[at as usize].enabled {
                break;
            }
        }
        at as usize
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        if m.platform || m.control {
            return;
        }
        match ks.key.as_str() {
            "left" | "up" => self.selected = self.step(-1),
            "right" | "down" | "tab" if !m.shift => self.selected = self.step(1),
            "tab" => self.selected = self.step(-1),
            "enter" => return self.pick(self.selected, m.alt, cx),
            "escape" => return cx.emit(LauncherEvent::Cancel),
            k => match k.parse::<usize>() {
                Ok(n @ 1..=9) => return self.pick(n - 1, m.alt, cx),
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn card(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let c = &self.choices[ix];
        let selected = ix == self.selected && c.enabled;
        let tone = if c.enabled {
            theme::text()
        } else {
            theme::dim()
        };
        let tui = c.tui;
        div()
            .id(ElementId::NamedInteger("choice".into(), ix as u64))
            .group(SharedString::from(format!("choice-{ix}")))
            .relative()
            .flex()
            .flex_col()
            .justify_between()
            .w(u(168.))
            .h(u(92.))
            .p(u(14.))
            .rounded(u(12.))
            .border_1()
            .border_color(rgba(if selected {
                theme::focus_edge()
            } else {
                theme::hairline()
            }))
            .bg(rgb(if selected {
                theme::surface()
            } else {
                theme::raised()
            }))
            .when(c.enabled, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::surface())))
                    .on_click(cx.listener(move |l, _, _, cx| l.pick(ix, false, cx)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(icon(c.glyph, 20., tone))
                    .child(
                        div()
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .when(ix < 9 && c.enabled, |d| d.child(format!("{}", ix + 1))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(1.))
                    .child(
                        div()
                            .one_line()
                            .text_size(u(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(tone))
                            .child(c.label.clone()),
                    )
                    .child(
                        div()
                            .one_line()
                            .text_size(u(11.))
                            .text_color(rgb(theme::dim()))
                            .child(c.detail.clone()),
                    ),
            )
            // A chat agent can also start in its own terminal.
            .when_some(tui.filter(|_| c.enabled), |d, _| {
                d.child(
                    div()
                        .id(ElementId::NamedInteger("choice-tui".into(), ix as u64))
                        .absolute()
                        .right(u(6.))
                        .bottom(u(6.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(u(24.))
                        .rounded(u(6.))
                        .opacity(0.)
                        .group_hover(SharedString::from(format!("choice-{ix}")), |s| {
                            s.opacity(1.)
                        })
                        .hover(|s| s.bg(rgb(theme::surface_hover())))
                        .child(icon(Icon::Terminal, 13., theme::text_2()))
                        .on_click(cx.listener(move |l, _, _, cx| {
                            cx.stop_propagation();
                            l.pick(ix, true, cx);
                        })),
                )
            })
            .into_any_element()
    }
}

impl Render for LauncherPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| {
            div()
                .pl(u(2.))
                .pb(u(10.))
                .text_size(u(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::dim()))
                .child(text)
        };
        let base: Vec<usize> = (0..self.choices.len())
            .filter(|&i| matches!(self.choices[i].pick, Pick::Terminal | Pick::Browser))
            .collect();
        let agents: Vec<usize> = (0..self.choices.len())
            .filter(|i| !base.contains(i))
            .collect();
        let mut open = div().flex().flex_wrap().gap(u(8.));
        for &i in &base {
            open = open.child(self.card(i, cx));
        }
        let mut team = div().flex().flex_wrap().gap(u(8.));
        for &i in &agents {
            team = team.child(self.card(i, cx));
        }

        div()
            .id("launcher")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::pane()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(26.))
                    .w(u(536.))
                    .max_w(relative(0.92))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(u(4.))
                            .child(
                                div()
                                    .text_size(u(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(theme::text()))
                                    .child("New tab"),
                            )
                            .children(self.place.as_ref().map(|p| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(u(6.))
                                    .text_size(u(12.))
                                    .text_color(rgb(theme::dim()))
                                    .child(icon(Icon::Folder, 12., theme::dim()))
                                    .child(p.clone())
                            })),
                    )
                    .child(div().child(label("Open")).child(open))
                    .when(!agents.is_empty(), |d| {
                        d.child(div().child(label("Agents")).child(team))
                    })
                    .child(div().text_size(u(11.)).text_color(rgb(theme::dim())).child(
                        crate::kbd::hints("↵ open · 1–9 pick · ⌥↵ agent in a terminal · esc close"),
                    )),
            )
    }
}
