//! The background service off macOS: no menu bar icon yet (Linux needs
//! StatusNotifierItem, Windows a tray icon), but a job that fails still
//! plays its sound and posts a notification that opens it in Deck.

use std::sync::atomic::{AtomicBool, Ordering};

static DIRTY: AtomicBool = AtomicBool::new(true);

/// Jobs told about in a notification, by the number its id carries.
static TOLD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A job ended: one that failed on its own (not a tab, not stopped on
/// purpose) plays the "Job failed" sound and posts a notification that
/// opens it in Deck.
pub fn job_ended(name: &str, code: i32, quiet: bool) {
    DIRTY.store(true, Ordering::Relaxed);
    if code == 0 || quiet {
        return;
    }
    let settings = crate::settings::load();
    crate::notify::play(&settings.sound_failed, settings.sound_volume);
    if !settings.notify_attention {
        return;
    }
    let n = {
        let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
        told.push(name.to_string());
        told.len() as u64 - 1
    };
    let tail = crate::daemon::tail_text(name, 3);
    let body = match tail.trim() {
        "" => format!("Exited with code {code}"),
        t => format!("Exited with code {code}: {}", t.lines().last().unwrap_or("").trim()),
    };
    crate::notify::post_kind(n, &format!("{name} failed"), &body, crate::notify::Kind::Info);
}

/// Hold the main thread for the life of the service, opening a job in
/// Deck when its notification is clicked.
pub fn run() {
    use futures::StreamExt;
    let Some(mut rx) = crate::notify::start() else {
        loop {
            std::thread::park();
        }
    };
    futures::executor::block_on(async move {
        while let Some(ev) = rx.next().await {
            if let crate::notify::Event::Open(n) = ev {
                let name = TOLD.lock().ok().and_then(|t| t.get(n as usize).cloned());
                if let Some(name) = name {
                    crate::desktop::open(&format!("deck://job/{}", crate::browser::encode_query(&name)));
                }
            }
        }
    });
    loop {
        std::thread::park();
    }
}
