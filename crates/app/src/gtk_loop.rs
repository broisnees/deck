//! Pages on Linux are WebKitGTK views, so GTK needs initialising on the
//! main thread and its events handling alongside gpui's. GTK is held to its
//! X11 backend: wry embeds each page as an X11 child of Deck's window.

use std::time::Duration;

/// How often GTK's queue is drained: well under a frame, so pages scroll
/// and type without lag.
const TICK: Duration = Duration::from_millis(8);

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
        cx.background_executor().timer(TICK).await;
    })
    .detach();
}
