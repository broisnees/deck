//! The background service's menu bar icon: a terminal mark with how many
//! jobs run, and a menu of every job (its state, how long, its command)
//! with Show in Deck, Restart and Stop; Open Deck, Stop All and Quit.
//!
//! AppKit through plain messages, with one small class as the menu's
//! delegate and its items' target. The menu is rebuilt each time it opens.

use crate::daemon::{self, JobInfo};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{class, define_class, msg_send, sel, AllocAnyThread};
use objc2_foundation::NSString;
use std::sync::atomic::{AtomicBool, Ordering};

/// Something changed that the icon's count should show.
static DIRTY: AtomicBool = AtomicBool::new(true);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DeckdMenuTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &AnyObject) {
            fill(menu, self);
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &AnyObject) {
            refresh_title();
        }

        #[unsafe(method(show:))]
        fn show(&self, item: &AnyObject) {
            if let Some(name) = name_of(item) {
                open_url(&format!("deck://job/{}", crate::browser::encode_query(&name)));
            }
        }

        #[unsafe(method(restart:))]
        fn restart(&self, item: &AnyObject) {
            if let Some(name) = name_of(item) {
                std::thread::spawn(move || daemon::restart_named(&name));
            }
        }

        #[unsafe(method(stop:))]
        fn stop(&self, item: &AnyObject) {
            if let Some(name) = name_of(item) {
                std::thread::spawn(move || {
                    daemon::stop_named(&name);
                    DIRTY.store(true, Ordering::Relaxed);
                });
            }
        }

        #[unsafe(method(stopAll:))]
        fn stop_all(&self, _item: &AnyObject) {
            std::thread::spawn(|| {
                for j in daemon::snapshot() {
                    daemon::stop_named(&j.name);
                }
                DIRTY.store(true, Ordering::Relaxed);
            });
        }

        #[unsafe(method(openDeck:))]
        fn open_deck(&self, _item: &AnyObject) {
            open_url("deck://open");
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _item: &AnyObject) {
            daemon::quit();
        }
    }
);

impl Target {
    fn new() -> Retained<Self> {
        // SAFETY: NSObject's init; the class has no state of its own.
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Jobs told about in a notification, by the number its id carries.
static TOLD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A job ended: the icon's count changes; one that failed on its own (not
/// a tab, not stopped on purpose) plays the "Job failed" sound and posts a
/// notification that opens it in Deck.
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

/// Clicks on the service's notifications open that job in Deck.
pub fn route_clicks() {
    if let Some(mut rx) = crate::notify::start() {
        std::thread::spawn(move || {
            use futures::StreamExt;
            futures::executor::block_on(async move {
                while let Some(ev) = rx.next().await {
                    if let crate::notify::Event::Open(n) = ev {
                        let name = TOLD.lock().ok().and_then(|t| t.get(n as usize).cloned());
                        if let Some(name) = name {
                            open_url(&format!("deck://job/{}", crate::browser::encode_query(&name)));
                        }
                    }
                }
            });
        });
    }
}

thread_local! {
    static ITEM: std::cell::RefCell<Option<Retained<AnyObject>>> = const { std::cell::RefCell::new(None) };
}

/// Run AppKit on this (the main) thread with the status item, until quit.
pub fn run() {
    // SAFETY: AppKit on the main thread, which `deck daemon` runs on.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        // An accessory: a menu bar icon, no Dock icon, no menu bar of its own.
        let _: bool = msg_send![&*app, setActivationPolicy: 1isize];
        let bar: Retained<AnyObject> = msg_send![class!(NSStatusBar), systemStatusBar];
        let item: Retained<AnyObject> = msg_send![&*bar, statusItemWithLength: -1.0f64];
        let target = Target::new();
        let menu: Retained<AnyObject> = msg_send![class!(NSMenu), new];
        let _: () = msg_send![&*menu, setDelegate: &*target];
        let _: () = msg_send![&*menu, setAutoenablesItems: false];
        let _: () = msg_send![&*item, setMenu: &*menu];
        if let Some(button) = button(&item) {
            // A stack of cards: a deck.
            let symbol = NSString::from_str("rectangle.stack.fill");
            let image: Option<Retained<AnyObject>> =
                msg_send![class!(NSImage), imageWithSystemSymbolName: &*symbol, accessibilityDescription: std::ptr::null::<AnyObject>()];
            if let Some(image) = image {
                let _: () = msg_send![&*image, setTemplate: true];
                let _: () = msg_send![&*button, setImage: &*image];
                let _: () = msg_send![&*button, setImagePosition: 2isize]; // image left of the title
            }
        }
        ITEM.with(|i| *i.borrow_mut() = Some(item));
        refresh_title();
        route_clicks();
        let _: Retained<AnyObject> = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: 2.0f64,
            target: &*target,
            selector: sel!(tick:),
            userInfo: std::ptr::null::<AnyObject>(),
            repeats: true
        ];
        // Kept for the life of the process: the menu's delegate is weak.
        std::mem::forget(target);
        std::mem::forget(menu);
        let _: () = msg_send![&*app, run];
    }
}

unsafe fn button(item: &AnyObject) -> Option<Retained<AnyObject>> {
    msg_send![item, button]
}

/// The icon's title: how many jobs run, nothing when none do.
fn refresh_title() {
    let jobs = daemon::snapshot();
    let running = jobs.iter().filter(|j| j.running).count();
    let failed = jobs.iter().any(|j| !j.running && j.exit.is_some_and(|c| c != 0));
    DIRTY.store(false, Ordering::Relaxed);
    let title = match (running, failed) {
        (0, false) => String::new(),
        (0, true) => " !".into(),
        (n, false) => format!(" {n}"),
        (n, true) => format!(" {n} !"),
    };
    ITEM.with(|i| {
        if let Some(item) = i.borrow().as_ref() {
            // SAFETY: main thread; the button belongs to our status item.
            unsafe {
                if let Some(b) = button(item) {
                    let t = NSString::from_str(&title);
                    let _: () = msg_send![&*b, setTitle: &*t];
                    let tip = NSString::from_str(&format!("Deck background: {running} running"));
                    let _: () = msg_send![&*b, setToolTip: &*tip];
                }
            }
        }
    });
}

fn name_of(item: &AnyObject) -> Option<String> {
    // SAFETY: our items carry an NSString as their represented object.
    unsafe {
        let o: Option<Retained<NSString>> = msg_send![item, representedObject];
        o.map(|s| s.to_string())
    }
}

fn open_url(url: &str) {
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
}

/// "3m", "2h 10m", "1d 4h".
fn uptime(started: i64) -> String {
    let s = (crate::local::now() - started).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// A row's title and its subtitle: who and where; state and how long.
fn labels(j: &JobInfo) -> (String, String) {
    let folder = j.cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "~".into());
    let agent = j.agent.as_ref().map(|a| {
        crate::workspace::AGENTS.iter().find(|x| x.cmd == *a).map_or_else(|| a.clone(), |x| x.name.to_string())
    });
    let state = match (j.running, j.exit) {
        (true, _) => format!("Running {}", uptime(j.started)),
        (false, Some(0)) => "Done".to_string(),
        (false, Some(c)) => format!("Exited {c}"),
        (false, None) => "Stopped".to_string(),
    };
    if j.tab {
        (agent.unwrap_or_else(|| "Terminal".into()), format!("{folder} · {state}"))
    } else {
        let what = agent.map(|a| format!(" · {a}")).unwrap_or_default();
        (j.name.clone(), format!("{state}{what} · {folder}"))
    }
}

/// An SF Symbol, tinted (`Some(rgb)`) or as a template for the menu's text
/// colour (`None`).
unsafe fn symbol(name: &str, tint: Option<(f64, f64, f64)>) -> Option<Retained<AnyObject>> {
    let image: Option<Retained<AnyObject>> = msg_send![
        class!(NSImage),
        imageWithSystemSymbolName: &*NSString::from_str(name),
        accessibilityDescription: std::ptr::null::<AnyObject>()
    ];
    let image = image?;
    match tint {
        None => {
            let _: () = msg_send![&*image, setTemplate: true];
            Some(image)
        }
        Some((r, g, b)) => {
            let color: Retained<AnyObject> = msg_send![class!(NSColor), colorWithSRGBRed: r, green: g, blue: b, alpha: 1.0f64];
            let config: Option<Retained<AnyObject>> =
                msg_send![class!(NSImageSymbolConfiguration), configurationWithHierarchicalColor: &*color];
            match config {
                Some(c) => msg_send![&*image, imageWithSymbolConfiguration: &*c],
                None => Some(image),
            }
        }
    }
}

/// Rebuild the menu: terminal tabs, then jobs, each with its state as a
/// coloured mark and a submenu of actions.
fn fill(menu: &AnyObject, target: &Target) {
    // SAFETY: AppKit on the main thread, items made and added here.
    unsafe {
        let _: () = msg_send![menu, removeAllItems];
        let add = |menu: &AnyObject, title: &str, action: Option<Sel>, name: Option<&str>, image: Option<Retained<AnyObject>>| -> Retained<AnyObject> {
            let item: Retained<AnyObject> = msg_send![class!(NSMenuItem), new];
            let _: () = msg_send![&*item, setTitle: &*NSString::from_str(title)];
            if let Some(a) = action {
                let _: () = msg_send![&*item, setAction: a];
                let _: () = msg_send![&*item, setTarget: target];
            }
            if let Some(n) = name {
                let _: () = msg_send![&*item, setRepresentedObject: &*NSString::from_str(n)];
            }
            if let Some(img) = image {
                let _: () = msg_send![&*item, setImage: &*img];
            }
            let _: () = msg_send![&*item, setEnabled: action.is_some() || name.is_some()];
            let _: () = msg_send![menu, addItem: &*item];
            item
        };
        let sep = |menu: &AnyObject| {
            let s: Retained<AnyObject> = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: &*s];
        };
        // A section title: macOS 14's header item, else a quiet disabled row.
        let header = |menu: &AnyObject, title: &str| {
            let t = NSString::from_str(title);
            let has: bool = msg_send![class!(NSMenuItem), respondsToSelector: sel!(sectionHeaderWithTitle:)];
            let item: Retained<AnyObject> = if has {
                msg_send![class!(NSMenuItem), sectionHeaderWithTitle: &*t]
            } else {
                let i: Retained<AnyObject> = msg_send![class!(NSMenuItem), new];
                let _: () = msg_send![&*i, setTitle: &*t];
                let _: () = msg_send![&*i, setEnabled: false];
                i
            };
            let _: () = msg_send![menu, addItem: &*item];
        };
        let jobs = daemon::snapshot();
        let running = jobs.iter().filter(|j| j.running).count();
        let green = Some((0.30, 0.78, 0.45));
        let red = Some((0.93, 0.36, 0.34));
        for (title, tabs) in [("Terminal tabs", true), ("Jobs", false)] {
            let group: Vec<&JobInfo> = jobs.iter().filter(|j| j.tab == tabs).collect();
            if group.is_empty() {
                continue;
            }
            header(menu, title);
            for j in group {
                let (name, sub) = labels(j);
                let mark = match (j.running, j.exit) {
                    (true, _) => symbol("circle.fill", green),
                    (false, Some(c)) if c != 0 => symbol("xmark.circle.fill", red),
                    _ => symbol("circle", None),
                };
                let item = add(menu, &name, None, Some(&j.name), mark);
                let has_sub: bool = msg_send![&*item, respondsToSelector: sel!(setSubtitle:)];
                if has_sub {
                    let _: () = msg_send![&*item, setSubtitle: &*NSString::from_str(&sub)];
                }
                let actions: Retained<AnyObject> = msg_send![class!(NSMenu), new];
                let _: () = msg_send![&*actions, setAutoenablesItems: false];
                if !has_sub {
                    add(&actions, &sub, None, None, None);
                    sep(&actions);
                }
                add(&actions, "Show in Deck", Some(sel!(show:)), Some(&j.name), symbol("macwindow", None));
                if !j.tab {
                    add(
                        &actions,
                        if j.running { "Restart" } else { "Start Again" },
                        Some(sel!(restart:)),
                        Some(&j.name),
                        symbol("arrow.clockwise", None),
                    );
                }
                add(
                    &actions,
                    if j.running { "Stop" } else { "Remove" },
                    Some(sel!(stop:)),
                    Some(&j.name),
                    symbol(if j.running { "stop.circle" } else { "trash" }, None),
                );
                let _: () = msg_send![&*item, setSubmenu: &*actions];
            }
            sep(menu);
        }
        if jobs.is_empty() {
            add(menu, "Nothing running in the background", None, None, None);
            sep(menu);
        }
        add(menu, "Open Deck", Some(sel!(openDeck:)), None, symbol("macwindow", None));
        if running > 0 {
            add(menu, &format!("Stop All ({running})"), Some(sel!(stopAll:)), None, symbol("stop.circle", None));
        }
        sep(menu);
        add(menu, "Quit Background Service", Some(sel!(quit:)), None, symbol("power", None));
    }
}
