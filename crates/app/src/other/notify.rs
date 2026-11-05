//! Notifications off macOS. On Linux they go through `notify-send`, whose
//! actions (`--action`, libnotify 0.7.9 and later) carry Approve, Always
//! and Deny; the desktop's notification server draws them. Replying from
//! the banner needs the D-Bus interface directly and is not done yet.
//! Windows posts nothing yet.

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use std::sync::Mutex;

/// What the user did with a notification.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub enum Event {
    /// Clicked it: bring its pane forward.
    Open(u64),
    /// Approve the agent's request from the banner.
    Allow(u64),
    /// Approve it, and the same again in that chat without asking.
    AllowAlways(u64),
    Deny(u64),
    /// Typed an answer in the banner.
    Reply(u64, String),
}

/// What a notification is about, which sets its buttons.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum Kind {
    Info,
    Permission,
    Question,
}

static CLICKS: Mutex<Option<UnboundedSender<Event>>> = Mutex::new(None);

pub fn start() -> Option<UnboundedReceiver<Event>> {
    let (tx, rx) = unbounded();
    if let Ok(mut c) = CLICKS.lock() {
        *c = Some(tx);
    }
    Some(rx)
}

/// The freedesktop sound theme has no names like macOS's, so every sound
/// but None plays the theme's message sound.
pub fn play(name: &str, volume: f32) {
    if name.is_empty() || name == "None" {
        return;
    }
    let _ = volume;
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("canberra-gtk-play")
            .args(["--id", "message-new-instant"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

pub const SOUNDS: &[&str] = &[
    "None", "Default", "Glass", "Ping", "Pop", "Purr", "Hero", "Funk", "Submarine", "Blow", "Bottle", "Frog", "Morse", "Sosumi", "Basso",
];

#[allow(dead_code)]
pub fn post(pane: u64, title: &str, body: &str) {
    post_kind(pane, title, body, Kind::Info);
}

pub fn post_kind(pane: u64, title: &str, body: &str, kind: Kind) {
    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("notify-send");
        cmd.args(["--app-name", "Deck", "--wait", "--action", "default=Open"]);
        if kind == Kind::Permission {
            cmd.args(["--action", "allow=Approve", "--action", "always=Always Allow", "--action", "deny=Deny"]);
        }
        cmd.arg("--").arg(title).arg(body);
        cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        std::thread::spawn(move || {
            let Ok(out) = cmd.output() else { return };
            let event = match String::from_utf8_lossy(&out.stdout).trim() {
                "default" => Event::Open(pane),
                "allow" => Event::Allow(pane),
                "always" => Event::AllowAlways(pane),
                "deny" => Event::Deny(pane),
                _ => return,
            };
            if let Some(tx) = CLICKS.lock().ok().and_then(|c| c.clone()) {
                let _ = tx.unbounded_send(event);
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pane, title, body, kind);
    }
}
