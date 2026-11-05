//! Small HTTP GETs for the browser chrome (search suggestions, favicons)
//! through NSURLSession, so proxies, certificates and the HTTP cache behave
//! as they do in Safari, with no TLS stack of our own.

#[cfg(target_os = "macos")]
mod apple {
    use block2::RcBlock;
    use futures::channel::oneshot;
    use objc2_foundation::{
        NSData, NSError, NSHTTPURLResponse, NSString, NSURLRequest, NSURLRequestCachePolicy,
        NSURLResponse, NSURLSession, NSURL,
    };
    use std::sync::Mutex;

    /// The body of a 2xx response, or `None` on any failure or after `timeout`
    /// seconds. Dropping the future does not cancel the request; its answer is
    /// simply discarded.
    pub async fn get(url: &str, timeout: f64) -> Option<Vec<u8>> {
        let url = NSURL::URLWithString(&NSString::from_str(url))?;
        let request = NSURLRequest::requestWithURL_cachePolicy_timeoutInterval(
            &url,
            NSURLRequestCachePolicy::UseProtocolCachePolicy,
            timeout,
        );
        let (tx, rx) = oneshot::channel::<Option<Vec<u8>>>();
        let tx = Mutex::new(Some(tx));
        let done = RcBlock::new(
            move |data: *mut NSData, response: *mut NSURLResponse, error: *mut NSError| {
                // SAFETY: NSURLSession hands over valid objects or null.
                let (data, response) = unsafe { (data.as_ref(), response.as_ref()) };
                let ok = error.is_null()
                    && response
                        .and_then(|r| r.downcast_ref::<NSHTTPURLResponse>())
                        .is_none_or(|r| (200..300).contains(&r.statusCode()));
                let body = data.filter(|_| ok).map(|d| d.to_vec());
                if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                    let _ = tx.send(body);
                }
            },
        );
        // SAFETY: the block only touches a Mutex-guarded sender, so it may run on
        // the session's own queue.
        let task = unsafe {
            NSURLSession::sharedSession().dataTaskWithRequest_completionHandler(&request, &done)
        };
        task.resume();
        rx.await.ok().flatten()
    }

    /// An HTTP request with a method, headers and a body: the status and body
    /// of whatever came back (any status), or `None` when nothing did.
    pub async fn request(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout: f64,
    ) -> Option<(u16, Vec<u8>)> {
        use objc2_foundation::NSMutableURLRequest;
        let url = NSURL::URLWithString(&NSString::from_str(url))?;
        let request = NSMutableURLRequest::requestWithURL(&url);
        request.setHTTPMethod(&NSString::from_str(method));
        request.setTimeoutInterval(timeout);
        for (k, v) in headers {
            request.setValue_forHTTPHeaderField(Some(&NSString::from_str(v)), &NSString::from_str(k));
        }
        if let Some(b) = body {
            request.setHTTPBody(Some(&NSData::from_vec(b)));
        }
        let (tx, rx) = oneshot::channel::<Option<(u16, Vec<u8>)>>();
        let tx = Mutex::new(Some(tx));
        let done = RcBlock::new(
            move |data: *mut NSData, response: *mut NSURLResponse, error: *mut NSError| {
                // SAFETY: NSURLSession hands over valid objects or null.
                let (data, response) = unsafe { (data.as_ref(), response.as_ref()) };
                let status = response
                    .and_then(|r| r.downcast_ref::<NSHTTPURLResponse>())
                    .map(|r| r.statusCode() as u16);
                let answer = status
                    .filter(|_| error.is_null())
                    .map(|st| (st, data.map(|d| d.to_vec()).unwrap_or_default()));
                if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                    let _ = tx.send(answer);
                }
            },
        );
        // SAFETY: as in `get`.
        let task = unsafe {
            NSURLSession::sharedSession().dataTaskWithRequest_completionHandler(&request, &done)
        };
        task.resume();
        rx.await.ok().flatten()
    }
}
#[cfg(target_os = "macos")]
pub use apple::{get, request};

#[cfg(not(target_os = "macos"))]
mod portable {
    use futures::channel::oneshot;

    /// The body of a 2xx response, or `None` on any failure or after `timeout`
    /// seconds. Dropping the future does not cancel the request; its answer is
    /// simply discarded.
    pub async fn get(url: &str, timeout: f64) -> Option<Vec<u8>> {
        match request("GET", url, &[], None, timeout).await {
            Some((200..=299, body)) => Some(body),
            _ => None,
        }
    }

    /// An HTTP request with a method, headers and a body: the status and body
    /// of whatever came back (any status), or `None` when nothing did. ureq on
    /// a thread of its own, with rustls and the system's certificates.
    pub async fn request(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
        timeout: f64,
    ) -> Option<(u16, Vec<u8>)> {
        let mut req = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(body.unwrap_or_default()).ok()?;
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs_f64(timeout.max(0.1))))
                .http_status_as_error(false)
                .build()
                .into();
            let answer = agent.run(req).ok().and_then(|mut r| {
                let status = r.status().as_u16();
                let body = r.body_mut().with_config().limit(64 << 20).read_to_vec().ok()?;
                Some((status, body))
            });
            let _ = tx.send(answer);
        });
        rx.await.ok().flatten()
    }
}
#[cfg(not(target_os = "macos"))]
pub use portable::{get, request};

/// The search engine's suggestions for `query`, best first. Each of these
/// answers in the OpenSearch shape; Kagi needs a login, so it borrows
/// DuckDuckGo's.
pub async fn suggestions(query: &str) -> Vec<String> {
    use crate::settings::Search;
    let q = crate::browser::encode_query(query);
    let url = match crate::browser::search_engine() {
        Search::Google => format!(
            "https://suggestqueries.google.com/complete/search?client=firefox&ie=utf-8&oe=utf-8&q={q}"
        ),
        Search::DuckDuckGo | Search::Kagi => {
            format!("https://duckduckgo.com/ac/?type=list&q={q}")
        }
        Search::Bing => format!("https://api.bing.com/osjson.aspx?query={q}"),
        Search::Brave => format!("https://search.brave.com/api/suggest?q={q}"),
    };
    get(&url, 3.)
        .await
        .map(|body| parse_suggestions(&body))
        .unwrap_or_default()
}

/// `["query", ["one", "two", …], …]`.
fn parse_suggestions(body: &[u8]) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.get(1)
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_bodies_parse() {
        assert_eq!(
            parse_suggestions(br#"["rust",["rust lang","rust game"],[],{}]"#),
            vec!["rust lang", "rust game"]
        );
        assert!(parse_suggestions(b"<html>").is_empty());
    }
}
