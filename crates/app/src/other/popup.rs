//! Popups off macOS: not made yet. Every window a page asks for opens as a
//! Deck tab, so a sign-in that needs `window.opener` falls back to a plain
//! redirect where the site offers one.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ask;

pub fn wants(_url: &str, _ask: Ask) -> bool {
    false
}

pub fn asked(_features: &wry::NewWindowFeatures) -> Ask {
    Ask
}

pub fn open(
    _url: &str,
    _features: wry::NewWindowFeatures,
    _setup: impl for<'a> FnOnce(wry::WebViewBuilder<'a>) -> wry::WebViewBuilder<'a>,
    _committed: impl Fn(&str) + 'static,
    _ready: fn(&wry::WebView),
) -> Option<wry::NewWindowResponse> {
    None
}

pub fn close_all(_owner: usize) {}
