//! Ad blocking off macOS: WebKit's content rule lists are Apple's; WebKitGTK
//! and WebView2 each have their own filter API, not wired up yet.

pub fn set_enabled(_on: bool) {}

pub fn prepare() {}
