//! Settings › Permissions: the macOS permissions only you can switch on,
//! whether Deck has them, and a click to the pane in System Settings; then
//! the ones macOS asks about by itself, for reference.

use super::{button, group, row, Page, Section, SettingsPane};
use crate::privacy::{Permission, State};
use crate::theme::{self, u};
use gpui::prelude::*;
use gpui::{div, rgb, rgba, AnyElement, Context, ElementId};
use std::time::Duration;

impl SettingsPane {
    pub(super) fn page_permissions(&self, cx: &mut Context<Self>) -> Vec<Section> {
        let mut manual: Vec<_> = Permission::MANUAL
            .iter()
            .enumerate()
            .map(|(ix, p)| row(p.name(), self.permission_control(ix, *p, cx)).about(p.why()))
            .collect();
        manual.insert(
            1,
            row("Full Disk Access for the background service", self.service_control(cx)).about(
                "Terminals kept open across restarts and Deck's agents run under deckd, which macOS counts as its own program. Without this, they cannot read Downloads, Desktop or Documents (an SSH key in Downloads fails with \"Operation not permitted\").",
            ),
        );
        let asked = Permission::ASKED
            .iter()
            .enumerate()
            .map(|(ix, p)| row(p.name(), self.permission_control(10 + ix, *p, cx)).about(p.why()))
            .collect();
        vec![
            group(
                "Turn on in System Settings",
                manual,
                Some(
                    "macOS lets no app turn these on by itself. Quit and reopen Deck after switching one on if it does not take; for the background service, reopen the terminals that need it."
                        .into(),
                ),
            ),
            group("Asked when needed", asked, None),
        ]
    }

    /// The service's Full Disk Access: whether it has it, and Open, which
    /// shows the pane and the service's program in Finder to drag in.
    fn service_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let _ = cx;
        let state = crate::privacy::service_full_disk();
        let mut control = div().flex().items_center().gap(u(12.));
        if state != State::Unknown {
            let on = state == State::On;
            let dot = div().flex_none().size(u(7.)).rounded_full();
            let dot = if on { dot.bg(rgb(theme::palette().ansi[2])) } else { dot.border_1().border_color(rgba(theme::input_edge())) };
            control = control.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text_2()))
                    .child(dot)
                    .child(if on { "On" } else { "Off" }),
            );
        }
        control
            .child(button("perm-service", "Open").on_click(|_, _, _| crate::privacy::grant_service()))
            .into_any_element()
    }

    /// Whether it is on, where macOS says, and the way to change it.
    fn permission_control(&self, ix: usize, p: Permission, cx: &mut Context<Self>) -> AnyElement {
        let state = p.state();
        let (text, color, filled) = match state {
            State::On => ("On", theme::palette().ansi[2], true),
            State::Off => ("Off", theme::dim(), false),
            State::Unknown => ("", theme::dim(), false),
        };
        let dot = div().flex_none().size(u(7.)).rounded_full();
        let dot = if filled {
            dot.bg(rgb(color))
        } else {
            dot.border_1().border_color(rgba(theme::input_edge()))
        };
        let mut control = div().flex().items_center().gap(u(12.));
        if state != State::Unknown {
            control = control.child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(12.))
                    .text_color(rgb(theme::text_2()))
                    .child(dot)
                    .child(text),
            );
        }
        if state == State::Off && p.askable() {
            control = control.child(
                button(ElementId::NamedInteger("perm-ask".into(), ix as u64), "Ask")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        p.ask();
                        cx.notify();
                    })),
            );
        }
        control
            .child(
                button(ElementId::NamedInteger("perm-open".into(), ix as u64), "Open")
                    .on_click(move |_, _, _| p.open()),
            )
            .into_any_element()
    }

    /// Look again every two seconds while the page is up, so switching one
    /// on in System Settings shows here without a click.
    pub(super) fn watch_permissions(&mut self, cx: &mut Context<Self>) {
        if self.perms_polling {
            return;
        }
        self.perms_polling = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let again = this.update(cx, |p, cx| {
                let on = p.page == Page::Permissions;
                p.perms_polling = on;
                if on {
                    cx.notify();
                }
                on
            });
            if !again.unwrap_or(false) {
                return;
            }
        })
        .detach();
    }
}
