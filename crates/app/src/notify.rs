//! macOS notifications for an agent that needs you: a permission to grant,
//! a question to answer. Clicking one brings deck forward on that pane.
//!
//! Notification Center needs a real app bundle (an identifier to file the
//! notifications under), so a bare `cargo run` binary posts nothing rather
//! than crashing; the installed Deck.app does.

use block2::RcBlock;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Request identifiers are `deck-<pane>-<n>`; the pane is what a click opens.
const PREFIX: &str = "deck-";

/// What the user did with a notification.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Clicked it: bring its pane forward.
    Open(u64),
    /// Approve the agent's request from the banner.
    Allow(u64),
    /// Approve it, and the same again in that chat without asking.
    AllowAlways(u64),
    Deny(u64),
    /// Typed an answer in the banner: the answer to a question, or for an
    /// approval what to do instead.
    Reply(u64, String),
}

/// What a notification is about, which sets its buttons.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// Something to know: clicking opens the pane.
    Info,
    /// An agent asks to run or edit something: Approve, Always, Deny, Reply.
    Permission,
    /// An agent asks a question: Reply.
    Question,
}

const CAT_PERMISSION: &str = "deck.permission";
const CAT_QUESTION: &str = "deck.question";

static CLICKS: Mutex<Option<UnboundedSender<Event>>> = Mutex::new(None);
static DELEGATE: OnceLock<Retained<Delegate>> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DeckNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Show it even while deck is in front: it is only posted when you
        /// are looking at another pane.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &block2::DynBlock<dyn Fn()>,
        ) {
            let id = response.notification().request().identifier().to_string();
            let action = response.actionIdentifier().to_string();
            // SAFETY: a text-input response answers `userText`; others don't
            // respond to it, which is asked first.
            let typed: Option<String> = unsafe {
                let has: bool = msg_send![response, respondsToSelector: objc2::sel!(userText)];
                if has {
                    let t: Option<Retained<NSString>> = msg_send![response, userText];
                    t.map(|t| t.to_string())
                } else {
                    None
                }
            };
            if let Some(pane) = pane_of(&id) {
                let ev = match action.as_str() {
                    "allow" => Event::Allow(pane),
                    "always" => Event::AllowAlways(pane),
                    "deny" => Event::Deny(pane),
                    "reply" => Event::Reply(pane, typed.unwrap_or_default()),
                    _ => Event::Open(pane),
                };
                if let Some(tx) = CLICKS.lock().ok().and_then(|t| t.clone()) {
                    let _ = tx.unbounded_send(ev);
                }
            }
            done.call(());
        }
    }
);

impl Delegate {
    fn new() -> Retained<Self> {
        // SAFETY: NSObject's plain init; the class adds no state.
        unsafe { msg_send![Self::alloc(), init] }
    }
}

fn pane_of(identifier: &str) -> Option<u64> {
    identifier
        .strip_prefix(PREFIX)?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Only an app with a bundle identifier can use Notification Center.
fn center() -> Option<Retained<UNUserNotificationCenter>> {
    NSBundle::mainBundle().bundleIdentifier()?;
    Some(UNUserNotificationCenter::currentNotificationCenter())
}

/// Ask for permission (macOS asks the user once), register the buttons
/// approvals and questions carry, and route what is done with them.
pub fn start() -> Option<UnboundedReceiver<Event>> {
    let center = center()?;
    let delegate = DELEGATE.get_or_init(Delegate::new);
    center.setDelegate(Some(ProtocolObject::from_ref(&**delegate)));
    register_categories(&center);
    let asked = RcBlock::new(|granted: Bool, _error: *mut NSError| {
        if !granted.as_bool() {
            tracing::info!("notifications not allowed; deck will only mark tabs");
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &asked,
    );
    let (tx, rx) = unbounded();
    if let Ok(mut c) = CLICKS.lock() {
        *c = Some(tx);
    }
    Some(rx)
}

/// Approve / Always allow / Deny / Reply on approvals; Reply on questions.
fn register_categories(center: &UNUserNotificationCenter) {
    use objc2::class;
    use objc2::runtime::AnyObject;
    // SAFETY: UserNotifications' plain class constructors.
    unsafe {
        let action = |id: &str, title: &str, options: usize| -> Retained<AnyObject> {
            msg_send![class!(UNNotificationAction), actionWithIdentifier: &*NSString::from_str(id), title: &*NSString::from_str(title), options: options]
        };
        let reply = |placeholder: &str| -> Retained<AnyObject> {
            msg_send![
                class!(UNTextInputNotificationAction),
                actionWithIdentifier: &*NSString::from_str("reply"),
                title: &*NSString::from_str("Reply…"),
                options: 0usize,
                textInputButtonTitle: &*NSString::from_str("Send"),
                textInputPlaceholder: &*NSString::from_str(placeholder)
            ]
        };
        let array = |items: &[&Retained<AnyObject>]| -> Retained<AnyObject> {
            let arr: Retained<AnyObject> = msg_send![class!(NSMutableArray), new];
            for i in items {
                let _: () = msg_send![&*arr, addObject: &***i];
            }
            arr
        };
        let empty: Retained<AnyObject> = msg_send![class!(NSArray), new];
        // Destructive = 2: Deny shows in red.
        let allow = action("allow", "Approve", 0);
        let always = action("always", "Always Allow", 0);
        let deny = action("deny", "Deny", 2);
        let instead = reply("Do this instead…");
        let answer = reply("Your answer…");
        let permission: Retained<AnyObject> = msg_send![
            class!(UNNotificationCategory),
            categoryWithIdentifier: &*NSString::from_str(CAT_PERMISSION),
            actions: &*array(&[&allow, &always, &deny, &instead]),
            intentIdentifiers: &*empty,
            options: 0usize
        ];
        let question: Retained<AnyObject> = msg_send![
            class!(UNNotificationCategory),
            categoryWithIdentifier: &*NSString::from_str(CAT_QUESTION),
            actions: &*array(&[&answer]),
            intentIdentifiers: &*empty,
            options: 0usize
        ];
        let set: Retained<AnyObject> = msg_send![class!(NSSet), setWithArray: &*array(&[&permission, &question])];
        let _: () = msg_send![center, setNotificationCategories: &*set];
    }
}

/// Play one of macOS's sounds (Glass, Ping, Pop…) by name, at `volume`
/// (0–1). "Default" is the system alert sound; "" or "None" is silence.
pub fn play(name: &str, volume: f32) {
    use objc2::class;
    use objc2::runtime::AnyObject;
    if name.is_empty() || name == "None" {
        return;
    }
    let name = if name == "Default" { "Tink" } else { name };
    // SAFETY: NSSound on whatever thread: it is thread-safe to play.
    unsafe {
        let s: Option<Retained<AnyObject>> = msg_send![class!(NSSound), soundNamed: &*NSString::from_str(name)];
        if let Some(s) = s {
            let _: () = msg_send![&*s, stop];
            let _: () = msg_send![&*s, setVolume: volume.clamp(0., 1.)];
            let _: bool = msg_send![&*s, play];
        }
    }
}

/// The sounds macOS ships, offered in Settings.
pub const SOUNDS: &[&str] = &[
    "None", "Default", "Glass", "Ping", "Pop", "Purr", "Hero", "Funk", "Submarine", "Blow", "Bottle", "Frog", "Morse", "Sosumi", "Basso",
];

/// Post a notification about `pane`, silent: the caller plays the sound it
/// wants (see [`play`]). Later ones for the same pane are grouped under it
/// in Notification Center.
#[allow(dead_code)]
pub fn post(pane: u64, title: &str, body: &str) {
    post_kind(pane, title, body, Kind::Info);
}

pub fn post_kind(pane: u64, title: &str, body: &str, kind: Kind) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let Some(center) = center() else {
        return;
    };
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let _ = UNNotificationSound::defaultSound;
    match kind {
        Kind::Info => {}
        Kind::Permission => content.setCategoryIdentifier(&NSString::from_str(CAT_PERMISSION)),
        Kind::Question => content.setCategoryIdentifier(&NSString::from_str(CAT_QUESTION)),
    }
    content.setThreadIdentifier(&NSString::from_str(&format!("{PREFIX}{pane}")));
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let id = NSString::from_str(&format!("{PREFIX}{pane}-{n}"));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    center.addNotificationRequest_withCompletionHandler(&request, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_carry_their_pane() {
        assert_eq!(pane_of("deck-12-3"), Some(12));
        assert_eq!(pane_of("deck-7"), Some(7));
        assert_eq!(pane_of("other-12-3"), None);
    }
}
