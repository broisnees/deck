//! The Usage tab: what the coding agents cost and used over a range of days,
//! read from their own records. A headline figure with each agent's share, a
//! chart by day, the totals (tokens processed, cached input, output, what the
//! cache saved) and a breakdown by model, day or project. The range exports
//! as CSV, one row per day, agent and model.
//!
//! Costs are at API list prices: what the tokens would have cost billed by
//! the token, not what a plan charges.

use crate::icon::{icon, Icon};
use crate::theme::{self, u, OneLine};
use crate::usage::{self, Report};
use gpui::prelude::*;
use gpui::{div, px, relative, rgb, rgba, AnyElement, Context, ElementId, FontWeight, SharedString, Window};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Metric {
    Cost,
    Tokens,
}

#[derive(Clone, Copy, PartialEq)]
enum Split {
    Model,
    Day,
    Project,
}

/// Where the CSV export stands.
#[derive(Clone, Copy, PartialEq)]
enum Export {
    Idle,
    Saving,
    Saved,
    Failed,
}

const RANGES: [(u32, &str); 4] = [(1, "Past 24h"), (7, "7 days"), (30, "30 days"), (90, "90 days")];

pub struct UsagePage {
    /// Keys reach Deck's shortcuts while this page is in front.
    pub focus: gpui::FocusHandle,
    days: u32,
    metric: Metric,
    split: Split,
    report: Option<Report>,
    loading: bool,
    /// The day under the pointer in the chart.
    hover: Option<i64>,
    export: Export,
}

impl UsagePage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut p = Self {
            focus: cx.focus_handle(),
            days: 30,
            metric: Metric::Cost,
            split: Split::Model,
            report: None,
            loading: false,
            hover: None,
            export: Export::Idle,
        };
        p.load(cx);
        p
    }

    /// Read the range again, off the main thread.
    fn load(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let days = self.days;
        let task = cx.background_executor().spawn(async move { usage::report(days) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |p, cx| {
                p.loading = false;
                // A range picked while this one read: read that one.
                if r.days == p.days {
                    p.report = Some(r);
                } else {
                    p.load(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_days(&mut self, days: u32, cx: &mut Context<Self>) {
        if days != self.days {
            self.days = days;
            self.load(cx);
        }
    }

    /// Writes the report to Downloads off the main thread, shows it in
    /// Finder, and says so on the button for two seconds.
    fn export_csv(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.report.clone() else { return };
        if self.export == Export::Saving {
            return;
        }
        self.export = Export::Saving;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let path = usage::export_csv(&r)?;
            std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn()?;
            std::io::Result::Ok(())
        });
        cx.spawn(async move |this, cx| {
            let done = if task.await.is_ok() { Export::Saved } else { Export::Failed };
            let _ = this.update(cx, |p, cx| {
                p.export = done;
                cx.notify();
            });
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |p, cx| {
                if p.export == done {
                    p.export = Export::Idle;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn figure(&self, cost: f64, tokens: u64) -> String {
        match self.metric {
            Metric::Cost => money(cost),
            Metric::Tokens => count(tokens),
        }
    }

    fn render_chart(&self, r: &Report, cx: &mut Context<Self>) -> AnyElement {
        let value = |d: &usage::DayTotal| -> f64 {
            match self.metric {
                Metric::Cost => d.by_agent.iter().map(|a| a.1).sum(),
                Metric::Tokens => d.by_agent.iter().map(|a| a.2 as f64).sum(),
            }
        };
        let top = r.daily.iter().map(value).fold(0., f64::max);
        let scale = nice_max(top);
        let height = 180.;
        let mut bars = div().flex().items_end().gap(u(if r.daily.len() > 40 { 1. } else { 3. })).h(u(height)).flex_1();
        for d in &r.daily {
            let day = d.day;
            let lit = self.hover == Some(day);
            let mut col = div()
                .id(ElementId::NamedInteger("usage-day".into(), (day + 1000) as u64))
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .justify_end()
                .rounded_t(u(2.))
                .when(lit, |c| c.bg(rgba(theme::tint())))
                .on_hover(cx.listener(move |p, on: &bool, _, cx| {
                    let next = if *on { Some(day) } else if p.hover == Some(day) { None } else { p.hover };
                    if next != p.hover {
                        p.hover = next;
                        cx.notify();
                    }
                }));
            // Stacked by agent, the busiest at the bottom.
            let mut parts: Vec<(&str, f64)> = d
                .by_agent
                .iter()
                .map(|(a, c, t)| (*a, if self.metric == Metric::Cost { *c } else { *t as f64 }))
                .filter(|(_, v)| *v > 0.)
                .collect();
            parts.sort_by(|a, b| a.1.total_cmp(&b.1));
            for (agent, v) in parts {
                let h = if scale > 0. { (v / scale * height as f64) as f32 } else { 0. };
                col = col.child(
                    div()
                        .w_full()
                        .h(u(h.max(1.5)))
                        .bg(rgb(Icon::agent(agent).brand().unwrap_or(theme::text_2())))
                        .opacity(if self.hover.is_none() || lit { 1. } else { 0.55 }),
                );
            }
            bars = bars.child(col);
        }
        // The gridlines' labels, top to bottom.
        let label = |v: f64| match self.metric {
            Metric::Cost => money(v),
            Metric::Tokens => count(v as u64),
        };
        let axis = div()
            .flex()
            .flex_col()
            .justify_between()
            .h(u(height))
            .w(u(54.))
            .flex_none()
            .text_size(u(10.5))
            .text_color(rgb(theme::dim()))
            .children([scale, scale * 2. / 3., scale / 3., 0.].map(|v| div().child(label(v))));
        let first = r.daily.first().map(|d| d.day).unwrap_or(0);
        let mid = first / 2;
        let when = self.hover.and_then(|h| r.daily.iter().find(|d| d.day == h)).map(|d| {
            let total: f64 = d.by_agent.iter().map(|a| a.1).sum();
            let tokens: u64 = d.by_agent.iter().map(|a| a.2).sum();
            format!("{} · {} · {} tokens", day_label(d.day), money(total), count(tokens))
        });
        div()
            .flex()
            .flex_col()
            .gap(u(8.))
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(match self.metric {
                        Metric::Cost => "Daily cost",
                        Metric::Tokens => "Daily tokens",
                    }))
                    .children(when.map(|w| div().text_size(u(11.5)).text_color(rgb(theme::text_2())).child(w))),
            )
            .child(div().flex().gap(u(8.)).child(axis).child(bars))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .pl(u(62.))
                    .text_size(u(10.5))
                    .text_color(rgb(theme::dim()))
                    .child(day_label(first))
                    .when(first < -2, |d| d.child(day_label(mid)))
                    .child(day_label(0)),
            )
            .into_any_element()
    }

    fn render_breakdown(&self, r: &Report) -> AnyElement {
        let total_cost = r.cost().max(1e-9);
        let total_tokens = r.tokens().max(1) as f64;
        let head = |a: &'static str, b: &'static str, c: &'static str, d: &'static str| {
            div()
                .flex()
                .items_center()
                .h(u(30.))
                .text_size(u(11.5))
                .text_color(rgb(theme::dim()))
                .border_b_1()
                .border_color(rgba(theme::hairline()))
                .child(div().flex_1().child(a))
                .child(div().w(u(100.)).flex().justify_end().child(b))
                .child(div().w(u(80.)).flex().justify_end().child(c))
                .child(div().w(u(90.)).flex().justify_end().child(d))
        };
        let line = |mark: Option<Icon>, name: String, cost: f64, tokens: u64| {
            let share = match self.metric {
                Metric::Cost => cost / total_cost,
                Metric::Tokens => tokens as f64 / total_tokens,
            };
            div()
                .flex()
                .items_center()
                .gap(u(8.))
                .h(u(38.))
                .border_b_1()
                .border_color(rgba(theme::hairline()))
                .children(mark.map(|m| icon(m, 14., m.brand().unwrap_or(theme::text_2()))))
                .child(div().flex_1().min_w_0().one_line().child(name))
                .child(div().w(u(100.)).flex().justify_end().child(money(cost)))
                .child(div().w(u(80.)).flex().justify_end().text_color(rgb(theme::text_2())).child(format!("{:.1}%", share * 100.)))
                .child(div().w(u(90.)).flex().justify_end().text_color(rgb(theme::text_2())).child(count(tokens)))
        };
        let mut table = div().flex().flex_col().text_size(u(13.));
        match self.split {
            Split::Model => {
                table = table.child(head("Model", "Cost", "Share", "Tokens"));
                for m in &r.models {
                    table = table.child(line(Some(Icon::agent(m.agent)), m.model.clone(), m.cost, m.tokens()));
                }
            }
            Split::Day => {
                table = table.child(head("Day", "Cost", "Share", "Tokens"));
                for d in r.daily.iter().rev() {
                    let cost: f64 = d.by_agent.iter().map(|a| a.1).sum();
                    let tokens: u64 = d.by_agent.iter().map(|a| a.2).sum();
                    if tokens > 0 {
                        table = table.child(line(None, day_label(d.day), cost, tokens));
                    }
                }
            }
            Split::Project => {
                table = table.child(head("Project", "Cost", "Share", "Tokens"));
                for p in &r.projects {
                    table = table.child(line(Some(Icon::Folder), p.name.clone(), p.cost, p.tokens));
                }
            }
        }
        if r.models.is_empty() {
            table = table.child(
                div()
                    .py(u(24.))
                    .text_color(rgb(theme::dim()))
                    .child("Nothing in this range."),
            );
        }
        table.into_any_element()
    }
}

impl Render for UsagePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let seg = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(u(24.))
                .px(u(10.))
                .rounded(u(6.))
                .text_size(u(12.5))
                .cursor_pointer()
                .when(on, |d| {
                    d.bg(rgb(theme::row_active()))
                        .border_1()
                        .border_color(rgba(theme::hairline()))
                        .text_color(rgb(theme::text()))
                })
                .when(!on, |d| d.text_color(rgb(theme::text_2())).hover(|s| s.text_color(rgb(theme::text()))))
                .child(label)
        };
        let group = || div().flex().items_center().gap(u(2.)).p(u(2.)).rounded(u(8.)).bg(rgba(theme::tint()));

        let mut ranges = group();
        for (i, (days, label)) in RANGES.iter().copied().enumerate() {
            let id: &'static str = ["r1", "r7", "r30", "r90"][i];
            ranges = ranges.child(seg(id, label, self.days == days).on_click(cx.listener(move |p, _, _, cx| p.set_days(days, cx))));
        }
        let metric = group()
            .child(seg("m-cost", "Cost", self.metric == Metric::Cost).on_click(cx.listener(|p, _, _, cx| {
                p.metric = Metric::Cost;
                cx.notify();
            })))
            .child(seg("m-tokens", "Tokens", self.metric == Metric::Tokens).on_click(cx.listener(|p, _, _, cx| {
                p.metric = Metric::Tokens;
                cx.notify();
            })));
        let refresh = div()
            .id("usage-refresh")
            .flex()
            .items_center()
            .justify_center()
            .size(u(28.))
            .rounded(u(6.))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme::row_hover())))
            .child(if self.loading {
                crate::icon::spinner("usage-spin", 14., theme::text_2())
            } else {
                icon(Icon::Refresh, 14., theme::text_2())
            })
            .on_click(cx.listener(|p, _, _, cx| p.load(cx)));
        let (export_icon, export_label) = match self.export {
            Export::Idle => (Icon::Download, "Export CSV"),
            Export::Saving => (Icon::Download, "Saving…"),
            Export::Saved => (Icon::Check, "Saved to Downloads"),
            Export::Failed => (Icon::Info, "Couldn't save"),
        };
        let can_export = self.report.as_ref().is_some_and(|r| !r.rows.is_empty());
        let export = div()
            .id("usage-export")
            .flex()
            .items_center()
            .gap(u(6.))
            .h(u(28.))
            .px(u(10.))
            .rounded(u(6.))
            .border_1()
            .border_color(rgba(theme::hairline()))
            .text_size(u(12.))
            .text_color(rgb(theme::text_2()))
            .child(icon(export_icon, 13., theme::text_2()))
            .child(export_label)
            .when(can_export, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                    .on_click(cx.listener(|p, _, _, cx| p.export_csv(cx)))
            })
            .when(!can_export, |d| d.opacity(0.5));

        let range_label = RANGES.iter().find(|r| r.0 == self.days).map_or("", |r| r.1);
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(u(10.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(u(8.))
                    .text_size(u(15.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Usage"))
                    .child(div().text_color(rgb(theme::dim())).child("/"))
                    .child(div().text_color(rgb(theme::text_2())).child(range_label)),
            )
            .child(metric)
            .child(ranges)
            .child(export)
            .child(refresh);

        let body: AnyElement = match &self.report {
            None => div()
                .py(u(60.))
                .flex()
                .justify_center()
                .text_color(rgb(theme::dim()))
                .child("Reading your agents' records…")
                .into_any_element(),
            Some(r) => {
                let sessions: u32 = r.sessions.iter().map(|s| s.1).sum();
                let cost = r.cost();
                let tokens = r.tokens();
                let mut agents = div().flex().flex_col().gap(u(14.)).pt(u(18.));
                let mut per: Vec<(&'static str, f64, u64)> = Vec::new();
                for m in &r.models {
                    match per.iter_mut().find(|p| p.0 == m.agent) {
                        Some(p) => {
                            p.1 += m.cost;
                            p.2 += m.tokens();
                        }
                        None => per.push((m.agent, m.cost, m.tokens())),
                    }
                }
                per.sort_by(|a, b| b.1.total_cmp(&a.1));
                for (agent, c, t) in per {
                    let n = r.sessions.iter().find(|s| s.0 == agent).map_or(0, |s| s.1);
                    let mark = Icon::agent(agent);
                    let share = match self.metric {
                        Metric::Cost => c / cost.max(1e-9),
                        Metric::Tokens => t as f64 / tokens.max(1) as f64,
                    };
                    agents = agents.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(u(3.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(u(8.))
                                    .child(div().size(u(7.)).rounded_full().bg(rgb(mark.brand().unwrap_or(theme::text_2()))))
                                    .child(icon(mark, 15., mark.brand().unwrap_or(theme::text_2())))
                                    .child(div().font_weight(FontWeight::MEDIUM).child(agent_name(agent)))
                                    .child(div().flex_1().text_size(u(11.5)).text_color(rgb(theme::dim())).child(format!(
                                        "{n} session{}",
                                        if n == 1 { "" } else { "s" }
                                    )))
                                    .child(div().font_weight(FontWeight::MEDIUM).child(self.figure(c, t))),
                            )
                            .child(div().pl(u(15.)).text_size(u(11.5)).text_color(rgb(theme::dim())).child(format!(
                                "{:.1}% of {} · {} tokens",
                                share * 100.,
                                if self.metric == Metric::Cost { "cost" } else { "tokens" },
                                count(t)
                            ))),
                    );
                }
                let headline = div()
                    .flex()
                    .flex_col()
                    .w(u(300.))
                    .flex_none()
                    .child(
                        div()
                            .text_size(u(40.))
                            .line_height(u(48.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.figure(cost, tokens)),
                    )
                    .child(div().text_size(u(12.5)).text_color(rgb(theme::text_2())).child(format!(
                        "{sessions} sessions · {}",
                        if self.metric == Metric::Cost { "API estimate" } else { "all tokens processed" }
                    )))
                    .child(agents);

                let input: u64 = r.models.iter().map(|m| m.input).sum();
                let output: u64 = r.models.iter().map(|m| m.output).sum();
                let read: u64 = r.models.iter().map(|m| m.cache_read).sum();
                let write: u64 = r.models.iter().map(|m| m.cache_write).sum();
                let saved: f64 = r.models.iter().map(|m| m.saved).sum();
                let stat = |label: &'static str, v: String| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(u(4.))
                        .flex_1()
                        .min_w(u(110.))
                        .child(div().text_size(u(12.)).text_color(rgb(theme::text_2())).child(label))
                        .child(div().text_size(u(20.)).font_weight(FontWeight::MEDIUM).child(v))
                };
                let totals = div()
                    .flex()
                    .flex_col()
                    .gap(u(12.))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Totals"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(u(16.))
                            .child(stat("Processed tokens", count(tokens)))
                            .child(stat("Cached input", count(read)))
                            .child(stat("Uncached input", count(input + write)))
                            .child(stat("Output", count(output)))
                            .child(stat("Cache savings", money(saved))),
                    );
                let split = group()
                    .child(seg("s-model", "Model", self.split == Split::Model).on_click(cx.listener(|p, _, _, cx| {
                        p.split = Split::Model;
                        cx.notify();
                    })))
                    .child(seg("s-day", "Day", self.split == Split::Day).on_click(cx.listener(|p, _, _, cx| {
                        p.split = Split::Day;
                        cx.notify();
                    })))
                    .child(seg("s-project", "Project", self.split == Split::Project).on_click(cx.listener(|p, _, _, cx| {
                        p.split = Split::Project;
                        cx.notify();
                    })));
                div()
                    .flex()
                    .flex_col()
                    .gap(u(36.))
                    .child(div().flex().flex_wrap().gap(u(32.)).child(headline).child(self.render_chart(r, cx)))
                    .child(totals)
                    .children(limit_history())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(u(8.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(div().flex_1().font_weight(FontWeight::MEDIUM).child("Breakdown"))
                                    .child(split),
                            )
                            .child(self.render_breakdown(r)),
                    )
                    .child(
                        div()
                            .text_size(u(11.5))
                            .text_color(rgb(theme::dim()))
                            .child("Read from Claude Code's, Codex's, OpenCode's and Gemini CLI's own records on this Mac. Costs are at API list prices (OpenCode's as it recorded them; Gemini CLI records none), not what a plan bills."),
                    )
                    .into_any_element()
            }
        };

        div()
            .id("usage-page")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::pane()))
            .text_color(rgb(theme::text()))
            .child(
                div()
                    .mx_auto()
                    .max_w(px(1100.))
                    .w(relative(1.))
                    .px(u(32.))
                    .py(u(24.))
                    .flex()
                    .flex_col()
                    .gap(u(24.))
                    .child(header)
                    .child(body),
            )
    }
}

fn agent_name(agent: &str) -> SharedString {
    crate::workspace::AGENTS
        .iter()
        .find(|a| a.cmd == agent)
        .map_or_else(|| agent.to_string().into(), |a| a.name.into())
}

/// Plan limits by day: each window (Claude's 5-hour and weekly, Codex's)
/// as a row of the last 14 days, each bar the highest it reached that day.
fn limit_history() -> Option<AnyElement> {
    let hist = usage::limit_history();
    if hist.is_empty() {
        return None;
    }
    let today = hist.iter().map(|d| d.day).max()?;
    let mut keys: Vec<(String, String)> = Vec::new();
    for d in &hist {
        let k = (d.agent.clone(), d.label.clone());
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let mut col = div()
        .flex()
        .flex_col()
        .gap(u(12.))
        .child(div().font_weight(FontWeight::MEDIUM).child("Plan limits, last 14 days"));
    for (agent, label) in keys {
        let mut bars = div().flex().items_end().gap(u(3.)).h(u(36.)).flex_1();
        for back in (0..14).rev() {
            let day = today - back;
            let peak = hist.iter().find(|d| d.day == day && d.agent == agent && d.label == label).map(|d| d.peak);
            let tone = match peak {
                Some(p) if p >= 90. => theme::DANGER,
                Some(p) if p >= 70. => theme::palette().ansi[3],
                Some(_) => theme::text_2(),
                None => theme::dim(),
            };
            bars = bars.child(
                div()
                    .flex_1()
                    .h(u((peak.unwrap_or(0.) / 100. * 36.).max(2.)))
                    .rounded_t(u(2.))
                    .bg(rgb(tone))
                    .opacity(if peak.is_some() { 1. } else { 0.25 }),
            );
        }
        let last = hist.iter().find(|d| d.day == today && d.agent == agent && d.label == label).map_or(0., |d| d.peak);
        col = col.child(
            div()
                .flex()
                .items_center()
                .gap(u(16.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .w(u(160.))
                        .flex_none()
                        .child(icon(Icon::agent(&agent), 13., Icon::agent(&agent).brand().unwrap_or(theme::text_2())))
                        .child(div().text_size(u(12.5)).child(label.clone())),
                )
                .child(bars)
                .child(div().w(u(48.)).flex().justify_end().text_size(u(12.)).text_color(rgb(theme::text_2())).child(format!("{last:.0}%"))),
        );
    }
    Some(col.into_any_element())
}

fn money(v: f64) -> String {
    if v >= 1000. {
        let whole = v.round() as u64;
        let s = whole.to_string();
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        format!("${out}")
    } else {
        format!("${v:.2}")
    }
}

fn count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.2}B", n as f64 / 1e9),
    }
    .replace(".0K", "K")
    .replace(".0M", "M")
}

/// A round number at or above `v` for the chart's top line.
fn nice_max(v: f64) -> f64 {
    if v <= 0. {
        return 1.;
    }
    let mag = 10f64.powf(v.log10().floor());
    for step in [1., 1.5, 2., 3., 4., 5., 6., 8., 10.] {
        if step * mag >= v {
            return step * mag;
        }
    }
    10. * mag
}

/// "Today", "Yesterday", or "Oct 14" for `day` days from today.
fn day_label(day: i64) -> String {
    match day {
        0 => "Today".into(),
        -1 => "Yesterday".into(),
        _ => {
            let (epoch, (h, m, s)) = crate::retry::now();
            let secs = epoch as i64 - (h * 3600 + m * 60 + s) as i64 + day * 86_400 + 43_200;
            let (_, mo, d) = usage::civil(secs.div_euclid(86_400));
            const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            format!("{} {d}", M[(mo - 1) as usize])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_read_short() {
        assert_eq!(money(302.876), "$302.88");
        assert_eq!(money(1616.81), "$1,617");
        assert_eq!(count(805_000_000), "805M");
        assert_eq!(count(2_310_000), "2.3M");
        assert_eq!(count(103_000), "103K");
        assert_eq!(nice_max(57.), 60.);
        assert_eq!(usage::civil(0), (1970, 1, 1));
        assert_eq!(usage::civil(20_727), (2026, 10, 1));
    }
}
