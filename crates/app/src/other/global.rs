//! System-wide shortcuts off macOS: not registered yet. Linux needs X11
//! grabs or the GlobalShortcuts portal on Wayland, Windows RegisterHotKey.
//! Deck's in-window shortcuts are unaffected.

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use std::sync::Mutex;

static TX: Mutex<Option<UnboundedSender<&'static str>>> = Mutex::new(None);

/// Where pressed shortcuts arrive, by shortcut id. Once, at launch.
pub fn start() -> UnboundedReceiver<&'static str> {
    let (tx, rx) = unbounded();
    if let Ok(mut t) = TX.lock() {
        *t = Some(tx);
    }
    rx
}

pub fn register(keys: &[(&'static str, String)]) {
    if !keys.is_empty() {
        tracing::info!("system-wide shortcuts are not supported on this platform yet");
    }
}
