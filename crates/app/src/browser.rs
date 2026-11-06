//! A browser pane: a gpui toolbar above a native WKWebView parented into the
//! window and kept glued to the pane's layout bounds.
//!
//! The webview is a real NSView above gpui's Metal layer, so nothing gpui
//! draws can overlap it. Everything interactive is laid out beside it, and
//! while the address bar shows suggestions the page steps aside for them.
//! A pane's first page stays hidden until it has committed and painted (no
//! white flash); after that the old page stays up while the next one loads,
//! as in Safari. A blank tab is a gpui page, not an empty white document.

use crate::favicon;
use crate::field::{Field, FieldEvent};
use crate::history::{self, Visit};
use crate::icon::{icon, spinner, Icon};
use crate::settings::Search;
use crate::theme::{self, u, OneLine};
use crate::workspace::icon_button;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    canvas, div, img, relative, rgb, rgba, AnyElement, Bounds, Context, ElementId, Entity,
    EventEmitter, FontWeight, Image, MouseButton, Pixels, SharedString, Subscription, Task, Window,
};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSView;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wry::dpi::{LogicalPosition, LogicalSize};

#[cfg_attr(not(target_os = "macos"), path = "other/popup.rs")]
mod popup;
#[cfg(windows)]
mod win;

/// Any open page on Windows, whose profile every page shares.
#[cfg(windows)]
pub fn any_page() -> Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2> {
    win::any_page()
}

pub enum BrowserEvent {
    /// The page has the keyboard now.
    Focused,
    /// The page asked for a new window it does not script (target=_blank,
    /// a plain window.open), or a link was ⌘-clicked.
    OpenTab(String),
    /// The title, address or icon changed: things the sidebar shows.
    Retitled,
    Close,
    /// The editor asked to open a folder (a worktree) in Deck: as an editor
    /// (`terminal: false`) or a terminal.
    OpenFolder { path: PathBuf, terminal: bool },
    /// A shortcut of Deck's, pressed in the page.
    Key(String),
    /// The import offer's buttons.
    Import,
    DismissImport,
}

pub enum Msg {
    /// One of Deck's shortcuts, pressed in the page.
    Key(String),
    Title(String),
    /// A navigation committed (`true`) or finished loading (`false`).
    Load(String, bool),
    /// The page changed its address without a load (pushState, #hash).
    Moved,
    Focused,
    NewWindow(String),
    /// A ⌘-click or middle click on a link.
    LinkTab(String),
    /// Where the page's icon is.
    Icon(String),
    DownloadStarted(PathBuf),
    DownloadDone(Option<PathBuf>, bool),
    /// The editor's `{deck: "open", kind, path}`.
    OpenFolder(PathBuf, bool),
    /// The recorder's `{deck: "log", entries}`: console lines, errors and
    /// requests, each a flat object.
    Log(Vec<Value>),
    /// The page is waiting on an `alert`, `confirm` or `prompt`.
    #[cfg(windows)]
    Dialog,
}

/// Safari's own, so sites serve their current code instead of the fallback
/// bundles an unknown WebKit gets (WKWebView's default has no `Safari/`).
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// Runs in every page before its own scripts. Clicks inside the page never
/// reach gpui, so it reports them; single-page apps move without loading, so
/// it reports that too (deck then reads the real address from WebKit rather
/// than trusting the page); and it names the page's best icon.
const PAGE_SCRIPT: &str = r#"(() => {
  const post = (m) => { try { window.ipc.postMessage(m); } catch (_) {} };
  addEventListener('mousedown', () => post('focus'), true);
  const moved = () => post('moved');
  for (const k of ['pushState', 'replaceState']) {
    const f = history[k];
    history[k] = function () { const r = f.apply(this, arguments); moved(); return r; };
  }
  addEventListener('popstate', moved);
  addEventListener('hashchange', moved);
  const icon = () => {
    let best = null, score = -1;
    for (const l of document.querySelectorAll('link[rel~="icon"], link[rel="apple-touch-icon"]')) {
      if (!l.href) continue;
      const svg = l.type === 'image/svg+xml' || /\.svg(\?|$)/.test(l.href);
      const size = parseInt((l.getAttribute('sizes') || '').split('x')[0], 10)
        || (l.rel.includes('apple') ? 180 : 16);
      const s = svg ? 1000 : (size >= 32 && size <= 192 ? 500 + size : size);
      if (s > score) { score = s; best = l.href; }
    }
    post('icon:' + (best || location.origin + '/favicon.ico'));
  };
  if (document.readyState === 'loading') addEventListener('DOMContentLoaded', icon);
  else icon();
  const tab = (e) => {
    const a = e.target && e.target.closest && e.target.closest('a[href]');
    if (!a || !/^https?:/.test(a.href)) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    post('tab:' + a.href);
  };
  addEventListener('click', (e) => { if (e.metaKey && e.button === 0) tab(e); }, true);
  addEventListener('auxclick', (e) => { if (e.button === 1) tab(e); }, true);
})();"#;

/// Deck's shortcuts, as "cmd-w", "cmd-shift-[": a page (or the editor)
/// hands these to Deck instead of keeping them, so ⌘W, ⌘K and the rest work
/// with a page focused. Set from the keymap whenever it changes.
static FORWARD: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn set_forward_keys(keys: Vec<String>) {
    if let Ok(mut f) = FORWARD.lock() {
        // One spelling for each: the order the page script and the key
        // monitor build them in ("cmd-shift-k" and "shift-cmd-k" are one
        // shortcut, and only one of them ever matched).
        *f = keys.iter().filter_map(|k| canonical(k)).collect();
    }
}

/// "cmd-shift-K" → "shift-cmd-k": modifiers as ctrl, alt, shift, cmd, then
/// the key, lowercase.
pub fn canonical(key: &str) -> Option<String> {
    let mut mods = (false, false, false, false);
    let mut last = None;
    for part in key.split('-') {
        match part {
            "ctrl" => mods.0 = true,
            "alt" => mods.1 = true,
            "shift" => mods.2 = true,
            "cmd" => mods.3 = true,
            "" => last = Some("-".to_string()),
            k => last = Some(k.to_lowercase()),
        }
    }
    let k = last?;
    let mut out = String::new();
    for (on, name) in [(mods.0, "ctrl-"), (mods.1, "alt-"), (mods.2, "shift-"), (mods.3, "cmd-")] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(&k);
    Some(out)
}

/// Deck's shortcuts pressed while a page has the keyboard, caught before
/// WebKit (which keeps some, ⌃Tab among them, from the page's own script).
static PAGE_KEYS: std::sync::Mutex<Option<futures::channel::mpsc::UnboundedSender<String>>> = std::sync::Mutex::new(None);

/// Watch the app's key presses: one of Deck's shortcuts, while a web view
/// is what has the keyboard, goes to Deck instead. Returns where they
/// arrive. Main thread, once. Off macOS the page's own key script
/// (`key_script`) is all there is.
#[cfg(not(target_os = "macos"))]
pub fn watch_page_keys() -> futures::channel::mpsc::UnboundedReceiver<String> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    if let Ok(mut t) = PAGE_KEYS.lock() {
        *t = Some(tx);
    }
    rx
}

#[cfg(target_os = "macos")]
pub fn watch_page_keys() -> futures::channel::mpsc::UnboundedReceiver<String> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let (tx, rx) = futures::channel::mpsc::unbounded();
    if let Ok(mut t) = PAGE_KEYS.lock() {
        *t = Some(tx);
    }
    let handler = block2::RcBlock::new(|event: *mut AnyObject| -> *mut AnyObject {
        // SAFETY: AppKit hands a live key event on the main thread.
        unsafe {
            let Some(ev) = event.as_ref() else { return event };
            let flags: usize = msg_send![ev, modifierFlags];
            let (shift, ctrl, alt, cmd) = (flags & (1 << 17) != 0, flags & (1 << 18) != 0, flags & (1 << 19) != 0, flags & (1 << 20) != 0);
            if !(ctrl || alt || cmd) {
                return event;
            }
            // Only while a web view is the first responder.
            let window: Option<Retained<AnyObject>> = msg_send![ev, window];
            let Some(window) = window else { return event };
            let responder: Option<Retained<AnyObject>> = msg_send![&*window, firstResponder];
            let in_page = responder.is_some_and(|r| {
                let mut v: Option<Retained<AnyObject>> = Some(r);
                let wk = class!(WKWebView);
                for _ in 0..12 {
                    let Some(cur) = v else { return false };
                    let is: bool = msg_send![&*cur, isKindOfClass: wk];
                    if is {
                        return true;
                    }
                    let isview: bool = msg_send![&*cur, isKindOfClass: class!(NSView)];
                    if !isview {
                        return false;
                    }
                    v = msg_send![&*cur, superview];
                }
                false
            });
            if !in_page {
                return event;
            }
            let code: u16 = msg_send![ev, keyCode];
            let key = match code {
                48 => "tab".to_string(),
                36 => "enter".to_string(),
                49 => "space".to_string(),
                51 => "backspace".to_string(),
                53 => "escape".to_string(),
                123 => "left".to_string(),
                124 => "right".to_string(),
                125 => "down".to_string(),
                126 => "up".to_string(),
                _ => {
                    let chars: Option<Retained<objc2_foundation::NSString>> = msg_send![ev, charactersIgnoringModifiers];
                    match chars.map(|c| c.to_string().to_lowercase()) {
                        Some(c) if c.chars().count() == 1 => c,
                        _ => return event,
                    }
                }
            };
            let mut combo = String::new();
            for (on, name) in [(ctrl, "ctrl-"), (alt, "alt-"), (shift, "shift-"), (cmd, "cmd-")] {
                if on {
                    combo.push_str(name);
                }
            }
            combo.push_str(&key);
            let ours = FORWARD.lock().is_ok_and(|f| f.contains(&combo));
            if !ours {
                return event;
            }
            if let Some(tx) = PAGE_KEYS.lock().ok().and_then(|t| t.clone()) {
                let _ = tx.unbounded_send(combo);
            }
            std::ptr::null_mut()
        }
    });
    // SAFETY: NSEvent's local monitor; the block lives as long as the app.
    unsafe {
        let mask: u64 = 1 << 10; // NSEventMaskKeyDown
        let _: Option<Retained<AnyObject>> =
            msg_send![class!(NSEvent), addLocalMonitorForEventsMatchingMask: mask, handler: &*handler];
    }
    std::mem::forget(handler);
    rx
}

/// The script that catches those keys in a page and posts `key:cmd-w`.
fn key_script() -> String {
    let keys = FORWARD.lock().map(|f| f.clone()).unwrap_or_default();
    let list = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".into());
    format!(
        r#"(() => {{
  const keys = new Set({list});
  const post = (m) => {{ try {{ window.ipc.postMessage(m); }} catch (_) {{}} }};
  const named = {{ BracketLeft: '[', BracketRight: ']', Comma: ',', Period: '.', Semicolon: ';', Quote: "'", Slash: '/', Backslash: '\\', Minus: '-', Equal: '=', Backquote: '`', Space: 'space', Enter: 'enter', Tab: 'tab', Escape: 'escape', Backspace: 'backspace', Delete: 'delete', ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' }};
  addEventListener('keydown', (e) => {{
    if (!e.metaKey && !e.ctrlKey && !e.altKey) return;
    const c = e.code || '';
    const k = c.startsWith('Key') ? c.slice(3).toLowerCase() : c.startsWith('Digit') ? c.slice(5) : named[c];
    if (!k) return;
    const combo = (e.ctrlKey ? 'ctrl-' : '') + (e.altKey ? 'alt-' : '') + (e.shiftKey ? 'shift-' : '') + (e.metaKey ? 'cmd-' : '') + k;
    if (!keys.has(combo)) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    post('key:' + combo);
  }}, true);
}})();"#
    )
}

/// Runs in every web page (not the editor) before its own scripts, and
/// records what devtools would show an agent: console output, uncaught
/// errors and rejections, failed loads, `fetch` and XHR calls with their
/// bodies clipped, other resources, and the document itself. Entries go to
/// deck in batches over `window.ipc` at most every 200 ms. It keeps the
/// page's behaviour as it was and never throws into it.
///
/// It also logs `alert`, `confirm` and `prompt`, and answers them itself
/// for a moment after an agent's click or keypress (`window.__deckAct`, set
/// by `browse.rs`), so a dialog never leaves the agent's call hanging.
pub(crate) const RECORDER_SCRIPT: &str = r#"(() => {
  const W = window;
  if (W.__deckRecorder) return;
  try { Object.defineProperty(W, '__deckRecorder', { value: true }); } catch (_) { return; }
  try {
    const setT = W.setTimeout.bind(W), stringify = JSON.stringify, perf = W.performance;
    const MAX = 500, BATCH = 100, CLIP = 2048, TEXT = 4096;
    const q = [];
    let timer = 0, dropped = 0;
    const flush = () => {
      timer = 0;
      if (dropped) { q.push({ t: 'console', level: 'warn', text: 'deck: ' + dropped + ' log entries dropped' }); dropped = 0; }
      while (q.length) {
        const m = stringify({ deck: 'log', entries: q.splice(0, BATCH) });
        try { W.ipc.postMessage(m); } catch (_) {}
      }
    };
    const push = (e) => {
      try {
        if (q.length >= MAX) { q.shift(); dropped++; }
        q.push(e);
        if (!timer) timer = setT(flush, 200);
      } catch (_) {}
    };
    W.addEventListener('pagehide', () => { try { flush(); } catch (_) {} }, true);
    document.addEventListener('visibilitychange', () => {
      try { if (document.visibilityState === 'hidden') flush(); } catch (_) {}
    }, true);

    const clip = (s, n) => { s = String(s); return s.length > n ? s.slice(0, n) + '… [' + s.length + ' chars]' : s; };
    const errText = (e) => {
      try {
        const head = (e.name || 'Error') + ': ' + e.message, st = e.stack ? String(e.stack) : '';
        return st.indexOf(head) === 0 ? st : st ? head + '\n' + st : head;
      } catch (_) { return String(e); }
    };
    const nodeText = (n) => {
      if (n.nodeType !== 1) return '#' + String(n.nodeName || 'node').toLowerCase().replace(/^#/, '');
      let s = '<' + n.tagName.toLowerCase();
      if (n.id) s += '#' + n.id;
      if (typeof n.className === 'string' && n.className.trim()) s += '.' + n.className.trim().split(/\s+/).join('.');
      return s + '>';
    };
    const isNode = (v) => typeof Node !== 'undefined' && v instanceof Node;
    const show = (v) => {
      try {
        if (typeof v === 'string') return v;
        if (v === undefined) return 'undefined';
        if (v === null || typeof v !== 'object' && typeof v !== 'function') return String(v);
        if (typeof v === 'function') return 'function ' + (v.name || 'anonymous') + '()';
        if (v instanceof Error) return errText(v);
        if (isNode(v)) return nodeText(v);
        const seen = new WeakSet();
        let budget = 1000;
        const out = stringify(v, function (k, x) {
          if (--budget < 0) return undefined;
          if (typeof x === 'bigint') return x + 'n';
          if (typeof x === 'function') return 'function ' + (x.name || 'anonymous') + '()';
          if (x === undefined) return 'undefined';
          if (x && typeof x === 'object') {
            if (x instanceof Error) return errText(x);
            if (isNode(x)) return nodeText(x);
            if (x instanceof Map) return 'Map(' + x.size + ')';
            if (x instanceof Set) return 'Set(' + x.size + ')';
            if (seen.has(x)) return '[Circular]';
            seen.add(x);
          }
          return x;
        });
        return out === undefined ? String(v) : out;
      } catch (_) {
        try { return String(v); } catch (_) { return '[unprintable]'; }
      }
    };
    const fmt = (args) => {
      const out = [];
      let i = 0;
      if (typeof args[0] === 'string' && args[0].indexOf('%') >= 0) {
        i = 1;
        out.push(args[0].replace(/%([sdifoOc%])/g, (m, c) => {
          if (c === '%') return '%';
          if (i >= args.length) return m;
          const a = args[i++];
          if (c === 'c') return '';
          if (c === 'd' || c === 'i') return String(parseInt(a, 10));
          if (c === 'f') return String(parseFloat(a));
          return show(a);
        }));
      }
      for (; i < args.length; i++) out.push(show(args[i]));
      return clip(out.join(' '), TEXT);
    };

    // The console, as the page uses it.
    const C = W.console;
    if (C) {
      for (const level of ['log', 'info', 'warn', 'error', 'debug', 'trace']) {
        const orig = C[level];
        if (typeof orig !== 'function') continue;
        C[level] = function () {
          try { push({ t: 'console', level: level === 'trace' ? 'log' : level, text: fmt(Array.from(arguments)) }); } catch (_) {}
          return orig.apply(this, arguments);
        };
      }
      const assert = C.assert;
      if (typeof assert === 'function') {
        C.assert = function (ok) {
          try {
            if (!ok) {
              const rest = Array.from(arguments).slice(1);
              push({ t: 'console', level: 'error', text: 'Assertion failed' + (rest.length ? ': ' + fmt(rest) : '') });
            }
          } catch (_) {}
          return assert.apply(this, arguments);
        };
      }
    }

    // Uncaught errors, and elements that failed to load (capture phase:
    // those events do not bubble).
    const failedUrls = new Set();
    const failedRes = (type, url) => {
      if (failedUrls.size > 500) failedUrls.clear();
      failedUrls.add(url);
      for (const x of q) {
        if (x.t === 'net' && x.url === url && !x.failed) { x.failed = true; x.error = 'failed to load'; return; }
      }
      push({ t: 'net', type, url, failed: true, error: 'failed to load' });
    };
    W.addEventListener('error', (ev) => {
      try {
        const el = ev.target;
        if (el && el !== W && el.nodeType === 1) {
          const url = el.currentSrc || el.src || el.href;
          if (url && typeof url === 'string') failedRes(el.tagName.toLowerCase(), url);
          return;
        }
        const e = { t: 'error', kind: 'error', text: clip(ev.message || (ev.error ? errText(ev.error) : 'error'), TEXT) };
        if (ev.filename) e.src = ev.filename + ':' + ev.lineno + ':' + ev.colno;
        if (ev.error && ev.error.stack) e.stack = clip(String(ev.error.stack), TEXT);
        push(e);
      } catch (_) {}
    }, true);
    W.addEventListener('unhandledrejection', (ev) => {
      try {
        const r = ev.reason;
        const e = { t: 'error', kind: 'rejection', text: clip('Unhandled rejection: ' + (r instanceof Error ? (r.name || 'Error') + ': ' + r.message : show(r)), TEXT) };
        if (r && r.stack) e.stack = clip(String(r.stack), TEXT);
        push(e);
      } catch (_) {}
    });

    // fetch and XHR, with their bodies clipped.
    const ms = (t0) => Math.round(perf.now() - t0);
    const abs = (u) => { try { return new URL(String(u), location.href).href; } catch (_) { return String(u); } };
    const textual = (ct) => /json|text|xml|javascript|graphql|form/i.test(ct) && !/event-stream/i.test(ct);
    const bodyText = (b) => {
      if (typeof b === 'string') return clip(b, CLIP);
      const is = (name) => typeof W[name] === 'function' && b instanceof W[name];
      if (is('URLSearchParams')) return clip(b.toString(), CLIP);
      if (is('FormData')) return '[FormData]';
      if (is('Blob')) return '[Blob ' + b.size + ' bytes]';
      if (is('ArrayBuffer') || (typeof ArrayBuffer === 'function' && ArrayBuffer.isView(b))) return '[binary ' + b.byteLength + ' bytes]';
      if (is('ReadableStream')) return '[stream]';
      if (is('Document')) return '[Document]';
      return '[' + Object.prototype.toString.call(b).slice(8, -1) + ']';
    };
    const failure = (e, err, t0) => {
      e.failed = true;
      e.error = clip(String((err && err.message) || err), 300);
      if (err && err.name === 'AbortError') e.aborted = true;
      e.ms = ms(t0);
      push(e);
    };
    // The start of a response's body, read from a copy and then let go.
    const peek = (res) => new Promise((resolve) => {
      let over = false;
      const end = (v) => { if (!over) { over = true; resolve(v); } };
      setT(() => end(null), 3000);
      try {
        const c = res.clone();
        if (c.body && c.body.getReader && typeof TextDecoder === 'function') {
          const rd = c.body.getReader(), dec = new TextDecoder();
          let s = '';
          const pump = () => rd.read().then((r) => {
            if (r.value) s += dec.decode(r.value, { stream: !r.done });
            if (r.done || s.length > CLIP || over) {
              if (!r.done) { try { rd.cancel(); } catch (_) {} }
              end(s.length > CLIP || !r.done ? s.slice(0, CLIP) + '…' : s);
              return;
            }
            return pump();
          }, () => end(s ? s.slice(0, CLIP) : null));
          pump();
        } else {
          c.text().then((t) => end(clip(t, CLIP)), () => end(null));
        }
      } catch (_) { end(null); }
    });
    const ofetch = W.fetch;
    if (typeof ofetch === 'function') {
      W.fetch = function (input, init) {
        const t0 = perf.now();
        const e = { t: 'net', type: 'fetch', method: 'GET', url: '' };
        try {
          const req = typeof Request === 'function' && input instanceof Request;
          e.url = abs(req ? input.url : input);
          e.method = String((init && init.method) || (req && input.method) || 'GET').toUpperCase();
          if (init && init.body != null) e.req = bodyText(init.body);
        } catch (_) {}
        let p;
        try { p = ofetch.apply(this, arguments); } catch (err) { failure(e, err, t0); throw err; }
        try {
          p.then((res) => {
            try {
              e.status = res.status;
              e.ms = ms(t0);
              const ct = (res.headers && res.headers.get('content-type')) || '';
              if (ct) e.ctype = ct;
              if (res.redirected && res.url) e.final = res.url;
              if (res.type === 'opaque' || res.type === 'opaqueredirect') { e.opaque = true; push(e); return; }
              if (textual(ct) && res.body !== null) {
                peek(res).then((t) => { if (t != null) e.res = t; push(e); });
              } else {
                push(e);
              }
            } catch (_) { push(e); }
          }, (err) => failure(e, err, t0));
        } catch (_) {}
        return p;
      };
    }
    const X = W.XMLHttpRequest && W.XMLHttpRequest.prototype;
    if (X && typeof X.open === 'function' && typeof X.send === 'function') {
      const xopen = X.open, xsend = X.send, pending = new WeakMap();
      X.open = function (method, url) {
        try { pending.set(this, { t: 'net', type: 'xhr', method: String(method || 'GET').toUpperCase(), url: abs(url) }); } catch (_) {}
        return xopen.apply(this, arguments);
      };
      X.send = function (body) {
        try {
          const x = this, e = pending.get(x);
          if (e) {
            pending.delete(x);
            const t0 = perf.now();
            if (body != null) e.req = bodyText(body);
            let why = '';
            for (const k of ['abort', 'timeout', 'error']) x.addEventListener(k, () => { why = k; }, { once: true });
            x.addEventListener('loadend', () => {
              try {
                e.ms = ms(t0);
                e.status = x.status;
                const ct = x.getResponseHeader('content-type') || '';
                if (ct) e.ctype = ct;
                if (!x.status) {
                  e.failed = true;
                  e.error = why === 'abort' ? 'aborted' : why || 'network error';
                  if (why === 'abort') e.aborted = true;
                } else if (textual(ct)) {
                  const rt = x.responseType;
                  if (rt === '' || rt === 'text') e.res = clip(x.responseText, CLIP);
                  else if (rt === 'json') e.res = clip(stringify(x.response), CLIP);
                }
                push(e);
              } catch (_) {}
            }, { once: true });
          }
        } catch (_) {}
        return xsend.apply(this, arguments);
      };
    }

    // Everything else the page loads (fetch and XHR are covered above).
    const resource = (en) => {
      try {
        if (en.initiatorType === 'fetch' || en.initiatorType === 'xmlhttprequest' || failedUrls.has(en.name)) return;
        const e = { t: 'net', type: en.initiatorType || 'other', url: en.name, ms: Math.max(0, Math.round(en.duration)) };
        if (en.transferSize) e.size = en.transferSize;
        if (en.responseStatus) e.status = en.responseStatus;
        push(e);
      } catch (_) {}
    };
    try {
      const po = new PerformanceObserver((list) => { for (const en of list.getEntries()) resource(en); });
      try { po.observe({ type: 'resource', buffered: true }); } catch (_) { po.observe({ entryTypes: ['resource'] }); }
    } catch (_) {}
    W.addEventListener('load', () => setT(() => {
      try {
        const n = perf.getEntriesByType && perf.getEntriesByType('navigation')[0];
        const e = { t: 'net', type: 'document', method: 'GET', url: (n && n.name) || location.href };
        if (n) {
          e.ms = Math.round(n.duration || 0);
          if (n.responseStatus) e.status = n.responseStatus;
          if (n.transferSize) e.size = n.transferSize;
        }
        push(e);
      } catch (_) {}
    }, 0));

    // Dialogs: logged, and answered for an agent while its action runs.
    for (const kind of ['alert', 'confirm', 'prompt']) {
      const orig = W[kind];
      if (typeof orig !== 'function') continue;
      W[kind] = function (message, value) {
        const e = { t: 'dialog', kind, text: clip(message === undefined ? '' : String(message), TEXT) };
        const act = W.__deckAct;
        if (act && act.until > Date.now()) {
          const r = kind === 'alert' ? undefined
            : kind === 'confirm' ? !!act.accept
            : !act.accept ? null
            : act.prompt != null ? String(act.prompt) : value === undefined ? '' : String(value);
          e.answer = kind === 'alert' ? 'ok' : r;
          e.auto = true;
          push(e);
          try { act.dialogs.push({ kind, text: e.text, answer: e.answer }); } catch (_) {}
          return r;
        }
        const r = orig.apply(this, arguments);
        if (kind !== 'alert') e.answer = r;
        push(e);
        return r;
      };
    }

    push({ t: 'page', url: location.href });
  } catch (_) {}
})();"#;

/// The most entries one `{deck: "log"}` message may carry, and how many a
/// pane keeps.
const LOG_BATCH: usize = 200;
const LOG_KEEP: usize = 2000;

const PAGE_TEXT: &str =
    "document.title + '\\n' + location.href + '\\n\\n' + (document.body ? document.body.innerText : '')";

/// Where the address pill starts and ends inside the toolbar, so the
/// suggestion list lines up under it: three 26 px buttons with their gaps
/// on the left, the close button on the right.
const PILL_LEFT: f32 = 96.;
const PILL_RIGHT: f32 = 38.;

fn is_blank(url: &str) -> bool {
    url.is_empty() || url == "about:blank"
}

fn is_web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// What the page sent over `window.ipc`. Anything else is ignored: the page
/// owns this channel too, so nothing it says is taken as an address to show.
fn parse_ipc(body: &str) -> Option<Msg> {
    if body == "focus" {
        return Some(Msg::Focused);
    }
    if body == "moved" {
        return Some(Msg::Moved);
    }
    if let Some(url) = body.strip_prefix("icon:") {
        return is_web(url).then(|| Msg::Icon(url.to_string()));
    }
    if let Some(k) = body.strip_prefix("key:") {
        return (k.len() < 40).then(|| Msg::Key(k.to_string()));
    }
    if let Some(url) = body.strip_prefix("tab:") {
        return is_web(url).then(|| Msg::LinkTab(url.to_string()));
    }
    // A recorder batch is at most 100 entries of a few KB; anything far
    // past that is not one.
    if body.starts_with('{') && body.len() <= 4 << 20 {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        match v.get("deck")?.as_str()? {
            // The editor's requests; heeded only when this pane is the editor.
            "open" => {
                let path = PathBuf::from(v.get("path")?.as_str()?);
                let terminal = v.get("kind").and_then(|k| k.as_str()) == Some("terminal");
                return path.is_absolute().then_some(Msg::OpenFolder(path, terminal));
            }
            // Any page may log, so what it sends is trimmed to flat,
            // bounded entries.
            "log" => {
                let entries: Vec<Value> = v
                    .get("entries")?
                    .as_array()?
                    .iter()
                    .filter_map(log_entry)
                    .take(LOG_BATCH)
                    .collect();
                return (!entries.is_empty()).then_some(Msg::Log(entries));
            }
            _ => {}
        }
    }
    None
}

/// One recorded entry as kept: an object of at most 16 plain fields, each
/// string clipped to 4 KB. `None` for anything that is not an object.
fn log_entry(v: &Value) -> Option<Value> {
    const FIELD: usize = 4096;
    let mut out = serde_json::Map::new();
    for (k, v) in v.as_object()?.iter().take(16) {
        if k.len() > 32 || k == "seq" || k == "at" {
            continue;
        }
        let v = match v {
            Value::String(s) if s.len() > FIELD => {
                let mut end = FIELD;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                Value::String(format!("{}…", &s[..end]))
            }
            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => v.clone(),
            _ => continue,
        };
        out.insert(k.clone(), v);
    }
    out.get("t").and_then(Value::as_str)?;
    Some(Value::Object(out))
}

/// Answers a page asking for a new window. A script that wants a window
/// of its own or starts a sign-in gets a popup linked to it (see
/// `popup::wants`); anything else, a link with a target too, opens as a
/// Deck tab, which the page cannot reach. The editor's are all tabs.
fn new_window(
    tx: UnboundedSender<Msg>,
    is_editor: bool,
) -> impl Fn(String, wry::NewWindowFeatures) -> wry::NewWindowResponse + 'static {
    move |url, features| {
        if !is_editor && popup::wants(&url, popup::asked(&features)) {
            let (page, nav) = (tx.clone(), tx.clone());
            let opened = popup::open(
                &url,
                features,
                |b| popup_page(b, page),
                move |u| {
                    let e = serde_json::json!({ "t": "nav", "url": u, "popup": true });
                    let _ = nav.unbounded_send(Msg::Log(vec![e]));
                },
                add_page_dialogs,
            );
            if let Some(answer) = opened {
                return answer;
            }
        }
        let _ = tx.unbounded_send(Msg::NewWindow(url));
        wry::NewWindowResponse::Deny
    }
}

/// A popup's page, set up as a pane's is. What it logs goes to the pane
/// that opened it, marked `popup`, and so do its downloads and ⌘-clicks;
/// what the page script says of focus, moves and icons is its own.
fn popup_page(b: wry::WebViewBuilder<'_>, tx: UnboundedSender<Msg>) -> wry::WebViewBuilder<'_> {
    let (tx_ipc, tx_dl, tx_done, tx_new) = (tx.clone(), tx.clone(), tx.clone(), tx);
    b.with_user_agent(USER_AGENT)
        .with_devtools(true)
        .with_accept_first_mouse(true)
        .with_permission_handler(|_| wry::PermissionResponse::Default)
        .with_initialization_script(PAGE_SCRIPT)
        .with_initialization_script(RECORDER_SCRIPT)
        .with_navigation_handler(|to| !crate::code::is_editor_url(&to))
        .with_ipc_handler(move |req| match parse_ipc(req.body()) {
            Some(Msg::Log(mut entries)) => {
                for e in &mut entries {
                    e["popup"] = true.into();
                }
                let _ = tx_ipc.unbounded_send(Msg::Log(entries));
            }
            Some(tab @ Msg::LinkTab(_)) => {
                let _ = tx_ipc.unbounded_send(tab);
            }
            _ => {}
        })
        .with_new_window_req_handler(new_window(tx_new, false))
        .with_download_started_handler(move |_, path| {
            let _ = tx_dl.unbounded_send(Msg::DownloadStarted(path.clone()));
            true
        })
        .with_download_completed_handler(move |_, path, ok| {
            let _ = tx_done.unbounded_send(Msg::DownloadDone(path, ok));
        })
}

/// What `browser.logs` reads: entries after `since` of one `kind`, the
/// last `limit` of them.
pub struct LogQuery {
    pub kind: LogKind,
    pub since: u64,
    pub limit: usize,
    pub failed: bool,
    pub filter: Option<String>,
    pub clear: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LogKind {
    All,
    /// Console lines, uncaught errors, dialogs and page markers.
    Console,
    Network,
    /// Uncaught errors, `console.error`, and failed or >= 400 requests.
    Errors,
}

impl LogQuery {
    pub fn from_params(p: &Value) -> Result<Self, String> {
        let num = |k: &str| {
            p.get(k)
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
        };
        let flag = |k: &str| {
            p.get(k)
                .is_some_and(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
        };
        let kind = match p.get("kind").and_then(Value::as_str).unwrap_or("all") {
            "all" | "logs" => LogKind::All,
            "console" => LogKind::Console,
            "network" | "net" => LogKind::Network,
            "errors" | "error" => LogKind::Errors,
            k => return Err(format!("kind is console, network, errors or all, got {k:?}")),
        };
        Ok(Self {
            kind,
            since: num("since").unwrap_or(0),
            limit: num("limit").unwrap_or(100).clamp(1, 1000) as usize,
            failed: flag("failed"),
            filter: p
                .get("filter")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_lowercase),
            clear: flag("clear"),
        })
    }

    fn wants(&self, e: &Value) -> bool {
        let t = e.get("t").and_then(Value::as_str).unwrap_or("");
        let failed = t == "net"
            && (e.get("failed").and_then(Value::as_bool).unwrap_or(false)
                || e.get("status").and_then(Value::as_u64).is_some_and(|s| s >= 400));
        let kind = match self.kind {
            LogKind::All => true,
            LogKind::Console => matches!(t, "console" | "error" | "page" | "nav" | "dialog"),
            LogKind::Network => t == "net",
            LogKind::Errors => {
                t == "error"
                    || failed
                    || (t == "console" && e.get("level").and_then(Value::as_str) == Some("error"))
            }
        };
        if !kind || (self.failed && !failed) {
            return false;
        }
        match &self.filter {
            None => true,
            Some(f) => ["url", "text"].iter().any(|k| {
                e.get(*k)
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.to_lowercase().contains(f))
            }),
        }
    }
}

#[derive(Clone)]
enum Row {
    /// Open this address: what Enter does when the text is one.
    Go {
        url: String,
        label: String,
    },
    Search(String),
    Page(Visit),
    Server(u16, String),
}

struct Suggest {
    rows: Vec<Row>,
    selected: usize,
}

struct FindBar {
    field: Entity<Field>,
    matches: Option<usize>,
    _sub: Subscription,
}

#[derive(Clone, Copy, PartialEq)]
enum DownloadState {
    Running,
    Done,
    Failed,
}

struct Download {
    name: String,
    path: Option<PathBuf>,
    state: DownloadState,
}

pub struct BrowserPane {
    /// The browser to offer importing from, until the offer is answered.
    pub import_offer: Option<SharedString>,
    pub importing: bool,
    webview: wry::WebView,
    /// The view the webview fills, placed where the pane is. Dropped after
    /// the webview, which takes itself out of it first.
    host: Option<Host>,
    /// What the page logged and fetched (see `RECORDER_SCRIPT`), oldest
    /// first. It outlives navigations.
    logs: VecDeque<Value>,
    log_seq: u64,
    /// Set when this pane is Deck's code editor (see `code.rs`): the page
    /// may then use the editor API.
    code: crate::code::Shared,
    pub url: SharedString,
    pub title: SharedString,
    pub loading: bool,
    /// 0–1 while loading, WebKit's own estimate.
    progress: f32,
    can_back: bool,
    can_forward: bool,
    pub address: Entity<Field>,
    /// Dev servers listening on this machine, offered on the blank page.
    pub servers: Vec<(u16, String)>,
    /// The app zoom, applied to the page so it scales with everything else.
    zoom: f64,
    bounds: Option<Bounds<Pixels>>,
    /// Whether this pane's tab is the one on screen.
    tab_visible: bool,
    /// Set once the first page has painted; until then the webview stays
    /// hidden and the pane shows black instead of WebKit's white.
    revealed: bool,
    shown: bool,
    /// Something floats over the window (the command palette) that a
    /// native view would cover.
    covered: bool,
    /// A picture of the page, shown in its place while something covers
    /// it (the native page can't be drawn over, so it is hidden then).
    cover_shot: Option<std::sync::Arc<Image>>,
    favicon: Option<Arc<Image>>,
    /// What was typed in the address bar, without the inline completion.
    typed: String,
    /// The address bar is being edited, so a page moving on does not
    /// overwrite what is being typed.
    editing: bool,
    /// The inline completion: the text it shows and the URL it opens.
    completion: Option<(String, String)>,
    /// Text changes deck made itself and should not treat as typing.
    quiet: u32,
    /// The address bar was not focused when the press began, so the click
    /// selects the whole address instead of placing the caret.
    select_on_click: bool,
    suggest: Option<Suggest>,
    /// Google's suggestions, and the text they were for.
    remote: Vec<String>,
    remote_for: String,
    /// The new tab page's sites.
    top: Vec<Visit>,
    find: Option<FindBar>,
    download: Option<Download>,
    last_link_tab: Option<Instant>,
    watching: bool,
    _watch: Option<Task<()>>,
    _reveal: Option<Task<()>>,
    _icon: Option<Task<()>>,
    _remote: Option<Task<()>>,
    _download: Option<Task<()>>,
    /// The page's `alert`, `confirm` or `prompt`, shown above it while it
    /// waits (WebView2 would draw its own, out of an agent's reach).
    #[cfg(windows)]
    dialog: Option<PageDialog>,
    _events: Task<()>,
    _subs: [Subscription; 2],
}

/// A page's dialog in the bar above it, with a prompt's field.
#[cfg(windows)]
struct PageDialog {
    kind: &'static str,
    text: String,
    field: Option<Entity<Field>>,
    _sub: Option<Subscription>,
}

impl EventEmitter<BrowserEvent> for BrowserPane {}

impl BrowserPane {
    /// The fallible half: create the native view inside `window`.
    pub fn build(
        url: &str,
        editor: Option<crate::code::Editor>,
        window: &Window,
    ) -> anyhow::Result<(wry::WebView, UnboundedReceiver<Msg>, String, crate::code::Shared)> {
        // An editor comes from Deck, or back from the session by its address.
        let editor = editor.or_else(|| crate::code::Editor::from_url(url));
        let url = if let Some(e) = &editor {
            e.url()
        } else if is_blank(url) || crate::code::is_editor_url(url) {
            "about:blank".to_string()
        } else {
            normalize_url(url)
        };
        let is_editor = editor.is_some();
        #[cfg(windows)]
        win::check_runtime()?;
        let code: crate::code::Shared = std::sync::Arc::new(std::sync::Mutex::new(editor));
        let (tx, rx) = futures::channel::mpsc::unbounded::<Msg>();
        #[cfg(windows)]
        let tx_dialog = tx.clone();
        let (tx_title, tx_load, tx_ipc, tx_new, tx_dl, tx_done, tx_nav) = (
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx,
        );

        // WebView2 keeps its profile beside the executable unless told.
        #[cfg(windows)]
        let mut context = wry::WebContext::new(Some(win::data_dir()));
        #[cfg(windows)]
        let builder = wry::WebViewBuilder::new_with_web_context(&mut context);
        #[cfg(not(windows))]
        let builder = wry::WebViewBuilder::new();
        let mut builder = builder
            .with_url(engine_uri(&url))
            .with_bounds(rect(Bounds::default()))
            // Hidden until it has painted, on macOS and Windows. A
            // WebKitGTK page made hidden stays a 1x1 window after it is
            // shown (wry's own note), so on Linux it starts shown, at no
            // size until it is laid out.
            .with_visible(cfg!(target_os = "linux"))
            // Right-click › Inspect Element, and Safari's Develop menu.
            .with_devtools(true)
            .with_back_forward_navigation_gestures(true)
            .with_accept_first_mouse(true)
            // Camera and microphone: WebKit asks per site, as Safari does,
            // instead of wry's default of granting every request.
            .with_permission_handler(|_| wry::PermissionResponse::Default)
            .with_initialization_script(PAGE_SCRIPT)
            .with_initialization_script(&key_script());
        // Safari's name suits WebKit; WebView2 is Chromium and keeps Edge's.
        if !cfg!(windows) {
            builder = builder.with_user_agent(USER_AGENT);
        }
        // The editor is Deck's own page; agents read the pages they test.
        if !is_editor {
            builder = builder.with_initialization_script(RECORDER_SCRIPT);
        }
        let webview = builder
            .with_asynchronous_custom_protocol(
                crate::code::SCHEME.into(),
                crate::code::protocol(code.clone()),
            )
            // The editor stays the editor: a link it shows opens in a tab of
            // its own. No other page may load the editor's scheme.
            .with_navigation_handler(move |to| {
                let to = editor_uri(to);
                let to_editor = crate::code::is_editor_url(&to);
                if is_editor && !to_editor && !to.starts_with("about:") {
                    let _ = tx_nav.unbounded_send(Msg::NewWindow(to));
                    return false;
                }
                !to_editor || is_editor
            })
            .with_ipc_handler(move |req| {
                if let Some(msg) = parse_ipc(req.body()) {
                    let _ = tx_ipc.unbounded_send(msg);
                }
            })
            .with_document_title_changed_handler(move |t| {
                let _ = tx_title.unbounded_send(Msg::Title(t));
            })
            .with_on_page_load_handler(move |ev, url| {
                let committed = matches!(ev, wry::PageLoadEvent::Started);
                let _ = tx_load.unbounded_send(Msg::Load(url, committed));
            })
            .with_new_window_req_handler(new_window(tx_new, is_editor))
            // Into ~/Downloads, numbered rather than overwriting.
            .with_download_started_handler(move |_, path| {
                let _ = tx_dl.unbounded_send(Msg::DownloadStarted(path.clone()));
                true
            })
            .with_download_completed_handler(move |_, path, ok| {
                let _ = tx_done.unbounded_send(Msg::DownloadDone(path, ok));
            })
            .build_as_child(window)?;
        add_page_dialogs(&webview);
        #[cfg(target_os = "linux")]
        crate::gtk_loop::PAGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(windows)]
        {
            use wry::WebViewExtWindows;
            win::attach(&webview, !is_blank(&url), move || {
                let _ = tx_dialog.unbounded_send(Msg::Dialog);
            })?;
            win::set_dark(&webview, theme::is_dark());
            crate::browser_import::install_waiting(&webview.webview());
        }
        #[cfg(target_os = "macos")]
        {
            use wry::WebViewExtMacOS;
            crate::adblock::attach(&webview.webview());
        }
        Ok((webview, rx, url, code))
    }

    pub fn new(
        webview: wry::WebView,
        mut rx: UnboundedReceiver<Msg>,
        url: String,
        code: crate::code::Shared,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                if this.update(cx, |p, cx| p.on_msg(msg, cx)).is_err() {
                    return;
                }
            }
        });

        let address = cx.new(|cx| {
            let mut f = Field::new(
                SharedString::from(format!(
                    "Search {} or enter address",
                    search_engine().name()
                )),
                cx,
            );
            f.bare = true;
            f
        });
        let typing = cx.subscribe_in(&address, window, Self::on_address);
        let focus = address.read(cx).focus.clone();
        let blur = cx.on_blur(&focus, window, |p, _, cx| p.stop_editing(cx));

        let host = Host::new(&webview);
        let all_corners = code.lock().unwrap_or_else(|e| e.into_inner()).is_some();
        #[cfg(target_os = "macos")]
        if let Some(h) = &host {
            style_native(&h.0, theme::CARD_RADIUS - 1., all_corners);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = all_corners;
        let mut pane = Self {
            import_offer: None,
            importing: false,
            webview,
            host,
            logs: VecDeque::new(),
            log_seq: 0,
            code,
            url: SharedString::default(),
            title: SharedString::default(),
            loading: !is_blank(&url),
            progress: 0.,
            can_back: false,
            can_forward: false,
            address,
            servers: Vec::new(),
            zoom: 1.0,
            bounds: None,
            tab_visible: true,
            revealed: false,
            // As made (see `build`): `sync_visibility` hides it from here.
            shown: cfg!(target_os = "linux"),
            covered: false,
            cover_shot: None,
            favicon: None,
            typed: String::new(),
            editing: false,
            completion: None,
            quiet: 0,
            select_on_click: false,
            suggest: None,
            remote: Vec::new(),
            remote_for: String::new(),
            top: Vec::new(),
            find: None,
            download: None,
            last_link_tab: None,
            watching: false,
            _watch: None,
            _reveal: None,
            _icon: None,
            _remote: None,
            _download: None,
            #[cfg(windows)]
            dialog: None,
            _events: events,
            _subs: [typing, blur],
        };
        if is_blank(&url) {
            pane.top = history::top_sites(8);
        } else {
            pane.set_url(&url, cx);
            pane.watch_load(cx);
        }
        pane
    }

    /// What this pane edits, when it is Deck's code editor.
    pub fn editor(&self) -> Option<crate::code::Editor> {
        self.code.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_editor(&self) -> bool {
        self.code.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Ask the editor page to open `path` (relative to its project), at
    /// `line`, with its changes shown when `diff`.
    pub fn editor_open(&self, path: &str, line: Option<u32>, diff: bool) {
        let opts = serde_json::json!({ "line": line, "diff": diff });
        let js = format!(
            "window.deck&&window.deck.open({},{})",
            serde_json::to_string(path).unwrap_or_default(),
            opts
        );
        let _ = self.webview.evaluate_script(&js);
    }

    /// A shortcut Deck took that the editor page acts on itself:
    /// "quickopen", "find" or "save".
    pub fn editor_key(&self, name: &str) {
        let _ = self
            .webview
            .evaluate_script(&format!("window.deck&&window.deck.key({name:?})"));
    }

    /// Show commit `hash` in the editor's History.
    pub fn editor_commit(&self, hash: &str) {
        let _ = self.webview.evaluate_script(&format!(
            "window.deck&&window.deck.commit&&window.deck.commit({})",
            serde_json::to_string(hash).unwrap_or_default()
        ));
    }

    /// Switch the editor's sidebar to "files" or "changes".
    pub fn editor_show(&self, view: &str) {
        let _ = self
            .webview
            .evaluate_script(&format!("window.deck&&window.deck.show({view:?})"));
    }

    /// Hand the editor the current theme and font after a change.
    pub fn editor_restyle(&self) {
        // On Windows every page wears the theme's scheme (macOS: the app's).
        #[cfg(windows)]
        win::set_dark(&self.webview, theme::is_dark());
        if !self.is_editor() {
            return;
        }
        let look = crate::code::look();
        let _ = self.webview.evaluate_script(&format!(
            "window.deck&&(window.deck.setTheme({}),window.deck.setFont({}))",
            look["theme"], look["font"]
        ));
    }

    /// Keep `e` in the log, numbered and stamped with when it arrived.
    fn record(&mut self, mut e: Value) {
        self.log_seq += 1;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        e["seq"] = self.log_seq.into();
        e["at"] = at.into();
        if self.logs.len() >= LOG_KEEP {
            self.logs.pop_front();
        }
        self.logs.push_back(e);
    }

    /// What the page logged and loaded, for `browser.logs`.
    pub fn logs(&mut self, q: &LogQuery) -> Value {
        let hits: Vec<&Value> = self
            .logs
            .iter()
            .filter(|e| e.get("seq").and_then(Value::as_u64).unwrap_or(0) > q.since)
            .filter(|e| q.wants(e))
            .collect();
        let more = hits.len().saturating_sub(q.limit);
        let entries: Vec<Value> = hits[more..].iter().map(|e| (*e).clone()).collect();
        let mut out = serde_json::json!({
            "url": self.url.as_ref(),
            "last_seq": self.log_seq,
            "entries": entries,
            "more": more,
        });
        // The page is stopped until it is answered.
        if let Some((kind, text)) = self.pending_dialog() {
            out["dialog"] = serde_json::json!({ "kind": kind, "text": text });
        }
        if q.clear {
            self.logs.clear();
        }
        out
    }

    #[cfg(target_os = "linux")]
    fn webview_ptr(&self) -> usize {
        std::ptr::from_ref(&self.webview) as usize
    }

    #[cfg(windows)]
    fn webview_ptr(&self) -> usize {
        win::key(&self.webview)
    }

    /// The dialog this page waits on: its kind and text.
    #[cfg(windows)]
    pub fn pending_dialog(&self) -> Option<(&'static str, String)> {
        win::pending(self.webview_ptr()).map(|(kind, text, _)| (kind, text))
    }

    /// Answer the page's dialog for an agent: OK or Cancel, with a
    /// prompt's text first in `values`.
    #[cfg(windows)]
    pub fn answer_dialog(&self, accept: bool, values: &[String]) -> Result<Value, String> {
        let (kind, text) = win::answer(self.webview_ptr(), accept, values.first().map(String::as_str))
            .ok_or("no dialog is open on this page")?;
        // The bar goes with the next render (see `on_msg`).
        let ok = accept || kind == "alert";
        Ok(serde_json::json!({ "dialog": kind, "text": text, "answer": if ok { "ok" } else { "cancel" } }))
    }

    /// Show the page's waiting dialog in the bar, or take the bar down once
    /// it has been answered.
    #[cfg(windows)]
    fn sync_dialog(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        let pending = win::pending(self.webview_ptr());
        let same = match (&pending, &self.dialog) {
            (Some((kind, text, _)), Some(d)) => *kind == d.kind && *text == d.text,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.dialog = pending.map(|(kind, text, default)| {
            let (field, sub) = if kind == "prompt" {
                let field = cx.new(|cx| {
                    let mut f = Field::new("", cx);
                    f.set_text(&default, cx);
                    f
                });
                let sub = cx.subscribe(&field, |p, _, ev: &FieldEvent, cx| match ev {
                    FieldEvent::Submit(_) => p.answer_bar(true, cx),
                    FieldEvent::Cancel => p.answer_bar(false, cx),
                    _ => {}
                });
                (Some(field), Some(sub))
            } else {
                (None, None)
            };
            PageDialog { kind, text, field, _sub: sub }
        });
        if let (Some(window), Some(field)) = (window, self.dialog.as_ref().and_then(|d| d.field.clone())) {
            window.focus(&field.read(cx).focus);
            reclaim_keyboard(window);
        }
        cx.notify();
    }

    /// The bar's OK or Cancel.
    #[cfg(windows)]
    fn answer_bar(&mut self, accept: bool, cx: &mut Context<Self>) {
        let value = self
            .dialog
            .as_ref()
            .and_then(|d| d.field.as_ref())
            .map(|f| f.read(cx).text().to_string());
        win::answer(self.webview_ptr(), accept, value.as_deref());
        self.dialog = None;
        // The next one, if the page asked again meanwhile.
        self.sync_dialog(None, cx);
        cx.notify();
    }

    #[cfg(windows)]
    fn render_dialog(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.dialog.as_ref()?;
        let button = |id: &'static str, label: &'static str, primary: bool, accept: bool| {
            div()
                .id(id)
                .flex()
                .flex_none()
                .items_center()
                .h(u(28.))
                .px(u(12.))
                .rounded(u(6.))
                .cursor_pointer()
                .when(primary, |b| b.bg(rgb(theme::text())).text_color(rgb(theme::pane())).hover(|s| s.opacity(0.88)))
                .when(!primary, |b| b.text_color(rgb(theme::text_2())).hover(|s| s.bg(rgb(theme::row_hover()))))
                .child(label)
                .on_click(cx.listener(move |p, _, _, cx| p.answer_bar(accept, cx)))
        };
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(u(10.))
                .min_h(u(40.))
                .px(u(14.))
                .py(u(6.))
                .border_t_1()
                .border_color(rgba(theme::hairline()))
                .bg(rgba(theme::tint()))
                .text_size(u(theme::TEXT_UI))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(rgb(theme::text()))
                        .child(d.text.clone()),
                )
                .children(d.field.clone().map(|f| div().w(u(220.)).flex_none().child(f)))
                .when(d.kind != "alert", |bar| bar.child(button("dialog-cancel", "Cancel", false, false)))
                .child(button("dialog-ok", "OK", true, true))
                .into_any_element(),
        )
    }

    /// On Linux the engine draws a page's dialogs itself, where an agent
    /// can't reach them yet.
    #[cfg(target_os = "linux")]
    pub fn pending_dialog(&self) -> Option<(&'static str, String)> {
        None
    }

    #[cfg(target_os = "linux")]
    pub fn answer_dialog(&self, _accept: bool, _values: &[String]) -> Result<Value, String> {
        Err("answering page dialogs is not supported on this platform yet".into())
    }

    #[cfg(target_os = "macos")]
    fn webview_ptr(&self) -> usize {
        use wry::WebViewExtMacOS;
        Retained::as_ptr(&self.webview.webview()) as usize
    }

    /// The dialog this page has up (see `ask`): its kind and text.
    #[cfg(target_os = "macos")]
    pub fn pending_dialog(&self) -> Option<(&'static str, String)> {
        let me = self.webview_ptr();
        DIALOGS.with(|d| {
            d.borrow()
                .iter()
                .find(|p| p.webview == me)
                .map(|p| (p.kind, p.text.clone()))
        })
    }

    /// Answer the page's dialog for an agent: OK or Cancel, with `values`
    /// for its fields (a prompt's text; a login's name and password).
    #[cfg(target_os = "macos")]
    pub fn answer_dialog(&self, accept: bool, values: &[String]) -> Result<Value, String> {
        use objc2_app_kit::{NSAlertFirstButtonReturn, NSAlertSecondButtonReturn};
        let me = self.webview_ptr();
        let found = DIALOGS.with(|d| {
            d.borrow()
                .iter()
                .find(|p| p.webview == me)
                .map(|p| (p.kind, p.text.clone(), p.alert.clone(), p.fields.clone()))
        });
        let (kind, text, alert, fields) = found.ok_or("no dialog is open on this page")?;
        for (f, v) in fields.iter().zip(values) {
            f.setStringValue(&objc2_foundation::NSString::from_str(v));
        }
        let sheet = alert.window();
        let parent = sheet
            .sheetParent()
            .ok_or("the dialog is waiting behind another one; answer that first")?;
        let ok = accept || kind == "alert";
        // Ends the sheet, which answers the page (see `ask`).
        parent.endSheet_returnCode(
            &sheet,
            if ok { NSAlertFirstButtonReturn } else { NSAlertSecondButtonReturn },
        );
        Ok(serde_json::json!({ "dialog": kind, "text": text, "answer": if ok { "ok" } else { "cancel" } }))
    }

    /// Where the page is in the window while it is on screen, for a
    /// picture of just this pane.
    pub fn page_bounds(&self) -> Option<Bounds<Pixels>> {
        self.bounds.filter(|_| self.shown)
    }

    fn on_msg(&mut self, msg: Msg, cx: &mut Context<Self>) {
        // Nothing on screen changes for a log line.
        let msg = match msg {
            Msg::Log(entries) => {
                for e in entries {
                    self.record(e);
                }
                return;
            }
            msg => msg,
        };
        match msg {
            Msg::Log(_) => {}
            #[cfg(windows)]
            Msg::Dialog => self.sync_dialog(None, cx),
            Msg::Title(t) => {
                history::set_title(&self.url, &t);
                self.title = t.into();
                cx.emit(BrowserEvent::Retitled);
            }
            Msg::Load(u, committed) => {
                // A new page's title has not arrived yet; the old one must
                // not be filed under the new address.
                if committed && u != self.url.as_ref() {
                    self.title = SharedString::default();
                }
                if !is_blank(&u) {
                    self.set_url(&u, cx);
                }
                if committed {
                    if !is_blank(&u) && !self.is_editor() {
                        self.record(serde_json::json!({ "t": "nav", "url": u }));
                    }
                    cx.emit(BrowserEvent::Retitled);
                    self.loading = true;
                    self.watch_load(cx);
                    self.reveal_soon(cx);
                } else {
                    history::record(&u, &self.title);
                    self.reveal(cx);
                    self.poll_load(cx);
                }
            }
            Msg::Moved => {
                if let Ok(u) = self.webview.url().map(editor_uri) {
                    if u != self.url.as_ref() && !is_blank(&u) {
                        self.set_url(&u, cx);
                        history::record(&u, "");
                        self.poll_load(cx);
                    }
                }
            }
            Msg::Focused => cx.emit(BrowserEvent::Focused),
            Msg::OpenFolder(path, terminal) => {
                if self.is_editor() && path.is_dir() {
                    cx.emit(BrowserEvent::OpenFolder { path, terminal });
                }
            }
            Msg::NewWindow(u) => cx.emit(BrowserEvent::OpenTab(u)),
            Msg::LinkTab(u) => {
                // The page can post this itself; a click is never faster
                // than this, a script flooding the channel is.
                let now = Instant::now();
                if self
                    .last_link_tab
                    .is_none_or(|t| now - t > Duration::from_millis(300))
                {
                    self.last_link_tab = Some(now);
                    cx.emit(BrowserEvent::OpenTab(u));
                }
            }
            Msg::Icon(url) => self.load_icon(url, cx),
            Msg::Key(k) => cx.emit(BrowserEvent::Key(k)),
            Msg::DownloadStarted(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Download".into());
                self._download = None;
                self.download = Some(Download {
                    name,
                    path: Some(path),
                    state: DownloadState::Running,
                });
            }
            Msg::DownloadDone(path, ok) => {
                if let Some(d) = &mut self.download {
                    d.state = if ok {
                        DownloadState::Done
                    } else {
                        DownloadState::Failed
                    };
                    if path.is_some() {
                        d.path = path;
                    }
                }
                self._download = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(8)).await;
                    let _ = this.update(cx, |p, cx| {
                        p.download = None;
                        cx.notify();
                    });
                }));
            }
        }
        cx.notify();
    }

    fn set_url(&mut self, url: &str, cx: &mut Context<Self>) {
        let host_changed = history::host(url) != history::host(&self.url);
        self.url = SharedString::from(url.to_string());
        if host_changed {
            self.favicon = favicon::get(history::host(url));
        }
        if !self.editing {
            self.set_address(url, cx);
        }
    }

    /// Put `text` in the address bar without it counting as typing.
    fn set_address(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.address.read(cx).text() != text {
            self.quiet += 1;
        }
        let shown = (!is_blank(text)).then(|| SharedString::from(pretty_url(text)));
        let text = if is_blank(text) { "" } else { text };
        self.address.update(cx, |f, cx| {
            f.set_text(text, cx);
            f.display = shown;
        });
    }

    pub fn is_blank(&self) -> bool {
        is_blank(&self.url)
    }

    pub fn favicon(&self) -> Option<Arc<Image>> {
        self.favicon.clone()
    }

    pub fn navigate(&mut self, input: &str, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let url = normalize_url(input);
        self.webview.load_url(&engine_uri(&url))?;
        self.set_url(&url, cx);
        self.loading = true;
        self.progress = 0.;
        self.sync_visibility();
        self.watch_load(cx);
        cx.notify();
        Ok(())
    }

    /// Navigate because the user asked to, and hand the keyboard to the page.
    fn open(&mut self, url: &str, cx: &mut Context<Self>) {
        self.end_suggest(cx);
        self.editing = false;
        let _ = self.navigate(url, cx);
        self.focus_native();
        cx.emit(BrowserEvent::Focused);
    }

    // -- loading --------------------------------------------------------------

    /// Follow the load through WebKit's own state while it runs: progress,
    /// back and forward. A failed load reports nothing to wry, so this is
    /// also what notices a load has stopped.
    fn watch_load(&mut self, cx: &mut Context<Self>) {
        if self.watching {
            return;
        }
        self.watching = true;
        self._watch = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            match this.update(cx, |p, cx| p.poll_load(cx)) {
                Ok(true) => {}
                _ => break,
            }
        }));
    }

    /// Read the load state; true while it is still loading.
    fn poll_load(&mut self, cx: &mut Context<Self>) -> bool {
        let (loading, progress) = self.load_state();
        let back = self.webview.can_go_back().unwrap_or(false);
        let forward = self.webview.can_go_forward().unwrap_or(false);
        if loading != self.loading {
            // The sidebar spins a loading page's icon.
            cx.emit(BrowserEvent::Retitled);
        }
        if loading != self.loading
            || (progress - self.progress).abs() > 0.01
            || back != self.can_back
            || forward != self.can_forward
        {
            self.loading = loading;
            self.progress = progress;
            self.can_back = back;
            self.can_forward = forward;
            cx.notify();
        }
        if !loading {
            self.watching = false;
            self.reveal(cx);
        }
        loading
    }

    /// Whether the page is loading, and how far along.
    #[cfg(target_os = "macos")]
    fn load_state(&self) -> (bool, f32) {
        use wry::WebViewExtMacOS;
        let wk = self.webview.webview();
        // SAFETY: plain property reads on the main thread.
        unsafe { (wk.isLoading(), wk.estimatedProgress() as f32) }
    }

    /// From WebView2's navigation events (see `win::attach`).
    #[cfg(windows)]
    fn load_state(&self) -> (bool, f32) {
        win::load_state(self.webview_ptr())
    }

    /// wry has no load progress on Linux: a page counts as loaded once
    /// it has finished (the `Load` message clears `loading`).
    #[cfg(target_os = "linux")]
    fn load_state(&self) -> (bool, f32) {
        (false, 1.)
    }

    /// The first page has committed: show it once it has had a moment to
    /// paint, or when it finishes, whichever comes first.
    fn reveal_soon(&mut self, cx: &mut Context<Self>) {
        if self.revealed {
            return;
        }
        self._reveal = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |p, cx| p.reveal(cx));
        }));
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        if !self.revealed && !self.is_blank() {
            self.revealed = true;
            self._reveal = None;
            self.sync_visibility();
            cx.notify();
        }
    }

    fn load_icon(&mut self, icon_url: String, cx: &mut Context<Self>) {
        let host = history::host(&self.url).to_string();
        if host.is_empty() {
            return;
        }
        if let Some(found) = favicon::get(&host) {
            self.favicon = Some(found);
            if !favicon::stale(&host) {
                return;
            }
        }
        self._icon = Some(cx.spawn(async move |this, cx| {
            let found = favicon::fetch(host.clone(), icon_url).await;
            let _ = this.update(cx, |p, cx| {
                if let (Some(found), true) = (found, history::host(&p.url) == host) {
                    p.favicon = Some(found);
                    cx.emit(BrowserEvent::Retitled);
                    cx.notify();
                }
            });
        }));
    }

    // -- address bar ----------------------------------------------------------

    /// Put the keyboard in the address bar with its text selected (⌘L).
    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let f = self.address.clone();
        window.focus(&f.read(cx).focus);
        reclaim_keyboard(window);
        f.update(cx, |f, cx| f.select_all(cx));
    }

    fn on_address(
        &mut self,
        _: &Entity<Field>,
        ev: &FieldEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            FieldEvent::Changed if self.quiet > 0 => self.quiet -= 1,
            FieldEvent::Changed if self.address.read(cx).focus.is_focused(window) => {
                self.on_typed(cx);
            }
            FieldEvent::PopupKey(k) => self.suggest_key(k, cx),
            FieldEvent::Submit(text) if !text.trim().is_empty() => {
                let url = match &self.completion {
                    Some((shown, url)) if shown == text => url.clone(),
                    _ => normalize_url(text),
                };
                self.open(&url, cx);
            }
            FieldEvent::Cancel => self.cancel_editing(cx),
            _ => {}
        }
    }

    /// The text changed under the user's hands: complete it inline when they
    /// added to the end, and refresh the suggestions.
    fn on_typed(&mut self, cx: &mut Context<Self>) {
        let (text, at_end) = {
            let f = self.address.read(cx);
            (f.text().to_string(), f.caret_at_end())
        };
        // Growing the text completes it; deleting (⌫ over the completion)
        // must not bring the completion straight back.
        let grew = text.len() > self.typed.len() && text.starts_with(&self.typed);
        self.typed = text.clone();
        self.editing = true;
        self.completion = None;
        if grew && at_end {
            if let Some((shown, url)) = history::complete(&text) {
                if let Some(rest) = shown.get(text.len()..).map(str::to_string) {
                    self.address.update(cx, |f, cx| f.complete_with(&rest, cx));
                    self.completion = Some((shown, url));
                }
            }
        }
        self.refresh_suggest(false, cx);
        self.fetch_remote(cx);
    }

    fn refresh_suggest(&mut self, keep_selection: bool, cx: &mut Context<Self>) {
        let typed = self.typed.trim().to_string();
        if typed.is_empty() {
            self.end_suggest(cx);
            return;
        }
        let mut rows = Vec::new();
        match &self.completion {
            Some((shown, url)) => rows.push(Row::Go {
                url: url.clone(),
                label: shown.clone(),
            }),
            None if looks_like_address(&typed) => rows.push(Row::Go {
                url: normalize_url(&typed),
                label: typed.clone(),
            }),
            None => rows.push(Row::Search(typed.clone())),
        }
        let first_url = match &rows[0] {
            Row::Go { url, .. } => Some(history::strip(url).trim_end_matches('/').to_string()),
            _ => None,
        };
        if first_url.is_some() && !typed.contains("://") {
            rows.push(Row::Search(typed.clone()));
        }
        let lower = typed.to_lowercase();
        for (port, process) in &self.servers {
            if format!("localhost:{port}").contains(&lower) {
                rows.push(Row::Server(*port, process.clone()));
            }
        }
        for v in history::search(&typed, 6)
            .into_iter()
            .filter(|v| first_url.as_deref() != Some(history::strip(&v.url).trim_end_matches('/')))
            .take(4)
        {
            rows.push(Row::Page(v));
        }
        if self.remote_for == typed {
            for s in self
                .remote
                .iter()
                .filter(|s| !s.eq_ignore_ascii_case(&typed))
                .take(5)
            {
                rows.push(Row::Search(s.clone()));
            }
        }
        let selected = match &self.suggest {
            Some(s) if keep_selection => s.selected.min(rows.len() - 1),
            _ => 0,
        };
        self.suggest = Some(Suggest { rows, selected });
        self.address.update(cx, |f, _| f.popup = true);
        self.sync_visibility();
        cx.notify();
    }

    /// Ask Google for suggestions a beat after typing pauses.
    fn fetch_remote(&mut self, cx: &mut Context<Self>) {
        let q = self.typed.trim().to_string();
        if q.is_empty() || q.contains("://") || q.starts_with("localhost") {
            self._remote = None;
            return;
        }
        self._remote = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(90))
                .await;
            let found = crate::net::suggestions(&q).await;
            let _ = this.update(cx, |p, cx| {
                if p.typed.trim() == q && p.suggest.is_some() {
                    p.remote = found;
                    p.remote_for = q;
                    p.refresh_suggest(true, cx);
                }
            });
        }));
    }

    fn suggest_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(s) = self.suggest.as_mut() else {
            return;
        };
        let n = s.rows.len();
        match key {
            "up" => s.selected = (s.selected + n - 1) % n,
            "down" => s.selected = (s.selected + 1) % n,
            // ⇥ takes the inline completion, as in Safari; past that it
            // walks the list.
            "tab" if self.completion.is_some() && s.selected == 0 => {
                self.address.update(cx, |f, cx| f.complete_with("", cx));
                if let Some((shown, _)) = &self.completion {
                    self.typed = shown.clone();
                }
            }
            "tab" => s.selected = (s.selected + 1) % n,
            "enter" => {
                let ix = s.selected;
                self.open_row(ix, cx);
                return;
            }
            "escape" => {
                self.cancel_editing(cx);
                return;
            }
            _ => {}
        }
        cx.notify();
    }

    fn open_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.suggest.as_ref().and_then(|s| s.rows.get(ix)).cloned() else {
            return;
        };
        let url = match row {
            Row::Go { url, .. } => url,
            Row::Search(q) => search_url(&q),
            Row::Page(v) => v.url,
            Row::Server(port, _) => format!("http://localhost:{port}"),
        };
        self.open(&url, cx);
    }

    fn end_suggest(&mut self, cx: &mut Context<Self>) {
        self.suggest = None;
        self._remote = None;
        self.address.update(cx, |f, _| f.popup = false);
        self.sync_visibility();
        cx.notify();
    }

    /// The address bar lost the keyboard: put the address back.
    fn stop_editing(&mut self, cx: &mut Context<Self>) {
        self.end_suggest(cx);
        self.typed.clear();
        self.completion = None;
        if self.editing {
            self.editing = false;
            let url = self.url.clone();
            self.set_address(&url, cx);
        }
    }

    /// Esc: put the address back and give the page the keyboard again.
    fn cancel_editing(&mut self, cx: &mut Context<Self>) {
        self.editing = true;
        self.stop_editing(cx);
        if !self.is_blank() {
            self.focus_native();
            cx.emit(BrowserEvent::Focused);
        }
    }

    // -- find in page ---------------------------------------------------------

    /// ⌘F: a find bar under the toolbar.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_blank() {
            return;
        }
        let field = match &self.find {
            Some(f) => f.field.clone(),
            None => {
                let field = cx.new(|cx| {
                    let mut f = Field::new("Find in page", cx);
                    // Enter, ↑↓ and Esc come to the pane.
                    f.popup = true;
                    f
                });
                let sub = cx.subscribe(&field, |p, _, ev: &FieldEvent, cx| match ev {
                    FieldEvent::Changed => p.find_run(false, true, cx),
                    FieldEvent::PopupKey("enter") | FieldEvent::PopupKey("down") => {
                        p.find_run(false, false, cx)
                    }
                    // ⇧⏎ arrives as a plain submit.
                    FieldEvent::Submit(_) | FieldEvent::PopupKey("up") => {
                        p.find_run(true, false, cx)
                    }
                    FieldEvent::PopupKey("escape") | FieldEvent::Cancel => p.close_find(cx),
                    _ => {}
                });
                self.find = Some(FindBar {
                    field: field.clone(),
                    matches: None,
                    _sub: sub,
                });
                field
            }
        };
        window.focus(&field.read(cx).focus);
        reclaim_keyboard(window);
        field.update(cx, |f, cx| f.select_all(cx));
        cx.notify();
    }

    /// ⌘G / ⇧⌘G while the find bar is open.
    pub fn find_step(&mut self, back: bool, cx: &mut Context<Self>) -> bool {
        if self.find.is_none() {
            return false;
        }
        self.find_run(back, false, cx);
        true
    }

    /// WebKit's own find moves the selection to the match and scrolls to it;
    /// the count is of the page's visible text.
    fn find_run(&mut self, back: bool, restart: bool, cx: &mut Context<Self>) {
        let Some(f) = &self.find else {
            return;
        };
        let q = serde_json::to_string(f.field.read(cx).text()).unwrap_or_default();
        let js = format!(
            "(() => {{ const q = {q}; const s = getSelection(); \
             if ({restart} || !q) s.removeAllRanges(); if (!q) return null; \
             window.find(q, false, {back}, true, false, false, false); \
             const t = (document.body ? document.body.innerText : '').toLowerCase(); \
             const l = q.toLowerCase(); let n = 0, i = 0; \
             while ((i = t.indexOf(l, i)) !== -1) {{ n++; i += l.length; }} return n; }})()"
        );
        let (tx, rx) = futures::channel::oneshot::channel();
        self.eval(&js, move |r| {
            let _ = tx.send(r);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(v)) = rx.await {
                let _ = this.update(cx, |p, cx| {
                    if let Some(f) = &mut p.find {
                        f.matches = v.as_u64().map(|n| n as usize);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn close_find(&mut self, cx: &mut Context<Self>) {
        let _ = self
            .webview
            .evaluate_script("getSelection().removeAllRanges()");
        self.find = None;
        self.focus_native();
        cx.emit(BrowserEvent::Focused);
        cx.notify();
    }

    // -- the native view ------------------------------------------------------

    pub fn focus_native(&self) {
        if self.shown {
            let _ = self.webview.focus();
        }
    }

    /// Hidden tabs must hide their webview: it is a native view and would
    /// otherwise stay on screen over whatever tab is showing.
    pub fn set_visible(&mut self, visible: bool) {
        self.tab_visible = visible;
        self.sync_visibility();
    }

    fn sync_visibility(&mut self) {
        let show = self.tab_visible
            && self.revealed
            && !self.is_blank()
            && self.suggest.is_none()
            && !self.covered;
        if show != self.shown {
            self.shown = show;
            // On screen with the frame being drawn, not a refresh apart.
            gpui::sync_next_present();
            // The host, so a docked inspector hides with its page.
            match &self.host {
                Some(h) => h.set_hidden(!show),
                // A WebKitGTK page hidden through GTK comes back a 1x1
                // window, so on Linux it is parked off the window instead.
                None if cfg!(target_os = "linux") => self.place_native(),
                // WebView2 hides cleanly, and stops painting while hidden;
                // placed first, so it shows where the pane is now.
                None if cfg!(windows) => {
                    self.place_native();
                    let _ = self.webview.set_visible(show);
                }
                None => {
                    let _ = self.webview.set_visible(show);
                }
            }
        }
    }

    /// Something is drawn over the page. Its picture goes in its place
    /// first, then the live page is hidden, so the pane looks the same
    /// instead of going blank; uncovered, the live page is back.
    pub fn set_covered(&mut self, covered: bool, cx: &mut Context<Self>) {
        if covered == self.covered {
            return;
        }
        if !covered || !self.shown {
            self.covered = covered;
            self.cover_shot = None;
            self.sync_visibility();
            cx.notify();
            return;
        }
        #[cfg(target_os = "macos")]
        let rx = { use wry::WebViewExtMacOS; snapshot(&self.webview.webview()) };
        #[cfg(windows)]
        let rx = win::snapshot(&self.webview);
        // No picture of the page on Linux: it hides with nothing in its place.
        #[cfg(target_os = "linux")]
        let rx = futures::channel::oneshot::channel::<Option<Vec<u8>>>().1;
        cx.spawn(async move |this, cx| {
            let shot = rx.await.ok().flatten();
            let _ = this.update(cx, |p, cx| {
                // Uncovered again before the picture came: nothing to do.
                p.cover_shot = shot.map(|png| std::sync::Arc::new(Image::from_bytes(gpui::ImageFormat::Png, png)));
                p.covered = true;
                p.sync_visibility();
                cx.notify();
            });
        })
        .detach();
    }

    pub fn set_zoom(&mut self, zoom: f64) {
        if (self.zoom - zoom).abs() > f64::EPSILON {
            self.zoom = zoom;
            let _ = self.webview.zoom(zoom);
        }
    }

    pub fn back(&self) {
        let _ = self.webview.go_back();
    }

    pub fn forward(&self) {
        let _ = self.webview.go_forward();
    }

    pub fn reload(&mut self) {
        if self.loading {
            #[cfg(target_os = "macos")]
            {
                use wry::WebViewExtMacOS;
                // SAFETY: main thread, a live view.
                unsafe { self.webview.webview().stopLoading() };
            }
            #[cfg(windows)]
            win::stop(self.webview_ptr());
            #[cfg(target_os = "linux")]
            let _ = self.webview.evaluate_script("window.stop()");
            self.loading = false;
        } else if !self.is_blank() {
            let _ = self.webview.reload();
            // It starts asynchronously; its commit restarts the watch.
            self.loading = true;
        }
    }

    /// ⌥⌘I: WebKit's inspector, docked to the page.
    pub fn toggle_devtools(&self) {
        if self.webview.is_devtools_open() {
            self.webview.close_devtools();
        } else {
            self.webview.open_devtools();
        }
    }

    /// Run `js` (an expression or statements) and hand back its JSON value.
    ///
    /// The result is stringified in the page so DOM nodes and cycles degrade
    /// to `{}` instead of failing the bridge, and exceptions come back as
    /// errors instead of never calling back.
    pub fn eval(&self, js: &str, done: impl FnOnce(Result<Value, String>) + Send + 'static) {
        let src = serde_json::to_string(js).unwrap_or_default();
        self.run_js(&format!("(0, eval)({src})"), done);
    }

    /// Like `eval`, for one expression, run as written instead of through
    /// `eval()`, so it also runs on pages whose Content Security Policy
    /// forbids `unsafe-eval`. Deck's page actions (`browse.rs`) use this.
    pub fn eval_expr(&self, js: &str, done: impl FnOnce(Result<Value, String>) + Send + 'static) {
        self.run_js(&format!("({js}\n)"), done);
    }

    fn run_js(&self, expr: &str, done: impl FnOnce(Result<Value, String>) + Send + 'static) {
        let wrapped = format!(
            "(() => {{ try {{ const v = {expr}; \
             return JSON.stringify({{ ok: v === undefined ? null : v }}); }} \
             catch (e) {{ return JSON.stringify({{ err: String(e) }}); }} }})()"
        );
        let done = Mutex::new(Some(done));
        // On failure the callback is dropped unrun, which drops `done`; the
        // caller sees its reply channel close and reports that.
        let _ = self
            .webview
            .evaluate_script_with_callback(&wrapped, move |json| {
                if let Some(done) = done.lock().ok().and_then(|mut d| d.take()) {
                    done(decode_eval(&json));
                }
            });
    }

    pub fn read_text(&self, done: impl FnOnce(Result<Value, String>) + Send + 'static) {
        self.eval(PAGE_TEXT, done);
    }

    /// A page nobody is looking at (a background page) still lays out at a
    /// desktop window's size, so what a script reads is what a person
    /// would see. Its next place on screen replaces this.
    pub fn size_offscreen(&mut self) {
        let b = Bounds::new(gpui::point(gpui::px(0.), gpui::px(0.)), gpui::size(gpui::px(1280.), gpui::px(900.)));
        self.bounds = None;
        match &self.host {
            Some(h) => h.place(b),
            None if !cfg!(target_os = "macos") => {
                let _ = self.webview.set_bounds(rect(Bounds::new(gpui::point(gpui::px(-20000.), gpui::px(-20000.)), b.size)));
            }
            None => {
                let _ = self.webview.set_bounds(rect(b));
            }
        }
    }

    fn sync_bounds(&mut self, bounds: Bounds<Pixels>) {
        if self.bounds == Some(bounds) {
            return;
        }
        self.bounds = Some(bounds);
        // Moved in step with the pixels around it (a resize, a divider).
        gpui::sync_next_present();
        match &self.host {
            Some(h) => h.place(bounds),
            None if !cfg!(target_os = "macos") => self.place_native(),
            None => {
                let _ = self.webview.set_bounds(rect(bounds));
            }
        }
    }

    /// Off macOS: the page where it belongs while it shows, else parked
    /// outside the window at its size (so a script reading it still sees
    /// a page that size). See `sync_visibility`.
    fn place_native(&self) {
        let size = self
            .bounds
            .map(|b| b.size)
            .unwrap_or(gpui::size(gpui::px(1280.), gpui::px(900.)));
        let b = match self.bounds {
            // Inside the card's hairline, which a native window would cover.
            Some(b) if self.shown => Bounds::new(
                gpui::point(b.origin.x + gpui::px(1.), b.origin.y),
                gpui::size(b.size.width - gpui::px(2.), b.size.height - gpui::px(1.)),
            ),
            _ => Bounds::new(gpui::point(gpui::px(-20000.), gpui::px(-20000.)), size),
        };
        let _ = self.webview.set_bounds(rect(b));
        // Its corners cut to the card's curve (macOS rounds the view's layer
        // instead, in `style_native`). The editor has no toolbar above it,
        // so its top corners are the card's too.
        #[cfg(target_os = "linux")]
        if self.shown {
            crate::x11::round_page(&self.webview, (theme::CARD_RADIUS - 1.) as u32, self.is_editor());
        }
        #[cfg(windows)]
        if self.shown {
            win::round(&self.webview, f32::from(b.size.width), theme::CARD_RADIUS - 1., self.is_editor());
        }
    }

    // -- rendering ------------------------------------------------------------

    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.address.read(cx).focus.is_focused(window);
        let nav = |id: &'static str, glyph: Icon, enabled: bool| {
            icon_button(id, glyph).when(!enabled, |d| d.opacity(0.35))
        };
        let leading: AnyElement = match (&self.favicon, editing || self.is_blank()) {
            (Some(img_), false) => site_image(img_.clone(), 14.),
            _ => icon(Icon::Search, 14., theme::dim()),
        };
        let pill = div()
            .id("address")
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(u(8.))
            .h(u(26.))
            .pl(u(10.))
            .rounded_full()
            // Part of the capsule at rest; raised with a thin edge only
            // while you type in it.
            .when(!editing, |d| {
                d.border_1()
                    .border_color(rgba(0xffffff00))
                    .hover(|s| s.bg(rgb(theme::surface())))
            })
            .when(editing, |d| {
                d.bg(rgb(theme::raised())).border_1().border_color(rgba(theme::focus_edge()))
            })
            .cursor_text()
            // The first click into the address selects all of it, as in
            // Safari; the unfocused text is a shortened address anyway, so a
            // caret placed in it would land in the wrong spot.
            .capture_any_mouse_down(cx.listener(|p, _, window, cx| {
                p.select_on_click = !p.address.read(cx).focus.is_focused(window);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|p, _, window, cx| {
                    if std::mem::take(&mut p.select_on_click) {
                        p.focus_address(window, cx);
                    }
                }),
            )
            .child(leading)
            .child(self.address.clone());

        let download = self.download.as_ref().map(|d| {
            let path = d.path.clone();
            div()
                .id("download")
                .flex()
                .flex_none()
                .items_center()
                .gap(u(6.))
                .h(u(26.))
                .ml(u(4.))
                .px(u(8.))
                .rounded(u(6.))
                .text_size(u(12.))
                .text_color(rgb(theme::text_2()))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                .child(match d.state {
                    DownloadState::Running => spinner("download-spin", 12., theme::text_2()),
                    DownloadState::Done => icon(Icon::Download, 13., theme::text()),
                    DownloadState::Failed => icon(Icon::Close, 13., theme::dim()),
                })
                .child(div().max_w(u(150.)).one_line().child(d.name.clone()))
                .on_click(move |_, _, _| {
                    // Show it in Finder, selected.
                    if let Some(p) = &path {
                        let _ = std::process::Command::new("/usr/bin/open")
                            .arg("-R")
                            .arg(p)
                            .spawn();
                    }
                })
        });

        // One floating capsule over the page, as dev toolbars float: the
        // controls, the address, close; nothing else.
        let capsule = div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .max_w(u(820.))
            .items_center()
            .gap(u(2.))
            .h(u(32.))
            .px(u(3.))
            .rounded_full()
            .bg(rgb(theme::menu()))
            .border_1()
            .border_color(rgba(theme::alpha(theme::text(), 0x14)))
            .shadow_md()
            .child(
                nav("back", Icon::ArrowLeft, self.can_back)
                    .on_click(cx.listener(|p, _, _, _| p.back())),
            )
            .child(
                nav("forward", Icon::ArrowRight, self.can_forward)
                    .on_click(cx.listener(|p, _, _, _| p.forward())),
            )
            .child(
                icon_button(
                    "reload",
                    if self.loading {
                        Icon::Close
                    } else {
                        Icon::Refresh
                    },
                )
                .on_click(cx.listener(|p, _, _, cx| {
                    p.reload();
                    cx.notify();
                })),
            )
            .child(pill)
            .children(download)
            .child(
                // Not `icon_button`: that sets a hover style already, and an
                // element takes only one (gpui asserts on a second).
                div()
                    .id("close-pane")
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(u(26.))
                    .rounded(u(6.))
                    .text_color(rgb(theme::text_2()))
                    .cursor_pointer()
                    .child(icon(Icon::Close, 15., theme::text_2()))
                    .ml(u(2.))
                    .opacity(0.7)
                    .hover(|s| s.opacity(1.).bg(rgb(theme::surface())).text_color(rgb(theme::text())))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(BrowserEvent::Close))),
            )
            // Real progress along the toolbar's bottom edge while loading.
            .when(self.loading && !self.is_blank(), |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .h(u(2.))
                        .w(relative(self.progress.clamp(0.08, 1.)))
                        .rounded(u(1.))
                        .bg(rgba(theme::alpha(theme::text(), 0xcc))),
                )
            });
        div()
            .flex()
            .flex_none()
            .justify_center()
            .items_center()
            .h(u(theme::TOOLBAR_PX))
            .px(u(8.))
            .bg(rgb(theme::pane()))
            .child(capsule)
    }

    fn render_find(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let f = self.find.as_ref()?;
        let q_empty = f.field.read(cx).text().is_empty();
        let count = match (q_empty, f.matches) {
            (true, _) | (_, None) => String::new(),
            (false, Some(0)) => "No matches".into(),
            (false, Some(1)) => "1 match".into(),
            (false, Some(n)) => format!("{n} matches"),
        };
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(u(2.))
                .h(u(40.))
                .px(u(6.))
                .bg(rgb(theme::pane()))
                .border_t_1()
                .border_color(rgba(theme::hairline()))
                .child(
                    div()
                        .w(u(PILL_LEFT - 6.))
                        .flex()
                        .justify_end()
                        .pr(u(8.))
                        .child(icon(Icon::Search, 14., theme::dim())),
                )
                .child(f.field.clone())
                .child(
                    div()
                        .flex_none()
                        .min_w(u(78.))
                        .px(u(8.))
                        .text_size(u(12.))
                        .text_color(rgb(theme::dim()))
                        .child(count),
                )
                .child(
                    icon_button("find-up", Icon::ChevronUp)
                        .on_click(cx.listener(|p, _, _, cx| p.find_run(true, false, cx))),
                )
                .child(
                    icon_button("find-down", Icon::ChevronDown)
                        .on_click(cx.listener(|p, _, _, cx| p.find_run(false, false, cx))),
                )
                .child(
                    icon_button("find-close", Icon::Close)
                        .on_click(cx.listener(|p, _, _, cx| p.close_find(cx))),
                ),
        )
    }

    fn render_suggest(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .flex()
            .flex_col()
            .gap(u(1.))
            .pt(u(6.))
            .pl(u(PILL_LEFT))
            .pr(u(PILL_RIGHT));
        if let Some(s) = &self.suggest {
            for (i, row) in s.rows.iter().enumerate() {
                let selected = i == s.selected;
                let (lead, primary, secondary): (AnyElement, String, Option<String>) = match row {
                    Row::Go { url, label } => (site_icon(url, Icon::Globe), label.clone(), None),
                    Row::Search(q) => (
                        icon(Icon::Search, 14., theme::text_2()),
                        q.clone(),
                        Some(format!("Search {}", search_engine().name())),
                    ),
                    Row::Page(v) => {
                        let addr = pretty_url(&v.url);
                        if v.title.is_empty() {
                            (site_icon(&v.url, Icon::Clock), addr, None)
                        } else {
                            (site_icon(&v.url, Icon::Clock), v.title.clone(), Some(addr))
                        }
                    }
                    Row::Server(port, process) => (
                        icon(Icon::Terminal, 14., theme::text_2()),
                        format!("localhost:{port}"),
                        Some(process.clone()),
                    ),
                };
                list = list.child(
                    div()
                        .id(ElementId::NamedInteger("suggest".into(), i as u64))
                        .flex()
                        .items_center()
                        .gap(u(10.))
                        .h(u(34.))
                        .px(u(10.))
                        .rounded(u(8.))
                        .text_size(u(13.))
                        .cursor_pointer()
                        .when(selected, |d| d.bg(rgb(theme::surface())))
                        .when(!selected, |d| d.hover(|s| s.bg(rgb(theme::row_hover()))))
                        .child(
                            div()
                                .flex_none()
                                .w(u(16.))
                                .flex()
                                .justify_center()
                                .child(lead),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_w_0()
                                .items_center()
                                .gap(u(8.))
                                .child(
                                    div()
                                        .flex_shrink()
                                        .min_w_0()
                                        .one_line()
                                        .text_color(rgb(theme::text()))
                                        .child(primary),
                                )
                                .children(secondary.map(|s| {
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .one_line()
                                        .text_size(u(12.))
                                        .text_color(rgb(theme::dim()))
                                        .child(s)
                                })),
                        )
                        .when(selected, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(u(12.))
                                    .text_color(rgb(theme::dim()))
                                    .child("↵"),
                            )
                        })
                        .on_click(cx.listener(move |p, _, _, cx| p.open_row(i, cx))),
                );
            }
        }
        div().size_full().bg(rgb(theme::pane())).child(list)
    }

    /// The new tab page: the sites you go back to, and the dev servers your
    /// terminals are running.
    fn render_blank(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = |text: &'static str| {
            div()
                .pl(u(4.))
                .pb(u(8.))
                .text_size(u(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::dim()))
                .child(text)
        };
        let mut col = div()
            .flex()
            .flex_col()
            .gap(u(28.))
            .w(u(536.))
            .max_w(relative(0.9));

        if !self.top.is_empty() {
            // Rows of four, built as rows: a wrapping flex box is measured
            // as one line, so its tiles stacked and ran over what followed.
            let mut tiles: Vec<AnyElement> = Vec::new();
            for (i, v) in self.top.iter().enumerate() {
                let url = v.url.clone();
                let host = history::host(&v.url).to_string();
                let mark: AnyElement = match favicon::get(&host) {
                    Some(img_) => site_image(img_, 22.),
                    None => div()
                        .text_size(u(17.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::text_2()))
                        .child(
                            host.chars()
                                .find(|c| c.is_alphanumeric())
                                .map(|c| c.to_uppercase().to_string())
                                .unwrap_or_default(),
                        )
                        .into_any_element(),
                };
                tiles.push(
                    div()
                        .id(ElementId::NamedInteger("top".into(), i as u64))
                        .group("tile")
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(u(8.))
                        .w(u(130.))
                        .py(u(12.))
                        .rounded(u(10.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::row_hover())))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(u(44.))
                                .rounded(u(12.))
                                .bg(rgb(theme::surface()))
                                .border_1()
                                .border_color(rgba(theme::hairline()))
                                .child(mark),
                        )
                        .child(
                            div()
                                .w(u(112.))
                                .one_line()
                                .text_center()
                                .text_size(u(12.))
                                .text_color(rgb(theme::text_2()))
                                .child(host),
                        )
                        .on_click(cx.listener(move |p, _, _, cx| p.open(&url, cx)))
                        .into_any_element(),
                );
            }
            let mut grid = div().flex().flex_col().gap(u(4.));
            let mut tiles = tiles.into_iter().peekable();
            while tiles.peek().is_some() {
                grid = grid.child(div().flex().gap(u(4.)).children(tiles.by_ref().take(4)));
            }
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .child(label("Frequently visited"))
                    .child(grid),
            );
        }

        if !self.servers.is_empty() {
            let mut rows = div().flex().flex_col().gap(u(1.));
            for (port, process) in &self.servers {
                let url = format!("http://localhost:{port}");
                rows = rows.child(
                    div()
                        .id(ElementId::NamedInteger("server".into(), *port as u64))
                        .flex()
                        .items_center()
                        .gap(u(10.))
                        .h(u(36.))
                        .px(u(10.))
                        .rounded(u(8.))
                        .text_size(u(13.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::row_hover())))
                        .child(icon(Icon::Terminal, 14., theme::text_2()))
                        .child(
                            div()
                                .text_color(rgb(theme::text()))
                                .child(format!("localhost:{port}")),
                        )
                        .child(
                            div()
                                .text_size(u(12.))
                                .text_color(rgb(theme::dim()))
                                .child(process.clone()),
                        )
                        .child(div().flex_1())
                        .child(icon(Icon::ArrowUpRight, 13., theme::dim()))
                        .on_click(cx.listener(move |p, _, _, cx| p.open(&url, cx))),
                );
            }
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .child(label("Running on this Mac"))
                    .child(rows),
            );
        }

        if self.top.is_empty() && self.servers.is_empty() {
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(u(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(u(48.))
                            .rounded(u(24.))
                            .bg(rgb(theme::surface()))
                            .child(icon(Icon::Search, 20., theme::text_2())),
                    )
                    .child(
                        div()
                            .text_size(u(14.))
                            .text_color(rgb(theme::text_2()))
                            .child(format!(
                                "Search {} or enter an address",
                                search_engine().name()
                            )),
                    )
                    .child(
                        div()
                            .text_size(u(12.))
                            .text_color(rgb(theme::dim()))
                            .child("Dev servers you start in a terminal show up here"),
                    ),
            );
        }

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(theme::pane()))
            .child(col)
    }
}

impl Render for BrowserPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        // The editor draws its own chrome; the page is the whole pane.
        if self.is_editor() {
            return div()
                .size_full()
                .bg(rgb(theme::pane()))
                .child(
                    canvas(
                        move |bounds, _, cx| entity.update(cx, |p, _| p.sync_bounds(bounds)),
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element();
        }
        let toolbar = self.render_toolbar(window, cx);
        let find = self.render_find(cx);
        #[cfg(windows)]
        let dialog = self.render_dialog(cx);
        #[cfg(not(windows))]
        let dialog: Option<AnyElement> = None;
        let body = if self.suggest.is_some() {
            self.render_suggest(cx).into_any_element()
        } else if self.is_blank() {
            self.render_blank(cx).into_any_element()
        } else {
            div()
                .relative()
                .size_full()
                .bg(rgb(theme::pane()))
                .children(self.cover_shot.clone().filter(|_| self.covered).map(|shot| {
                    img(shot).absolute().top_0().left_0().size_full().object_fit(gpui::ObjectFit::Fill)
                }))
                .child(
                    canvas(
                        move |bounds, _, cx| entity.update(cx, |p, _| p.sync_bounds(bounds)),
                        |_, _, _, _| {},
                    )
                    .size_full(),
                )
                .into_any_element()
        };

        // Once, until answered: bring sign-ins and history over from the
        // browser you use, so sites open signed in here.
        let offer = self.import_offer.clone().filter(|_| !self.is_editor()).map(|from| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(u(10.))
                .h(u(40.))
                .px(u(14.))
                .border_t_1()
                .border_color(rgba(theme::hairline()))
                .bg(rgba(theme::tint()))
                .text_size(u(theme::TEXT_UI))
                .child(icon(Icon::Import, 14., theme::accent()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .one_line()
                        .text_color(rgb(theme::text_2()))
                        .child(format!("Sign in less: bring your sign-ins, history and bookmarks over from {from}.")),
                )
                .child(
                    div()
                        .id("import-later")
                        .flex()
                        .items_center()
                        .h(u(28.))
                        .px(u(10.))
                        .rounded(u(6.))
                        .cursor_pointer()
                        .text_color(rgb(theme::text_2()))
                        .hover(|s| s.bg(rgb(theme::row_hover())))
                        .child("Not now")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(BrowserEvent::DismissImport))),
                )
                .child(
                    div()
                        .id("import-now")
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .h(u(28.))
                        .px(u(12.))
                        .rounded(u(6.))
                        .cursor_pointer()
                        .bg(rgb(theme::text()))
                        .text_color(rgb(theme::pane()))
                        .hover(|s| s.opacity(0.88))
                        .when(self.importing, |d| d.child(crate::icon::spinner("importing", 12., theme::pane())))
                        .child(if self.importing { "Importing…" } else { "Import" })
                        .when(!self.importing, |d| d.on_click(cx.listener(|_, _, _, cx| cx.emit(BrowserEvent::Import)))),
                )
        });
        div()
            .flex()
            .flex_col()
            .size_full()
            // The toolbar is set apart by tone, not a line: a site's own
            // header border sat right under ours, two edges in a row.
            .child(div().flex_none().bg(rgba(theme::alpha(theme::text(), 0x05))).child(toolbar))
            .children(offer)
            .children(dialog)
            .children(find)
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

/// A PNG of what the web view shows now, from WebKit, delivered once.
#[cfg(target_os = "macos")]
fn snapshot(wk: &objc2::runtime::AnyObject) -> futures::channel::oneshot::Receiver<Option<Vec<u8>>> {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));
    let done = block2::RcBlock::new(move |image: *mut AnyObject, _err: *mut AnyObject| {
        // SAFETY: WebKit calls back on the main thread with an NSImage or nil.
        let png = unsafe {
            image.as_ref().and_then(|image| {
                let tiff: Option<Retained<AnyObject>> = msg_send![image, TIFFRepresentation];
                let rep: Option<Retained<AnyObject>> = msg_send![class!(NSBitmapImageRep), imageRepWithData: &*tiff?];
                let props: Retained<AnyObject> = msg_send![class!(NSDictionary), dictionary];
                // NSBitmapImageFileTypePNG
                let data: Option<Retained<AnyObject>> = msg_send![&*rep?, representationUsingType: 4usize, properties: &*props];
                let data = data?;
                let len: usize = msg_send![&*data, length];
                let ptr: *const u8 = msg_send![&*data, bytes];
                (!ptr.is_null() && len > 0).then(|| std::slice::from_raw_parts(ptr, len).to_vec())
            })
        };
        if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
            let _ = tx.send(png);
        }
    });
    // SAFETY: a plain WebKit call with a completion block it copies.
    unsafe {
        let _: () = msg_send![wk, takeSnapshotWithConfiguration: std::ptr::null::<AnyObject>(), completionHandler: &*done];
    }
    rx
}

/// A site's icon at `size`, or `fallback` when deck has none for it yet.
fn site_icon(url: &str, fallback: Icon) -> AnyElement {
    match favicon::get(history::host(url)) {
        Some(img_) => site_image(img_, 14.),
        None => icon(fallback, 14., theme::text_2()),
    }
}

pub fn site_image(image: Arc<Image>, size: f32) -> AnyElement {
    img(image)
        .size(u(size))
        .flex_none()
        .rounded(u(size / 5.))
        .into_any_element()
}

// -- page dialogs ----------------------------------------------------------

/// WKWebView shows nothing for `alert`, `confirm`, `prompt` or an HTTP login
/// unless its delegates answer them, and wry's do not: `confirm()` came back
/// false and a password-protected page just failed. Their classes learn
/// them once; each webview then sets its delegates again, since WebKit
/// notes what a delegate can do only when it is set.
#[cfg(target_os = "macos")]
fn add_page_dialogs(webview: &wry::WebView) {
    use objc2::runtime::{AnyClass, AnyObject, Imp};
    use objc2::{msg_send, sel};
    use wry::WebViewExtMacOS;
    static UI: std::sync::Once = std::sync::Once::new();
    static NAV: std::sync::Once = std::sync::Once::new();
    let wk = webview.webview();
    // SAFETY: main thread; the delegates are plain property reads.
    let ui: Option<Retained<AnyObject>> = unsafe { msg_send![&*wk, UIDelegate] };
    let nav: Option<Retained<AnyObject>> = unsafe { msg_send![&*wk, navigationDelegate] };
    let add = |obj: &AnyObject, name, imp: Imp, types: &std::ffi::CStr| {
        let cls = obj.class() as *const AnyClass as *mut AnyClass;
        // SAFETY: each function has the signature its selector and type
        // encoding describe; a method the class already has is kept.
        unsafe { objc2::ffi::class_addMethod(cls, name, imp, types.as_ptr()) };
    };
    if let Some(ui) = &ui {
        UI.call_once(|| unsafe {
            add(ui, sel!(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:),
                std::mem::transmute::<AlertFn, Imp>(js_alert), c"v@:@@@@?");
            add(ui, sel!(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:),
                std::mem::transmute::<ConfirmFn, Imp>(js_confirm), c"v@:@@@@?");
            add(ui, sel!(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:),
                std::mem::transmute::<PromptFn, Imp>(js_prompt), c"v@:@@@@@?");
        });
        popup::teach(ui);
        // SAFETY: the same delegate, which wry keeps alive.
        let _: () = unsafe { msg_send![&*wk, setUIDelegate: &**ui] };
    }
    if let Some(nav) = &nav {
        NAV.call_once(|| unsafe {
            add(nav, sel!(webView:didReceiveAuthenticationChallenge:completionHandler:),
                std::mem::transmute::<AuthFn, Imp>(http_login), c"v@:@@@?");
        });
        // SAFETY: as above.
        let _: () = unsafe { msg_send![&*wk, setNavigationDelegate: &**nav] };
    }
}

#[cfg(target_os = "macos")]
type Obj = *mut objc2::runtime::AnyObject;
#[cfg(target_os = "macos")]
type Str = *mut objc2_foundation::NSString;
#[cfg(target_os = "macos")]
type Sel = objc2::runtime::Sel;
#[cfg(target_os = "macos")]
type Block<F> = *mut block2::Block<F>;
#[cfg(target_os = "macos")]
type AlertFn = unsafe extern "C-unwind" fn(Obj, Sel, Obj, Str, Obj, Block<dyn Fn()>);
#[cfg(target_os = "macos")]
type ConfirmFn = unsafe extern "C-unwind" fn(Obj, Sel, Obj, Str, Obj, Block<dyn Fn(objc2::runtime::Bool)>);
#[cfg(target_os = "macos")]
type PromptFn = unsafe extern "C-unwind" fn(Obj, Sel, Obj, Str, Str, Obj, Block<dyn Fn(Str)>);
#[cfg(target_os = "macos")]
type AuthFn = unsafe extern "C-unwind" fn(Obj, Sel, Obj, Obj, Block<dyn Fn(isize, Obj)>);

#[cfg(target_os = "macos")]
fn ns_string(s: Str) -> String {
    // SAFETY: an NSString WebKit handed over, or null.
    unsafe { s.as_ref() }.map(|m| m.to_string()).unwrap_or_default()
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn js_alert(_: Obj, _: Sel, webview: Obj, message: Str, frame: Obj, done: Block<dyn Fn()>) {
    // SAFETY: WebKit's completion handler, copied so it outlives this call.
    let Some(done) = (unsafe { block2::RcBlock::copy(done) }) else { return };
    let title = says(frame);
    ask(webview, "alert", &title, &ns_string(message), Vec::new(), Box::new(move |_, _| done.call(())));
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn js_confirm(
    _: Obj,
    _: Sel,
    webview: Obj,
    message: Str,
    frame: Obj,
    done: Block<dyn Fn(objc2::runtime::Bool)>,
) {
    // SAFETY: as in `js_alert`.
    let Some(done) = (unsafe { block2::RcBlock::copy(done) }) else { return };
    let title = says(frame);
    ask(webview, "confirm", &title, &ns_string(message), Vec::new(), Box::new(move |ok, _| {
        done.call((objc2::runtime::Bool::new(ok),))
    }));
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn js_prompt(
    _: Obj,
    _: Sel,
    webview: Obj,
    prompt: Str,
    default: Str,
    frame: Obj,
    done: Block<dyn Fn(Str)>,
) {
    // SAFETY: as in `js_alert`.
    let Some(done) = (unsafe { block2::RcBlock::copy(done) }) else { return };
    let Some(field) = text_field(&ns_string(default), false) else {
        return done.call((std::ptr::null_mut(),));
    };
    let title = says(frame);
    ask(webview, "prompt", &title, &ns_string(prompt), vec![field], Box::new(move |ok, values| {
        if ok {
            let answer = objc2_foundation::NSString::from_str(values.first().map_or("", String::as_str));
            done.call((Retained::as_ptr(&answer) as Str,));
        } else {
            done.call((std::ptr::null_mut(),));
        }
    }));
}

/// An HTTP login (Basic, Digest, NTLM) asks for a name and password, as
/// Safari does; anything else (a server's certificate) is left to WebKit.
#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn http_login(_: Obj, _: Sel, webview: Obj, challenge: Obj, done: Block<dyn Fn(isize, Obj)>) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;
    const USE_CREDENTIAL: isize = 0;
    const DEFAULT: isize = 1;
    // SAFETY: as in `js_alert`.
    let Some(done) = (unsafe { block2::RcBlock::copy(done) }) else { return };
    let Some(challenge) = (unsafe { challenge.as_ref() }) else {
        return done.call((DEFAULT, std::ptr::null_mut()));
    };
    // SAFETY: an NSURLAuthenticationChallenge and its protection space;
    // plain property reads on the main thread.
    let (method, host, port, realm, failures, user) = unsafe {
        let space: Option<Retained<AnyObject>> = msg_send![challenge, protectionSpace];
        let Some(space) = space else {
            return done.call((DEFAULT, std::ptr::null_mut()));
        };
        let method: Option<Retained<NSString>> = msg_send![&*space, authenticationMethod];
        let host: Option<Retained<NSString>> = msg_send![&*space, host];
        let port: isize = msg_send![&*space, port];
        let realm: Option<Retained<NSString>> = msg_send![&*space, realm];
        let failures: isize = msg_send![challenge, previousFailureCount];
        let proposed: Option<Retained<AnyObject>> = msg_send![challenge, proposedCredential];
        let user: Option<Retained<NSString>> = match &proposed {
            Some(c) => msg_send![&**c, user],
            None => None,
        };
        let s = |s: Option<Retained<NSString>>| s.map(|s| s.to_string()).unwrap_or_default();
        (s(method), s(host), port, s(realm), failures, s(user))
    };
    if !matches!(
        method.as_str(),
        "NSURLAuthenticationMethodDefault"
            | "NSURLAuthenticationMethodHTTPBasic"
            | "NSURLAuthenticationMethodHTTPDigest"
            | "NSURLAuthenticationMethodNTLM"
    ) {
        return done.call((DEFAULT, std::ptr::null_mut()));
    }
    let (Some(name), Some(pass)) = (text_field(&user, false), text_field("", true)) else {
        return done.call((DEFAULT, std::ptr::null_mut()));
    };
    let site = if port > 0 && port != 80 && port != 443 { format!("{host}:{port}") } else { host };
    let mut text = if realm.is_empty() {
        "Your name and password are sent to the site.".to_string()
    } else {
        format!("{realm:?} asks for a name and password.")
    };
    if failures > 0 {
        text = format!("That name or password did not work. {text}");
    }
    ask(webview, "login", &format!("Log in to {site}"), &text, vec![name, pass], Box::new(move |ok, values| {
        if !ok {
            // The site's own 401 page.
            return done.call((DEFAULT, std::ptr::null_mut()));
        }
        let get = |i: usize| NSString::from_str(values.get(i).map_or("", String::as_str));
        // SAFETY: +[NSURLCredential credentialWithUser:password:persistence:],
        // kept for this session only.
        let cred: Retained<AnyObject> = unsafe {
            msg_send![class!(NSURLCredential), credentialWithUser: &*get(0), password: &*get(1), persistence: 1isize]
        };
        done.call((USE_CREDENTIAL, Retained::as_ptr(&cred) as Obj));
    }));
}

/// "localhost:3000 says", as Safari heads a page's dialog.
#[cfg(target_os = "macos")]
fn says(frame: Obj) -> String {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    let origin = (|| {
        let frame = unsafe { frame.as_ref() }?;
        // SAFETY: a WKFrameInfo; its origin's host and port are plain
        // property reads on the main thread.
        unsafe {
            let origin: Option<Retained<AnyObject>> = msg_send![frame, securityOrigin];
            let origin = origin?;
            let host: Option<Retained<objc2_foundation::NSString>> = msg_send![&*origin, host];
            let port: isize = msg_send![&*origin, port];
            let host = host?.to_string();
            Some(if port > 0 { format!("{host}:{port}") } else { host })
        }
    })();
    match origin.filter(|o| !o.is_empty()) {
        Some(o) => format!("{o} says"),
        None => "This page says".into(),
    }
}

#[cfg(target_os = "macos")]
fn text_field(value: &str, secure: bool) -> Option<Retained<objc2_app_kit::NSTextField>> {
    use objc2_app_kit::{NSSecureTextField, NSTextField};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    let mtm = objc2::MainThreadMarker::new()?;
    let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(260., 24.));
    let f: Retained<NSTextField> = if secure {
        Retained::into_super(NSSecureTextField::initWithFrame(mtm.alloc(), frame))
    } else {
        NSTextField::initWithFrame(mtm.alloc(), frame)
    };
    f.setStringValue(&NSString::from_str(value));
    Some(f)
}

/// A page's dialog on screen, until the user or an agent
/// (`browser.dialog`) answers it.
#[cfg(target_os = "macos")]
struct Pending {
    webview: usize,
    kind: &'static str,
    text: String,
    alert: Retained<objc2_app_kit::NSAlert>,
    fields: Vec<Retained<objc2_app_kit::NSTextField>>,
}

#[cfg(target_os = "macos")]
thread_local! {
    static DIALOGS: std::cell::RefCell<Vec<Pending>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Show a page's dialog as a sheet on its window, so the rest of Deck
/// keeps running, and hand `done` whether it was OK'd and its fields.
#[cfg(target_os = "macos")]
fn ask(
    webview: Obj,
    kind: &'static str,
    title: &str,
    text: &str,
    fields: Vec<Retained<objc2_app_kit::NSTextField>>,
    done: Box<dyn Fn(bool, Vec<String>)>,
) {
    use objc2::msg_send;
    use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSModalResponse, NSWindow};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return done(false, Vec::new());
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(text));
    alert.addButtonWithTitle(&NSString::from_str(if kind == "login" { "Log In" } else { "OK" }));
    if kind != "alert" {
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    }
    match fields.as_slice() {
        [] => {}
        [one] => alert.setAccessoryView(Some(one)),
        many => {
            // Stacked, the first on top.
            let n = many.len() as f64;
            let box_ = NSView::initWithFrame(mtm.alloc(), NSRect::new(NSPoint::new(0., 0.), NSSize::new(260., 32. * n - 8.)));
            for (i, f) in many.iter().enumerate() {
                f.setFrameOrigin(NSPoint::new(0., 32. * (n - 1. - i as f64)));
                box_.addSubview(f);
            }
            alert.setAccessoryView(Some(&box_));
        }
    }
    if let Some(first) = fields.first() {
        alert.window().setInitialFirstResponder(Some(first));
    }
    let values = |fields: &[Retained<objc2_app_kit::NSTextField>]| -> Vec<String> {
        fields.iter().map(|f| f.stringValue().to_string()).collect()
    };
    // SAFETY: the WKWebView WebKit called about; `window` is a property read.
    let window: Option<Retained<NSWindow>> = unsafe { webview.as_ref() }.and_then(|w| unsafe { msg_send![w, window] });
    let Some(window) = window else {
        let ok = alert.runModal() == NSAlertFirstButtonReturn;
        return done(ok, values(&fields));
    };
    let key = Retained::as_ptr(&alert) as usize;
    DIALOGS.with(|d| {
        d.borrow_mut().push(Pending {
            webview: webview as usize,
            kind,
            text: text.to_string(),
            alert: alert.clone(),
            fields: fields.clone(),
        })
    });
    let handler = block2::RcBlock::new(move |code: NSModalResponse| {
        DIALOGS.with(|d| d.borrow_mut().retain(|p| Retained::as_ptr(&p.alert) as usize != key));
        done(code == NSAlertFirstButtonReturn, values(&fields));
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));
}

/// The page's view in the window, a plain view of its own that the webview
/// fills. WebKit docks Web Inspector inside the inspected view's superview,
/// so with this between them the inspector stays inside the pane instead of
/// spreading the page and itself over the whole window.
#[cfg(target_os = "macos")]
struct Host(Retained<NSView>);

#[cfg(target_os = "macos")]
impl Host {
    fn new(webview: &wry::WebView) -> Option<Self> {
        use objc2_app_kit::NSAutoresizingMaskOptions as Mask;
        use wry::WebViewExtMacOS;
        let wk = webview.webview();
        let page: &NSView = &wk;
        // SAFETY: main thread; wry has just added the webview to gpui's view.
        let parent = unsafe { page.superview() }?;
        let mtm = objc2::MainThreadMarker::new()?;
        let view = NSView::initWithFrame(mtm.alloc(), page.frame());
        // Kept to the top through a window resize until the next layout
        // places it, as wry keeps its own child views.
        view.setAutoresizingMask(Mask::ViewMinYMargin);
        view.setHidden(true);
        parent.addSubview(&view);
        view.addSubview(page);
        page.setFrame(view.bounds());
        page.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewHeightSizable);
        page.setHidden(false);
        Some(Self(view))
    }

    fn set_hidden(&self, hidden: bool) {
        self.0.setHidden(hidden);
    }

    /// Put the view at `b`, in window coordinates (y down), the way wry's
    /// `set_bounds` would have put the webview.
    fn place(&self, b: Bounds<Pixels>) {
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        // SAFETY: main thread; the parent is gpui's view, alive with the window.
        let Some(parent) = (unsafe { self.0.superview() }) else {
            return;
        };
        let (x, y) = (f32::from(b.origin.x) as f64, f32::from(b.origin.y) as f64);
        let (w, h) = (f32::from(b.size.width) as f64, f32::from(b.size.height) as f64);
        let y = if parent.isFlipped() {
            y
        } else {
            parent.frame().size.height - y - h
        };
        self.0.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
        // Alone, the page fills the view. With the inspector docked beside
        // it, WebKit lays the two out itself and is not to be fought.
        let subs = self.0.subviews();
        if subs.count() == 1 {
            if let Some(page) = subs.firstObject() {
                page.setFrame(self.0.bounds());
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for Host {
    fn drop(&mut self) {
        self.0.removeFromSuperview();
    }
}

impl Drop for BrowserPane {
    /// An inspector in a window of its own would outlive the pane, and so
    /// would the popups its page opened.
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        crate::gtk_loop::PAGES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if self.webview.is_devtools_open() {
            self.webview.close_devtools();
        }
        popup::close_all(self.webview_ptr());
        #[cfg(windows)]
        win::forget(self.webview_ptr());
    }
}

/// The page's view sits inside the card: round its bottom corners (the
/// card's own clipping does not reach a native view). Its appearance comes
/// from the app, which wears the theme's, so pages that follow
/// `prefers-color-scheme`, their scrollbars and form controls match deck.
#[cfg(target_os = "macos")]
fn style_native(view: &NSView, radius: f32, all_corners: bool) {
    use objc2_quartz_core::CACornerMask;
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setCornerRadius(radius as f64);
        // AppKit layers are not flipped: MinY is the bottom edge. The
        // editor has no toolbar above it, so it rounds its top too.
        let bottom = CACornerMask::LayerMinXMinYCorner | CACornerMask::LayerMaxXMinYCorner;
        layer.setMaskedCorners(if all_corners {
            bottom | CACornerMask::LayerMinXMaxYCorner | CACornerMask::LayerMaxXMaxYCorner
        } else {
            bottom
        });
        layer.setMasksToBounds(true);
    }
}

/// Off macOS wry places the webview itself and draws no dialogs of ours;
/// the engine's own (WebKitGTK, WebView2) show instead.
#[cfg(not(target_os = "macos"))]
struct Host;

#[cfg(not(target_os = "macos"))]
impl Host {
    fn new(_webview: &wry::WebView) -> Option<Self> {
        None
    }

    fn place(&self, _b: Bounds<Pixels>) {}

    fn set_hidden(&self, _hidden: bool) {}
}

#[cfg(not(target_os = "macos"))]
fn add_page_dialogs(_webview: &wry::WebView) {}

/// The address to hand the engine: on Windows the editor's
/// `deckcode://localhost/` is served as `http://deckcode.localhost/`.
fn engine_uri(url: &str) -> String {
    let prefix = format!("{}://localhost/", crate::code::SCHEME);
    match url.strip_prefix(&prefix) {
        Some(rest) if cfg!(windows) => format!("http://{}.localhost/{rest}", crate::code::SCHEME),
        _ => url.to_string(),
    }
}

/// The editor's address as wry reports it on Windows, where a custom
/// scheme is served as `http://deckcode.localhost/`, put back as
/// `deckcode://localhost/`; any other address as it is.
fn editor_uri(to: String) -> String {
    if cfg!(windows) {
        for http in ["http", "https"] {
            let prefix = format!("{http}://{}.localhost/", crate::code::SCHEME);
            if let Some(rest) = to.strip_prefix(&prefix) {
                return format!("{}://localhost/{rest}", crate::code::SCHEME);
            }
        }
    }
    to
}

/// At least a point each way: X11 refuses to make a window of no size,
/// and a page starts out at empty bounds until it is laid out.
fn rect(b: Bounds<Pixels>) -> wry::Rect {
    wry::Rect {
        position: LogicalPosition::new(f32::from(b.origin.x) as f64, f32::from(b.origin.y) as f64)
            .into(),
        size: LogicalSize::new(
            (f32::from(b.size.width) as f64).max(1.),
            (f32::from(b.size.height) as f64).max(1.),
        )
        .into(),
    }
}

/// wry hands back the JSON of what the page returned — here, a string that is
/// itself JSON.
fn decode_eval(json: &str) -> Result<Value, String> {
    // WebKit returned nothing: the script did not parse, or the page was
    // between documents.
    if json.is_empty() {
        return Err("the page did not run it (a syntax error, or it was loading)".into());
    }
    let inner: String = serde_json::from_str(json).map_err(|e| format!("bad eval result: {e}"))?;
    let mut v: Value = serde_json::from_str(&inner).map_err(|e| format!("bad eval result: {e}"))?;
    if let Some(err) = v.get("err").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(v.get_mut("ok").map(Value::take).unwrap_or(Value::Null))
}

/// The address as Safari shows it while you are not editing: no scheme, no
/// `www.`, no trailing slash.
fn pretty_url(url: &str) -> String {
    let s = url.split_once("://").map_or(url, |(_, r)| r);
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.strip_suffix('/').unwrap_or(s).to_string()
}

/// Percent-encode for a query string, spaces as `+`.
pub fn encode_query(s: &str) -> String {
    let mut q = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                q.push(b as char)
            }
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    q
}

/// The search engine picked in Settings; Google until told otherwise.
static ENGINE: Mutex<Search> = Mutex::new(Search::Google);

pub fn set_search_engine(engine: Search) {
    *ENGINE.lock().unwrap_or_else(|e| e.into_inner()) = engine;
}

pub fn search_engine() -> Search {
    *ENGINE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn search_url(query: &str) -> String {
    search_engine().url(&encode_query(query.trim()))
}

/// A host on the local network or this machine, where dev servers speak
/// plain HTTP.
fn is_local_host(s: &str) -> bool {
    let host = s.split(['/', '?', '#']).next().unwrap_or(s);
    let host = host.rsplit_once(':').map_or(host, |(h, port)| {
        if port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    host == "localhost"
        || host == "[::1]"
        || host.ends_with(".localhost")
        || (host.split('.').count() == 4
            && host
                .split('.')
                .all(|p| !p.is_empty() && p.parse::<u8>().is_ok()))
}

/// Text the address bar opens rather than searches for.
fn looks_like_address(s: &str) -> bool {
    let s = s.trim();
    if s.starts_with('?') {
        return false;
    }
    s.contains("://")
        || s.starts_with("about:")
        || s.starts_with("data:")
        || is_local_host(s)
        || (!s.contains(char::is_whitespace) && s.contains('.') && !s.ends_with('.'))
}

/// What the address bar does: URLs pass through, hosts get a scheme (plain
/// HTTP for this machine and the local network), anything else is a search
/// with the chosen engine. A leading `?` forces a search.
pub fn normalize_url(input: &str) -> String {
    let s = input.trim();
    if let Some(q) = s.strip_prefix('?') {
        return search_url(q);
    }
    if !looks_like_address(s) {
        return search_url(s);
    }
    if s.contains("://") || s.starts_with("about:") || s.starts_with("data:") {
        return s.to_string();
    }
    if is_local_host(s) {
        return format!("http://{s}");
    }
    format!("https://{s}")
}

/// Give the keyboard back to gpui after a webview has held it.
///
/// gpui's view never claims first responder on click (it is normally the only
/// view in the window), so once WKWebView has it, keys keep going to the page
/// until someone hands them back.
/// On X11 a page is a window of its own inside Deck's, and keeps the
/// keyboard after it is clicked; this gives it back to Deck's window.
#[cfg(target_os = "linux")]
pub fn reclaim_keyboard(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(handle) = HasWindowHandle::window_handle(window) {
        if let RawWindowHandle::Xlib(h) = handle.as_raw() {
            crate::x11::focus(h.window);
        }
    }
}

/// A page is a child window holding the focus once clicked; Deck's own
/// window takes it back.
#[cfg(windows)]
pub fn reclaim_keyboard(window: &Window) {
    win::reclaim_keyboard(window);
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn reclaim_keyboard(_window: &Window) {}

#[cfg(target_os = "macos")]
pub fn reclaim_keyboard(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Fully qualified: gpui's inherent `Window::window_handle` shadows the trait.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return;
    };
    // SAFETY: gpui's own content view, alive for as long as the window is.
    let view: &objc2_app_kit::NSView = unsafe { h.ns_view.cast().as_ref() };
    if let Some(win) = view.window() {
        let responder: &objc2_app_kit::NSResponder = view;
        win.makeFirstResponder(Some(responder));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_hosts_and_searches() {
        assert_eq!(normalize_url("https://a.b/c"), "https://a.b/c");
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("localhost"), "http://localhost");
        assert_eq!(
            normalize_url("192.168.1.20:8080/x"),
            "http://192.168.1.20:8080/x"
        );
        assert_eq!(
            normalize_url("app.localhost:5173"),
            "http://app.localhost:5173"
        );
        assert_eq!(
            normalize_url("rust gpui"),
            "https://www.google.com/search?q=rust+gpui"
        );
        assert_eq!(
            normalize_url("a&b"),
            "https://www.google.com/search?q=a%26b"
        );
        assert_eq!(
            normalize_url("?example.com"),
            "https://www.google.com/search?q=example.com"
        );
        assert_eq!(
            normalize_url("node."),
            "https://www.google.com/search?q=node."
        );
    }

    #[test]
    fn pretty_urls_drop_scheme_www_and_slash() {
        assert_eq!(pretty_url("https://www.example.com/"), "example.com");
        assert_eq!(
            pretty_url("http://localhost:5173/app"),
            "localhost:5173/app"
        );
    }

    #[test]
    fn page_messages_are_checked() {
        assert!(matches!(parse_ipc("focus"), Some(Msg::Focused)));
        assert!(matches!(parse_ipc("moved"), Some(Msg::Moved)));
        assert!(matches!(
            parse_ipc("tab:https://a.b/c"),
            Some(Msg::LinkTab(u)) if u == "https://a.b/c"
        ));
        assert!(parse_ipc("tab:javascript:alert(1)").is_none());
        assert!(parse_ipc("icon:file:///etc/passwd").is_none());
        assert!(parse_ipc("url:https://evil.example").is_none());
    }

    #[test]
    fn log_batches_are_trimmed() {
        let Some(Msg::Log(e)) = parse_ipc(
            r#"{"deck":"log","entries":[{"t":"console","level":"log","text":"hi","seq":99},3,"x",{"t":"net","url":"https://a.b/","status":404,"nested":{"a":1}},{"no":"t"}]}"#,
        ) else {
            panic!("not a log batch");
        };
        assert_eq!(e.len(), 2);
        assert_eq!(e[0], serde_json::json!({"t": "console", "level": "log", "text": "hi"}));
        assert_eq!(e[1], serde_json::json!({"t": "net", "url": "https://a.b/", "status": 404}));
        assert!(parse_ipc(r#"{"deck":"log","entries":[]}"#).is_none());
        assert!(parse_ipc(r#"{"deck":"log","entries":{"t":"console"}}"#).is_none());
        assert!(parse_ipc(r#"{"deck":"log"}"#).is_none());

        let long = "é".repeat(5000);
        let body = serde_json::json!({"deck": "log", "entries": [{"t": "console", "text": long}]});
        let Some(Msg::Log(e)) = parse_ipc(&body.to_string()) else {
            panic!("not a log batch");
        };
        assert!(e[0]["text"].as_str().unwrap().len() <= 4096 + "…".len());

        let many: Vec<Value> = (0..500).map(|i| serde_json::json!({"t": "console", "text": i.to_string()})).collect();
        let body = serde_json::json!({"deck": "log", "entries": many});
        let Some(Msg::Log(e)) = parse_ipc(&body.to_string()) else {
            panic!("not a log batch");
        };
        assert_eq!(e.len(), LOG_BATCH);
    }

    #[test]
    fn log_queries_pick_their_kind() {
        let q = |p: Value| LogQuery::from_params(&p).unwrap();
        let log = serde_json::json!({"t": "console", "level": "log", "text": "Ready"});
        let err = serde_json::json!({"t": "console", "level": "error", "text": "boom"});
        let thrown = serde_json::json!({"t": "error", "kind": "error", "text": "TypeError: x"});
        let ok = serde_json::json!({"t": "net", "type": "fetch", "url": "https://a.b/api", "status": 200});
        let missing = serde_json::json!({"t": "net", "type": "img", "url": "https://a.b/x.png", "status": 404});
        let down = serde_json::json!({"t": "net", "type": "fetch", "url": "https://c.d/", "failed": true});
        let nav = serde_json::json!({"t": "nav", "url": "https://a.b/"});
        let all = [&log, &err, &thrown, &ok, &missing, &down, &nav];
        let pick = |q: &LogQuery| all.iter().filter(|e| q.wants(e)).count();

        assert_eq!(pick(&q(serde_json::json!({}))), 7);
        assert_eq!(pick(&q(serde_json::json!({"kind": "console"}))), 4);
        assert_eq!(pick(&q(serde_json::json!({"kind": "network"}))), 3);
        assert_eq!(pick(&q(serde_json::json!({"kind": "errors"}))), 4);
        assert_eq!(pick(&q(serde_json::json!({"kind": "network", "failed": true}))), 2);
        assert_eq!(pick(&q(serde_json::json!({"filter": "A.B/API"}))), 1);
        assert_eq!(pick(&q(serde_json::json!({"kind": "console", "filter": "ready"}))), 1);
        assert!(LogQuery::from_params(&serde_json::json!({"kind": "dom"})).is_err());

        let p = q(serde_json::json!({"since": "12", "limit": 5000, "clear": "true"}));
        assert_eq!((p.since, p.limit, p.clear), (12, 1000, true));
    }

    #[test]
    fn eval_results_unwrap_and_errors_surface() {
        assert_eq!(decode_eval(r#""{\"ok\":3}""#), Ok(Value::from(3)));
        assert_eq!(decode_eval(r#""{\"ok\":null}""#), Ok(Value::Null));
        assert_eq!(decode_eval(r#""{\"err\":\"boom\"}""#), Err("boom".into()));
    }
}
