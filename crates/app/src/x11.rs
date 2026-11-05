//! Deck's own X11 calls on Linux, where pages are WebKitGTK windows inside
//! Deck's window: giving the keyboard back to Deck, and rounding a page's
//! corners to its card's. One Xlib connection for the life of the app, used
//! from the main thread only.

use std::sync::OnceLock;
use x11_dl::{xfixes, xlib};

struct X {
    xlib: xlib::Xlib,
    xfixes: Option<xfixes::Xlib>,
    /// The `Display`, as an address so it can sit in a static.
    display: usize,
}

fn x() -> Option<&'static X> {
    static X: OnceLock<Option<X>> = OnceLock::new();
    X.get_or_init(|| {
        let xlib = xlib::Xlib::open().ok()?;
        // SAFETY: opens the default display; null is checked.
        let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        (!display.is_null()).then(|| X {
            xlib,
            xfixes: xfixes::Xlib::open().ok(),
            display: display as usize,
        })
    })
    .as_ref()
}

/// Give the keyboard to `window` (Deck's own window).
pub fn focus(window: u64) {
    let Some(x) = x() else { return };
    let display = x.display as *mut xlib::Display;
    // SAFETY: a live connection and a window id from gpui.
    unsafe {
        (x.xlib.XSetInputFocus)(display, window, xlib::RevertToParent, xlib::CurrentTime);
        (x.xlib.XFlush)(display);
    }
}

/// Round the page's bottom corners (and its top ones when `top`) by
/// `radius` device pixels, so it sits inside its card's curve. The shape
/// follows the page's window as the server has it: GTK resizes it on its
/// own connection, so that is flushed first, or the shape kept an older,
/// smaller size and cut the page short.
pub fn round_page(webview: &wry::WebView, radius: u32, top: bool) {
    use gtk::prelude::*;
    use wry::WebViewExtUnix;
    let Some(x) = x() else { return };
    let Some(fixes) = &x.xfixes else { return };
    // wry's container: the parent of the GTK window holding the page.
    let Some(gdk) = webview.webview().toplevel().and_then(|t| t.window()) else {
        return;
    };
    let Ok(gdk) = gdk.downcast::<gdkx11::X11Window>() else {
        return;
    };
    let display = x.display as *mut xlib::Display;
    let Some(container) = parent(x, gdk.xid()) else { return };
    if let Some(d) = gtk::gdk::Display::default() {
        d.flush();
    }
    let (mut root, mut gx, mut gy, mut width, mut height, mut border, mut depth) = (0, 0, 0, 0, 0, 0, 0);
    // SAFETY: out-pointers we own, a window that exists.
    let ok = unsafe {
        (x.xlib.XSync)(display, 0);
        (x.xlib.XGetGeometry)(display, container, &mut root, &mut gx, &mut gy, &mut width, &mut height, &mut border, &mut depth)
    };
    if ok == 0 || width == 0 || height == 0 {
        return;
    }
    let rects = corner_rects(width, height, radius, top);
    let mut rects: Vec<xlib::XRectangle> = rects
        .into_iter()
        .map(|(x, y, w, h)| xlib::XRectangle { x: x as i16, y: y as i16, width: w as u16, height: h as u16 })
        .collect();
    // SAFETY: a live connection, a window that exists, and rectangles that
    // outlive the call; the region is freed after use.
    unsafe {
        let region = (fixes.XFixesCreateRegion)(display, rects.as_mut_ptr(), rects.len() as i32);
        // ShapeBounding is 0.
        (fixes.XFixesSetWindowShapeRegion)(display, container, 0, 0, 0, region);
        (fixes.XFixesDestroyRegion)(display, region);
        (x.xlib.XFlush)(display);
    }
}

fn parent(x: &X, window: u64) -> Option<u64> {
    let display = x.display as *mut xlib::Display;
    let (mut root, mut parent) = (0, 0);
    let mut children: *mut u64 = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: out-pointers we own; the child list is freed.
    let ok = unsafe { (x.xlib.XQueryTree)(display, window, &mut root, &mut parent, &mut children, &mut n) };
    if !children.is_null() {
        // SAFETY: allocated by Xlib above.
        unsafe { (x.xlib.XFree)(children.cast()) };
    }
    (ok != 0 && parent != 0 && parent != root).then_some(parent)
}

/// The shape of a `width` × `height` box with rounded corners, as rows:
/// one rectangle for the straight middle, one per pixel row in each curve.
fn corner_rects(width: u32, height: u32, radius: u32, top: bool) -> Vec<(u32, u32, u32, u32)> {
    let r = radius.min(width / 2).min(height / 2);
    let inset = |row: u32| -> u32 {
        // How far in the curve is on pixel row `row` of the corner (0 at
        // its flat edge), measured at the row's middle.
        let dy = r as f32 - row as f32 - 0.5;
        (r as f32 - (r as f32 * r as f32 - dy * dy).max(0.).sqrt()).round() as u32
    };
    let mut out = Vec::new();
    let top_r = if top { r } else { 0 };
    for row in 0..top_r {
        let i = inset(row);
        out.push((i, row, width - 2 * i, 1));
    }
    out.push((0, top_r, width, height - top_r - r));
    for row in 0..r {
        let i = inset(row);
        out.push((i, height - 1 - row, width - 2 * i, 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_curve_in_and_cover_the_box() {
        let rows = corner_rects(100, 50, 10, false);
        // The middle is full width down to the curve.
        assert_eq!(rows[0], (0, 0, 100, 40));
        // The last row is the most inset, the curve's first row near flat.
        let last = rows.iter().max_by_key(|r| r.1).unwrap();
        assert_eq!(last.1, 49);
        assert!(last.0 >= 5);
        assert!(rows.iter().all(|r| r.0 + r.2 <= 100));
        assert_eq!(corner_rects(100, 50, 10, true)[0].1, 0);
    }
}
