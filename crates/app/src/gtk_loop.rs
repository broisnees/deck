//! Pages on Linux are WebKitGTK views, so GTK needs initialising on the
//! main thread and its events handling alongside gpui's. GTK is held to its
//! X11 backend: wry embeds each page as an X11 child of Deck's window.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How often GTK's queue is drained while a page is open: well under a
/// frame, so pages scroll and type without lag.
const TICK: Duration = Duration::from_millis(8);
/// With no page open GTK has next to nothing to do; waking 125 times a
/// second for it kept an idle Deck at several percent of a core.
const IDLE_TICK: Duration = Duration::from_millis(100);

/// Pages alive now (see `BrowserPane::build` and its `Drop`).
pub static PAGES: AtomicUsize = AtomicUsize::new(0);

/// Start GTK and keep its loop turning for the life of the app. Main
/// thread, before the first page is made.
pub fn start(cx: &mut gpui::App) {
    gtk::gdk::set_allowed_backends("x11");
    if let Err(e) = gtk::init() {
        tracing::warn!("pages are off: GTK did not start ({e})");
        return;
    }
    cx.spawn(async move |cx| loop {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        let tick = if PAGES.load(Ordering::Relaxed) > 0 { TICK } else { IDLE_TICK };
        cx.background_executor().timer(tick).await;
    })
    .detach();
}
