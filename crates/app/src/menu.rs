//! Right-click menus, drawn by gpui in the app's own look rather than as an
//! NSMenu, so they sit with the rest of the chrome.
//!
//! The view that opens one keeps the [`Open`] it gets back and renders it;
//! the menu is deferred, so it paints above every pane, and anchored, so it
//! stays inside the window. Pages in a browser pane are native views that
//! nothing gpui draws can cover, and keep WebKit's own menu.

use crate::icon::{icon, Icon};
use crate::theme::{self, u};
use gpui::prelude::*;
use gpui::{
    anchored, deferred, div, px, rgb, rgba, Action,
    App, Context, ElementId, Entity, EventEmitter, FocusHandle,
    KeyDownEvent, Pixels, Point, SharedString, Subscription, Window,
};
use std::rc::Rc;

/// What an item does when picked.
type Run = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct Item {
    label: SharedString,
    icon: Option<Icon>,
    shortcut: Option<SharedString>,
    disabled: bool,
    /// Drawn in red: it takes something away (Lock, Delete).
    danger: bool,
    action: Run,
}

impl Item {
    pub fn new(
        label: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            icon: None,
            shortcut: None,
            disabled: false,
            danger: false,
            action: Rc::new(action),
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// The key that does the same thing, shown as a hint ("⌘C").
    pub fn shortcut(mut self, keys: impl Into<SharedString>) -> Self {
        self.shortcut = Some(keys.into()).filter(|k: &SharedString| !k.is_empty());
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

pub enum Entry {
    Item(Item),
    Separator,
}

impl From<Item> for Entry {
    fn from(item: Item) -> Self {
        Entry::Item(item)
    }
}

/// An item action that sends `action` to whatever rendered `focus`, so it
/// reaches that pane's handlers and then the workspace's, exactly as its
/// shortcut would.
pub fn send<A: Action + Clone>(focus: FocusHandle, action: A) -> impl Fn(&mut Window, &mut App) {
    move |window, cx| focus.dispatch_action(&action, window, cx)
}

/// A menu is never narrower than this.
pub const MIN_W: f32 = 208.;

/// The menu closed: an item ran, Esc, or a click landed elsewhere.
pub struct Dismissed;

pub struct ContextMenu {
    entries: Vec<Entry>,
    /// The row the keyboard or the pointer is on.
    highlight: Option<usize>,
    focus: FocusHandle,
    /// Where the keyboard goes back to when the menu closes.
    restore: Option<FocusHandle>,
}

impl EventEmitter<Dismissed> for ContextMenu {}

/// A menu a view has open, and where.
pub struct Open {
    menu: Entity<ContextMenu>,
    at: Point<Pixels>,
    _dismissed: Subscription,
}

impl Open {
    pub fn render(&self) -> impl IntoElement {
        deferred(
            anchored()
                .position(self.at)
                .snap_to_window_with_margin(px(8.))
                .child(self.menu.clone()),
        )
        .with_priority(1)
    }
}

/// Open a menu at `at` (window coordinates). The caller keeps what this
/// returns and renders it; `on_dismiss` must drop it again.
pub fn open<V: 'static>(
    entries: Vec<Entry>,
    at: Point<Pixels>,
    restore: Option<FocusHandle>,
    window: &mut Window,
    cx: &mut Context<V>,
    on_dismiss: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> Open {
    let menu = cx.new(|cx| ContextMenu {
        entries,
        highlight: None,
        focus: cx.focus_handle(),
        restore,
    });
    let focus = menu.read(cx).focus.clone();
    // Once this click has been handled: the workspace focuses the clicked
    // pane further up, and would take the keyboard straight back.
    window.defer(cx, move |window, _| {
        window.focus(&focus);
        crate::browser::reclaim_keyboard(window);
    });
    let sub = cx.subscribe_in(&menu, window, move |v, _, _: &Dismissed, window, cx| {
        on_dismiss(v, window, cx);
        cx.notify();
    });
    Open {
        menu,
        at,
        _dismissed: sub,
    }
}

/// The next enabled row from `from`, wrapping round; `None` when no row is
/// enabled. With nothing highlighted, ↓ starts at the top and ↑ at the bottom.
fn step(enabled: &[bool], from: Option<usize>, down: bool) -> Option<usize> {
    let n = enabled.len();
    if n == 0 {
        return None;
    }
    let start = match (from, down) {
        (Some(i), true) => i + 1,
        (Some(i), false) => i + n - 1,
        (None, true) => 0,
        (None, false) => n - 1,
    };
    (0..n)
        .map(|k| {
            if down {
                (start + k) % n
            } else {
                (start + n - k) % n
            }
        })
        .find(|&i| enabled[i])
}

impl ContextMenu {
    fn enabled(&self) -> Vec<bool> {
        self.entries
            .iter()
            .map(|e| matches!(e, Entry::Item(i) if !i.disabled))
            .collect()
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Only if the keyboard is still here: a click that closed the menu
        // may have focused what it landed on.
        if self.focus.is_focused(window) {
            match &self.restore {
                Some(f) => window.focus(f),
                None => window.blur(),
            }
        }
        cx.emit(Dismissed);
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Entry::Item(item)) = self.entries.get(ix) else {
            return;
        };
        if item.disabled {
            return;
        }
        let action = item.action.clone();
        // Focus goes back first, so the action lands where it was meant for.
        self.dismiss(window, cx);
        action(window, cx);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Every key stops here: Backspace or an arrow must not reach the
        // field or terminal underneath while its menu is open.
        cx.stop_propagation();
        match ev.keystroke.key.as_str() {
            "up" | "down" => {
                self.highlight = step(&self.enabled(), self.highlight, ev.keystroke.key == "down");
                cx.notify();
            }
            "enter" | "space" => {
                if let Some(ix) = self.highlight {
                    self.activate(ix, window, cx);
                }
            }
            "escape" => self.dismiss(window, cx),
            _ => {}
        }
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Labels line up whether or not a row has an icon.
        let gutter = self
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Item(i) if i.icon.is_some()));
        let mut col = div()
            .id("context-menu")
            .track_focus(&self.focus)
            .key_context("ContextMenu")
            .occlude()
            .flex()
            .flex_col()
            .min_w(u(MIN_W))
            .p(u(4.))
            .rounded(u(10.))
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .font_family(theme::ui())
            .text_size(u(13.))
            .line_height(u(18.))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down_out(cx.listener(|m, _, window, cx| m.dismiss(window, cx)));

        for (ix, entry) in self.entries.iter().enumerate() {
            col = col.child(match entry {
                Entry::Separator => div()
                    .h(u(1.))
                    .my(u(4.))
                    .mx(u(4.))
                    .bg(rgba(theme::hairline()))
                    .into_any_element(),
                Entry::Item(item) => {
                    let lit = self.highlight == Some(ix) && !item.disabled;
                    let fg = if item.disabled {
                        theme::dim()
                    } else if item.danger {
                        theme::DANGER
                    } else {
                        theme::text()
                    };
                    let glyph = if item.disabled {
                        theme::dim()
                    } else if item.danger {
                        theme::DANGER
                    } else {
                        theme::text_2()
                    };
                    div()
                        .id(ElementId::NamedInteger("menu-item".into(), ix as u64))
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .h(u(28.))
                        .px(u(8.))
                        .rounded(u(6.))
                        .text_color(rgb(fg))
                        .when(lit, |d| d.bg(rgb(theme::surface_hover())))
                        .when(gutter, |d| match item.icon {
                            // An agent's mark in its own colour, unless the
                            // row is greyed out.
                            Some(i) => d.child(icon(i, 14., i.brand().filter(|_| glyph != theme::dim()).unwrap_or(glyph))),
                            None => d.child(div().flex_none().size(u(14.))),
                        })
                        .child(div().flex_1().whitespace_nowrap().child(item.label.clone()))
                        .when_some(item.shortcut.clone(), |d, keys| {
                            // A chord as a keycap; a word ("chat", "not
                            // installed") as the quiet text it is.
                            d.child(div().pl(u(20.)).child(if crate::kbd::is_chord(&keys) {
                                crate::kbd::key(keys)
                            } else {
                                div()
                                    .text_size(u(12.))
                                    .text_color(rgb(theme::dim()))
                                    .child(keys)
                            }))
                        })
                        .when(!item.disabled, |d| {
                            d.cursor_pointer()
                                .on_hover(cx.listener(move |m, hovered: &bool, _, cx| {
                                    if *hovered {
                                        m.highlight = Some(ix);
                                    } else if m.highlight == Some(ix) {
                                        m.highlight = None;
                                    }
                                    cx.notify();
                                }))
                                .on_click(
                                    cx.listener(move |m, _, window, cx| m.activate(ix, window, cx)),
                                )
                        })
                        .into_any_element()
                }
            });
        }
        // Context menus appear at once: they open many times a day.
        col
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_skip_separators_and_disabled_rows_and_wrap() {
        // Copy (disabled) · Paste · ── · Select All
        let enabled = [false, true, false, true];
        assert_eq!(step(&enabled, None, true), Some(1));
        assert_eq!(step(&enabled, None, false), Some(3));
        assert_eq!(step(&enabled, Some(1), true), Some(3));
        assert_eq!(step(&enabled, Some(3), true), Some(1));
        assert_eq!(step(&enabled, Some(1), false), Some(3));
        assert_eq!(step(&enabled, Some(3), false), Some(1));
    }

    #[test]
    fn nothing_to_highlight_in_an_all_disabled_menu() {
        assert_eq!(step(&[false, false], None, true), None);
        assert_eq!(step(&[false, false], Some(0), false), None);
        assert_eq!(step(&[], None, true), None);
        assert_eq!(step(&[true], Some(0), true), Some(0));
    }
}
