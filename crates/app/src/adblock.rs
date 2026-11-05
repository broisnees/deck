//! Ads and trackers kept out of Deck's pages: a compact list of the
//! networks that serve most of them, compiled once by WebKit into a content
//! rule list and attached to every page. Only third-party loads are
//! blocked, so a site's own content (and signing in to it) is never hit.
//!
//! Settings › Browser turns it off; pages opened after that load freely.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Hosts whose third-party loads are ads, trackers or fingerprinting.
const HOSTS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "adservice.google.com",
    "google-analytics.com",
    "googletagmanager.com",
    "googletagservices.com",
    "pagead2.googlesyndication.com",
    "amazon-adsystem.com",
    "adnxs.com",
    "adsrvr.org",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "scorecardresearch.com",
    "quantserve.com",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "casalemedia.com",
    "moatads.com",
    "adform.net",
    "smartadserver.com",
    "yieldmo.com",
    "media.net",
    "zedo.com",
    "bidswitch.net",
    "connect.facebook.net",
    "ads-twitter.com",
    "static.ads-twitter.com",
    "ads.linkedin.com",
    "px.ads.linkedin.com",
    "analytics.tiktok.com",
    "hotjar.com",
    "mouseflow.com",
    "fullstory.com",
    "crazyegg.com",
    "clarity.ms",
    "newrelic.com",
    "nr-data.net",
    "branch.io",
    "adroll.com",
    "bat.bing.com",
    "ads.yahoo.com",
    "advertising.com",
    "teads.tv",
    "sharethrough.com",
    "33across.com",
    "indexww.com",
];

/// The rules as WebKit's JSON: block third-party loads from those hosts.
pub fn rules() -> String {
    let rules: Vec<serde_json::Value> = HOSTS
        .iter()
        .map(|h| {
            let host = h.replace('.', "\\.");
            serde_json::json!({
                "trigger": { "url-filter": format!("^https?://([^/]+\\.)?{host}[:/]"), "load-type": ["third-party"] },
                "action": { "type": "block" }
            })
        })
        .collect();
    serde_json::to_string(&rules).unwrap_or_else(|_| "[]".into())
}

static ON: AtomicBool = AtomicBool::new(true);
/// The compiled list, once WebKit has made it (an `WKContentRuleList`).
static LIST: Mutex<Option<usize>> = Mutex::new(None);
/// Pages made before the list was ready, to give it to when it is.
static WAITING: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Compile the list once, at launch (main thread).
pub fn prepare() {
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    // SAFETY: WebKit's store, on the main thread; the block is called once.
    unsafe {
        let store: Option<Retained<AnyObject>> = msg_send![class!(WKContentRuleListStore), defaultStore];
        let Some(store) = store else { return };
        let id = NSString::from_str("deck-adblock-v1");
        let json = NSString::from_str(&rules());
        let done = block2::RcBlock::new(|list: *mut AnyObject, err: *mut AnyObject| {
            if list.is_null() {
                if !err.is_null() {
                    tracing::warn!("ad blocking: WebKit did not take the rules");
                }
                return;
            }
            // Kept for the life of the app.
            let list: Retained<AnyObject> = Retained::retain(list).expect("non-null");
            let ptr = Retained::into_raw(list) as usize;
            if let Ok(mut l) = LIST.lock() {
                *l = Some(ptr);
            }
            let waiting = WAITING.lock().map(|mut w| std::mem::take(&mut *w)).unwrap_or_default();
            for wk in waiting {
                attach_ptr(wk as *mut AnyObject, ptr as *mut AnyObject);
            }
        });
        let _: () = msg_send![
            &*store,
            compileContentRuleListForIdentifier: &*id,
            encodedContentRuleList: &*json,
            completionHandler: &*done
        ];
    }
}

/// Give a new page the list (now, or once it is compiled). Main thread.
pub fn attach(wk: &AnyObject) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let wk_ptr = wk as *const AnyObject as *mut AnyObject;
    match LIST.lock().ok().and_then(|l| *l) {
        Some(list) => attach_ptr(wk_ptr, list as *mut AnyObject),
        None => {
            if let Ok(mut w) = WAITING.lock() {
                w.push(wk_ptr as usize);
            }
        }
    }
}

fn attach_ptr(wk: *mut AnyObject, list: *mut AnyObject) {
    use objc2::msg_send;
    // SAFETY: a live WKWebView and a compiled rule list, main thread.
    unsafe {
        let Some(wk) = wk.as_ref() else { return };
        let Some(list) = list.as_ref() else { return };
        let config: Option<Retained<AnyObject>> = msg_send![wk, configuration];
        let Some(config) = config else { return };
        let ucc: Option<Retained<AnyObject>> = msg_send![&*config, userContentController];
        if let Some(ucc) = ucc {
            let _: () = msg_send![&*ucc, addContentRuleList: list];
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rules_are_json_webkit_takes() {
        let v: serde_json::Value = serde_json::from_str(&super::rules()).unwrap();
        let first = &v[0];
        assert_eq!(first["action"]["type"], "block");
        assert_eq!(first["trigger"]["load-type"][0], "third-party");
        assert!(first["trigger"]["url-filter"].as_str().unwrap().contains("doubleclick\\.net"));
    }
}
