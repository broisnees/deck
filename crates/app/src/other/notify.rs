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
    #[cfg(windows)]
    win::register_app();
    let (tx, rx) = unbounded();
    if let Ok(mut c) = CLICKS.lock() {
        *c = Some(tx);
    }
    Some(rx)
}

/// The freedesktop sound theme has no names like macOS's, so every sound
/// but None plays the theme's message sound. Windows plays a system sound
/// for each name, at the system's volume: PlaySound has no volume.
pub fn play(name: &str, volume: f32) {
    if name.is_empty() || name == "None" {
        return;
    }
    let _ = volume;
    #[cfg(windows)]
    win::play(name);
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
            send(event);
        });
    }
    #[cfg(windows)]
    if let Err(e) = win::post(pane, title, body, kind) {
        tracing::warn!("toast not shown: {e}");
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (pane, title, body, kind);
    }
}

fn send(event: Event) {
    if let Some(tx) = CLICKS.lock().ok().and_then(|c| c.clone()) {
        let _ = tx.unbounded_send(event);
    }
}

#[cfg(windows)]
pub use win::{badge, deck_window};

#[cfg(windows)]
mod win {
    use super::{send, Event, Kind};
    use std::sync::Mutex;
    use windows::core::{w, Interface, HSTRING};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::{IPropertyValue, TypedEventHandler};
    use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager};
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows_core::BOOL;

    const AUMID: &str = "Deck.Deck";

    /// Toasts shown and not yet gone, kept so their Activated handlers live.
    static SHOWN: Mutex<Vec<ToastNotification>> = Mutex::new(Vec::new());

    /// File Deck's toasts under its own name: an unpackaged exe has no
    /// AppUserModelID until one is registered for it in HKCU.
    pub fn register_app() {
        use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
        let key = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
        let name: Vec<u16> = "Deck".encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated UTF-16 string and its size in bytes.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                w!("DisplayName"),
                REG_SZ.0,
                Some(name.as_ptr().cast()),
                (name.len() * 2) as u32,
            )
        };
        if rc.is_err() {
            tracing::warn!("could not register Deck for notifications: {rc:?}");
        }
        // SAFETY: a static string.
        if let Err(e) = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID)) } {
            tracing::warn!("could not set Deck's AppUserModelID: {e}");
        }
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn xml(title: &str, body: &str, kind: Kind) -> String {
        let reply = |placeholder: &str| {
            format!(
                r#"<input id="reply" type="text" placeHolderContent="{placeholder}"/><action content="Reply" arguments="reply" hint-inputId="reply" activationType="foreground"/>"#
            )
        };
        let actions = match kind {
            Kind::Info => String::new(),
            Kind::Permission => format!(
                r#"<actions><action content="Approve" arguments="allow" activationType="foreground"/><action content="Always Allow" arguments="always" activationType="foreground"/><action content="Deny" arguments="deny" activationType="foreground"/>{}</actions>"#,
                reply("Do this instead")
            ),
            Kind::Question => format!("<actions>{}</actions>", reply("Your answer")),
        };
        // Silent: the caller plays the sound it wants (see `play`).
        format!(
            r#"<toast launch="open" activationType="foreground"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual><audio silent="true"/>{actions}</toast>"#,
            escape(title),
            escape(body)
        )
    }

    pub fn post(pane: u64, title: &str, body: &str, kind: Kind) -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml(title, body, kind)))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        toast.Activated(&TypedEventHandler::new(move |_, args: windows::core::Ref<windows::core::IInspectable>| {
            let Some(args) = args.as_ref().and_then(|a| a.cast::<ToastActivatedEventArgs>().ok()) else {
                return Ok(());
            };
            let event = match args.Arguments()?.to_string().as_str() {
                "allow" => Event::Allow(pane),
                "always" => Event::AllowAlways(pane),
                "deny" => Event::Deny(pane),
                "reply" => {
                    let typed = args
                        .UserInput()
                        .and_then(|input| input.Lookup(&HSTRING::from("reply")))
                        .and_then(|v| v.cast::<IPropertyValue>()?.GetString())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    Event::Reply(pane, typed)
                }
                _ => Event::Open(pane),
            };
            send(event);
            Ok(())
        }))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)?;
        if let Ok(mut shown) = SHOWN.lock() {
            shown.push(toast);
            // Banners go after a few seconds; the last few are plenty.
            let extra = shown.len().saturating_sub(16);
            shown.drain(..extra);
        }
        Ok(())
    }

    /// A Windows system sound for each of the macOS names Settings offers.
    pub fn play(name: &str) {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
        let alias = match name {
            "Glass" => "Notification.Reminder",
            "Ping" => "Notification.IM",
            "Pop" => "Notification.SMS",
            "Purr" => "Notification.Mail",
            "Hero" => "SystemNotification",
            "Funk" => "SystemExclamation",
            "Submarine" => "MailBeep",
            "Blow" => "SystemAsterisk",
            "Bottle" => "Notification.Looping.Alarm2",
            "Frog" => "DeviceConnect",
            "Morse" => "Notification.Looping.Call2",
            "Sosumi" => ".Default",
            "Basso" => "SystemHand",
            _ => "Notification.Default",
        };
        // Looping sounds play once without SND_LOOP.
        // SAFETY: a NUL-terminated alias; SND_ASYNC returns at once.
        let ok = unsafe { PlaySoundW(&HSTRING::from(alias), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT) };
        if !ok.as_bool() {
            // SAFETY: plays the default beep.
            let _ = unsafe { windows::Win32::System::Diagnostics::Debug::MessageBeep(Default::default()) };
        }
    }

    /// Deck's main window: the foreground one if it is Deck's, else the
    /// first visible gpui window of this process.
    pub fn deck_window() -> Option<HWND> {
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
        };
        fn ours(hwnd: HWND) -> bool {
            let mut pid = 0;
            let mut class = [0u16; 32];
            // SAFETY: plain queries on a window handle and local buffers.
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let n = GetClassNameW(hwnd, &mut class) as usize;
                pid == GetCurrentProcessId()
                    && IsWindowVisible(hwnd).as_bool()
                    && String::from_utf16_lossy(&class[..n]) == "Zed::Window"
            }
        }
        unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> BOOL {
            if ours(hwnd) {
                // SAFETY: `found` is the Option<HWND> below.
                unsafe { *(found.0 as *mut Option<HWND>) = Some(hwnd) };
                return false.into();
            }
            true.into()
        }
        // SAFETY: a plain query.
        let front = unsafe { GetForegroundWindow() };
        if !front.is_invalid() && ours(front) {
            return Some(front);
        }
        let mut found: Option<HWND> = None;
        // SAFETY: the callback writes only to `found`, which outlives it.
        let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut found as *mut _ as isize)) };
        found
    }

    thread_local! {
        static TASKBAR: Option<windows::Win32::UI::Shell::ITaskbarList3> = {
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
            use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};
            // SAFETY: COM is initialised on gpui's main thread.
            unsafe {
                let list: Option<ITaskbarList3> = CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER).ok();
                list.filter(|l| l.HrInit().is_ok())
            }
        };
    }

    /// The taskbar button's overlay: how many panes want you (a dot past
    /// 9), or none. A new one flashes the button while Deck is behind.
    /// Main thread.
    pub fn badge(n: usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyIcon, FlashWindowEx, GetForegroundWindow, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY, HICON,
        };
        static LAST: AtomicUsize = AtomicUsize::new(0);
        let last = LAST.swap(n, Ordering::Relaxed);
        let Some(hwnd) = deck_window() else { return };
        TASKBAR.with(|list| {
            let Some(list) = list else { return };
            let icon = if n > 0 { icon(n) } else { None };
            let label = HSTRING::from(if n > 0 { format!("{n} waiting") } else { String::new() });
            // SAFETY: Deck's window and an icon the taskbar copies.
            unsafe {
                let _ = list.SetOverlayIcon(hwnd, icon.unwrap_or(HICON::default()), &label);
                if let Some(icon) = icon {
                    let _ = DestroyIcon(icon);
                }
            }
        });
        // SAFETY: plain calls on Deck's window.
        unsafe {
            if n > last && GetForegroundWindow() != hwnd {
                let info = FLASHWINFO {
                    cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                    hwnd,
                    dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                    uCount: 0,
                    dwTimeout: 0,
                };
                let _ = FlashWindowEx(&info);
            }
        }
    }

    /// 3x5 digits, one row per byte, high bit of the low three first.
    const DIGITS: [[u8; 5]; 10] = [
        [7, 5, 5, 5, 7],
        [2, 6, 2, 2, 7],
        [7, 1, 7, 4, 7],
        [7, 1, 3, 1, 7],
        [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7],
        [7, 4, 7, 5, 7],
        [7, 1, 1, 2, 2],
        [7, 5, 7, 5, 7],
        [7, 5, 7, 1, 7],
    ];

    /// A 16 px red disc with the count in white, or a white dot past 9.
    fn icon(n: usize) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
        use windows::Win32::Graphics::Gdi::{
            CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};
        const S: i32 = 16;
        let mut px = vec![0u32; (S * S) as usize];
        let (red, white) = (0xFFD9_3025u32, 0xFFFF_FFFFu32);
        for y in 0..S {
            for x in 0..S {
                let (dx, dy) = (x as f32 + 0.5 - 8., y as f32 + 0.5 - 8.);
                let d = (dx * dx + dy * dy).sqrt();
                // Premultiplied, with a soft edge.
                let a = (8. - d).clamp(0., 1.);
                if a > 0. {
                    let k = |c: u32, s: u32| (((c >> s) & 0xFF) as f32 * a) as u32;
                    px[(y * S + x) as usize] =
                        ((255. * a) as u32) << 24 | k(red, 16) << 16 | k(red, 8) << 8 | k(red, 0);
                }
            }
        }
        if n <= 9 {
            // Each font pixel 2x2: 6x10, centred.
            for (row, bits) in DIGITS[n].iter().enumerate() {
                for col in 0..3 {
                    if bits & (4 >> col) != 0 {
                        for (oy, ox) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let (x, y) = (5 + col * 2 + ox, 3 + row as i32 * 2 + oy);
                            px[(y * S + x) as usize] = white;
                        }
                    }
                }
            }
        } else {
            for y in 6..10 {
                for x in 6..10 {
                    px[(y * S + x) as usize] = white;
                }
            }
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: S,
                biHeight: -S,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: a top-down 32 bpp section the size of `px`, filled before
        // the icon copies it; both bitmaps are freed after.
        unsafe {
            let mut bits = std::ptr::null_mut();
            let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u32>(), px.len());
            let mask = CreateBitmap(S, S, 1, 1, None);
            let icon = CreateIconIndirect(&ICONINFO {
                fIcon: true.into(),
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: color,
            })
            .ok();
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            icon
        }
    }
}
