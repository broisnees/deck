//! A page on Windows: what WebView2 knows that wry does not pass on. How far
//! a load has got, the page's `alert`, `confirm` and `prompt` (held until
//! the pane or an agent answers them), its picture, its color scheme and its
//! corners, and where its profile lives.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    GetAvailableCoreWebView2BrowserVersionString, ICoreWebView2, ICoreWebView2Deferral,
    ICoreWebView2ScriptDialogOpeningEventArgs, ICoreWebView2_2,
    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_SCRIPT_DIALOG_KIND,
    COREWEBVIEW2_SCRIPT_DIALOG_KIND_ALERT, COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD,
    COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM, COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT,
};
use webview2_com::{
    CapturePreviewCompletedHandler, ContentLoadingEventHandler, DOMContentLoadedEventHandler,
    NavigationCompletedEventHandler, NavigationStartingEventHandler,
    ScriptDialogOpeningEventHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ, RGN_OR,
};
use windows::Win32::System::Com::{CoTaskMemFree, IStream, STREAM_SEEK_SET};
use windows::Win32::UI::Shell::SHCreateMemStream;
use wry::WebViewExtWindows;

/// Pages keep their cookies, storage and cache here, in Deck's own data
/// folder, not beside the executable where WebView2 would put them.
pub fn data_dir() -> PathBuf {
    agent_core::dirs::data_dir().join("WebView2")
}

/// Fails, in words, when the WebView2 runtime is missing.
pub fn check_runtime() -> anyhow::Result<()> {
    let mut version = PWSTR::null();
    // SAFETY: the loader writes a CoTaskMemAlloc'd string or nothing.
    let found = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    let ok = found.is_ok() && !version.is_null();
    if !version.is_null() {
        // SAFETY: ours to free, as above.
        unsafe { CoTaskMemFree(Some(version.0 as *const _)) };
    }
    if !ok {
        anyhow::bail!(
            "Pages need the Microsoft Edge WebView2 Runtime, which is not installed. \
             Get it from https://developer.microsoft.com/microsoft-edge/webview2/ and open the page again."
        );
    }
    Ok(())
}

/// A page's key: its ICoreWebView2, the same for as long as it lives.
pub fn key(webview: &wry::WebView) -> usize {
    webview.webview().as_raw() as usize
}

struct Page {
    core: ICoreWebView2,
    loading: bool,
    /// 0 asked for, 1 committed, 2 its document parsed.
    stage: usize,
    since: Instant,
    dialogs: VecDeque<Dialog>,
    /// Tells the pane a dialog came or went.
    changed: Rc<dyn Fn()>,
}

struct Dialog {
    kind: &'static str,
    text: String,
    default: String,
    args: ICoreWebView2ScriptDialogOpeningEventArgs,
    deferral: ICoreWebView2Deferral,
}

thread_local! {
    static PAGES: RefCell<HashMap<usize, Page>> = RefCell::new(HashMap::new());
}

fn with_page<R>(key: usize, f: impl FnOnce(&mut Page) -> R) -> Option<R> {
    PAGES.with(|p| p.borrow_mut().get_mut(&key).map(f))
}

/// Follow `webview`'s loads and take over its script dialogs; `changed`
/// runs when one starts or stops waiting. Main thread, once per page.
pub fn attach(webview: &wry::WebView, loading: bool, changed: impl Fn() + 'static) -> windows::core::Result<()> {
    let core = webview.webview();
    let me = key(webview);
    let changed: Rc<dyn Fn()> = Rc::new(changed);
    let dialog = changed.clone();
    PAGES.with(|p| {
        p.borrow_mut().insert(
            me,
            Page { core: core.clone(), loading, stage: 0, since: Instant::now(), dialogs: VecDeque::new(), changed },
        )
    });
    let stage = move |loading: bool, stage: usize| {
        with_page(me, |p| {
            p.loading = loading;
            p.stage = stage;
            p.since = Instant::now();
        });
    };
    let mut token = 0i64;
    // SAFETY: WebView2 calls on the main thread, which made the page; the
    // handlers keep only its key.
    unsafe {
        core.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, _| {
                stage(true, 0);
                Ok(())
            })),
            &mut token,
        )?;
        core.add_ContentLoading(
            &ContentLoadingEventHandler::create(Box::new(move |_, _| {
                stage(true, 1);
                Ok(())
            })),
            &mut token,
        )?;
        if let Ok(core2) = core.cast::<ICoreWebView2_2>() {
            core2.add_DOMContentLoaded(
                &DOMContentLoadedEventHandler::create(Box::new(move |_, _| {
                    stage(true, 2);
                    Ok(())
                })),
                &mut token,
            )?;
        }
        core.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                stage(false, 0);
                Ok(())
            })),
            &mut token,
        )?;
        // Deck answers them: in a bar over the page, or an agent through
        // `browser.dialog`. The page waits on the deferral meanwhile.
        core.Settings()?.SetAreDefaultScriptDialogsEnabled(false)?;
        core.add_ScriptDialogOpening(
            &ScriptDialogOpeningEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_SCRIPT_DIALOG_KIND::default();
                args.Kind(&mut kind)?;
                let (kind, text) = match kind {
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_ALERT => ("alert", read(|s| args.Message(s))),
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_CONFIRM => ("confirm", read(|s| args.Message(s))),
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_PROMPT => ("prompt", read(|s| args.Message(s))),
                    COREWEBVIEW2_SCRIPT_DIALOG_KIND_BEFOREUNLOAD => {
                        ("confirm", "Leave this page? Changes you made may not be saved.".to_string())
                    }
                    _ => return Ok(()),
                };
                let default = if kind == "prompt" { read(|s| args.DefaultText(s)) } else { String::new() };
                let deferral = args.GetDeferral()?;
                let d = Dialog { kind, text, default, args, deferral };
                let held = with_page(me, |p| p.dialogs.push_back(d)).is_some();
                if held {
                    dialog();
                }
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

/// A string WebView2 hands out, freed after reading.
fn read(get: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut s = PWSTR::null();
    if get(&mut s).is_err() || s.is_null() {
        return String::new();
    }
    // SAFETY: a CoTaskMemAlloc'd, NUL-terminated string that is ours now.
    unsafe {
        let out = s.to_string().unwrap_or_default();
        CoTaskMemFree(Some(s.0 as *const _));
        out
    }
}

/// Whether the page is loading, and roughly how far along: each step of a
/// load (asked for, committed, parsed) creeps toward the next.
pub fn load_state(key: usize) -> (bool, f32) {
    with_page(key, |p| {
        if !p.loading {
            return (false, 1.);
        }
        const STEPS: [f32; 4] = [0.1, 0.35, 0.7, 0.95];
        let (from, to) = (STEPS[p.stage], STEPS[p.stage + 1]);
        let t = p.since.elapsed().as_secs_f32();
        (true, from + (to - from) * (1. - (-t / 1.2).exp()))
    })
    .unwrap_or((false, 1.))
}

pub fn stop(key: usize) {
    with_page(key, |p| {
        // SAFETY: main thread, a live page.
        let _ = unsafe { p.core.Stop() };
    });
}

/// The dialog the page waits on: its kind, text and a prompt's default.
pub fn pending(key: usize) -> Option<(&'static str, String, String)> {
    with_page(key, |p| p.dialogs.front().map(|d| (d.kind, d.text.clone(), d.default.clone()))).flatten()
}

/// Answer the waiting dialog: OK (with a prompt's `value`) or Cancel. An
/// alert has only OK. Returns its kind and text.
pub fn answer(key: usize, accept: bool, value: Option<&str>) -> Option<(&'static str, String)> {
    let (d, changed) = with_page(key, |p| Some((p.dialogs.pop_front()?, p.changed.clone()))).flatten()?;
    // SAFETY: main thread; the args stay valid until the deferral completes.
    unsafe {
        if accept && d.kind != "alert" {
            if d.kind == "prompt" {
                let _ = d.args.SetResultText(&HSTRING::from(value.unwrap_or(&d.default)));
            }
            let _ = d.args.Accept();
        }
        let _ = d.deferral.Complete();
    }
    changed();
    Some((d.kind, d.text))
}

/// The page is going: let go of it, cancelling any dialog it waits on.
pub fn forget(key: usize) {
    let page = PAGES.with(|p| p.borrow_mut().remove(&key));
    for d in page.into_iter().flat_map(|p| p.dialogs) {
        // SAFETY: as in `answer`.
        let _ = unsafe { d.deferral.Complete() };
    }
}

/// Any open page, for what goes to the profile they all share (cookies).
pub fn any_page() -> Option<ICoreWebView2> {
    PAGES.with(|p| p.borrow().values().next().map(|p| p.core.clone()))
}

/// A PNG of what the page shows now, delivered once.
pub fn snapshot(webview: &wry::WebView) -> futures::channel::oneshot::Receiver<Option<Vec<u8>>> {
    let (tx, rx) = futures::channel::oneshot::channel();
    // SAFETY: a fresh memory stream; WebView2 writes it on the main thread
    // and calls back there once it is done.
    unsafe {
        let Some(stream) = SHCreateMemStream(None) else { return rx };
        let written = stream.clone();
        let done = CapturePreviewCompletedHandler::create(Box::new(move |result| {
            let _ = tx.send(result.ok().and_then(|_| read_stream(&written)));
            Ok(())
        }));
        let _ = webview.webview().CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &done);
    }
    rx
}

fn read_stream(stream: &IStream) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    // SAFETY: reads into `buf`, at most its length.
    unsafe {
        stream.Seek(0, STREAM_SEEK_SET, None).ok()?;
        loop {
            let mut n = 0u32;
            stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut n)).ok().ok()?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Pages that follow `prefers-color-scheme`, their scrollbars and form
/// controls, wear the theme's.
pub fn set_dark(webview: &wry::WebView, dark: bool) {
    let _ = webview.set_theme(if dark { wry::Theme::Dark } else { wry::Theme::Light });
}

/// Cut the page's window to the card's curve: its bottom corners, or all
/// four for the editor, which has no toolbar above it. `width` is the
/// logical width it was just placed at, to find the scale.
pub fn round(webview: &wry::WebView, width: f32, radius: f32, all_corners: bool) {
    let hwnd: HWND = webview.hwnd();
    let mut r = RECT::default();
    // SAFETY: wry's container window, alive with the webview; the region
    // goes to the window, which frees it.
    unsafe {
        if windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r).is_err() {
            return;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 || width <= 0. {
            return;
        }
        let d = ((radius * w as f32 / width) * 2.).round() as i32;
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, d, d);
        if !all_corners {
            // Square on top: the toolbar sits right above it.
            let top = CreateRectRgn(0, 0, w, h / 2);
            CombineRgn(Some(rgn), Some(rgn), Some(top), RGN_OR);
            let _ = DeleteObject(HGDIOBJ(top.0));
        }
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        }
    }
}

/// Give the keyboard back to Deck's own window after a page held it.
pub fn reclaim_keyboard(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    // SAFETY: gpui's own window, alive for as long as `window` is.
    let _ = unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(HWND(h.hwnd.get() as *mut _)))
    };
}
