//! The background service off macOS. Windows has a tray icon whose menu
//! lists every job with Open in Deck, Restart and Stop; Linux has no icon
//! yet (it needs StatusNotifierItem). On both a job that fails plays its
//! sound and posts a notification that opens it in Deck.

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
/// Deck when its notification is clicked. On Windows the tray icon's
/// message loop is what holds it.
pub fn run() {
    #[cfg(windows)]
    {
        std::thread::spawn(open_clicked);
        tray::run();
    }
    #[cfg(not(windows))]
    open_clicked();
    loop {
        std::thread::park();
    }
}

/// Open the job of each notification clicked, for as long as they come.
fn open_clicked() {
    use futures::StreamExt;
    let Some(mut rx) = crate::notify::start() else { return };
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
}

/// Take the tray icon away, as the service quits.
#[cfg(windows)]
pub fn remove_icon() {
    tray::remove();
}

#[cfg(windows)]
mod tray {
    //! A hidden window of its own (never shown: a top-level one, so it
    //! hears Explorer restart and can own the popup menu) with the icon.

    use crate::daemon::{self, JobInfo};
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use std::sync::Mutex;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
        GetMessageW, LoadIconW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
        TrackPopupMenu, TranslateMessage, HICON, IDI_APPLICATION, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
        MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP,
        WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
    };

    /// What the icon sends its window when clicked.
    const WM_TRAY: u32 = WM_APP + 1;
    const OPEN_DECK: usize = 1;
    const STOP_ALL: usize = 2;
    const QUIT: usize = 3;
    /// A job's items: this plus 4 × its row, plus 0 (open), 1 (restart) or
    /// 2 (stop).
    const JOB_BASE: usize = 100;

    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    static ICON: AtomicIsize = AtomicIsize::new(0);
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    /// The jobs of the menu last shown, by row.
    static ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Put up the icon and run its window's messages; returns only if
    /// there is no window to have.
    pub fn run() {
        // SAFETY: a window class and window of this thread, used only here.
        unsafe {
            let Ok(module) = GetModuleHandleW(PCWSTR::null()) else { return };
            let class = w!("DeckServiceTray");
            let wc = WNDCLASSW { lpfnWndProc: Some(proc_), hInstance: module.into(), lpszClassName: class, ..Default::default() };
            if RegisterClassW(&wc) == 0 {
                return;
            }
            let Ok(hwnd) = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("Deck service"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(module.into()), None) else {
                return;
            };
            WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
            TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
            // The app's icon when the exe carries one as resource 1.
            let icon = LoadIconW(Some(module.into()), PCWSTR(1 as _)).or_else(|_| LoadIconW(None, IDI_APPLICATION)).unwrap_or_default();
            ICON.store(icon.0 as isize, Ordering::Relaxed);
            add(hwnd);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn data(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut d = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: HICON(ICON.load(Ordering::Relaxed) as _),
            ..Default::default()
        };
        for (slot, u) in d.szTip.iter_mut().zip("Deck".encode_utf16()) {
            *slot = u;
        }
        d
    }

    unsafe fn add(hwnd: HWND) {
        let _ = Shell_NotifyIconW(NIM_ADD, &data(hwnd));
    }

    pub fn remove() {
        let hwnd = WINDOW.load(Ordering::Relaxed);
        if hwnd != 0 {
            // SAFETY: the icon added in `run`, by its window and id.
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data(HWND(hwnd as _))) };
        }
    }

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            if matches!(lparam.0 as u32 & 0xFFFF, WM_RBUTTONUP | WM_LBUTTONUP | WM_CONTEXTMENU) {
                show_menu(hwnd);
            }
            return LRESULT(0);
        }
        // Explorer started again: its tray is empty.
        if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) {
            add(hwnd);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// A menu item's text: `&` would underline the next letter.
    fn item(s: &str) -> Vec<u16> {
        s.replace('&', "&&").encode_utf16().chain(Some(0)).collect()
    }

    /// The menu, built afresh from the jobs each time it opens.
    unsafe fn show_menu(hwnd: HWND) {
        let Ok(menu) = CreatePopupMenu() else { return };
        let jobs = daemon::snapshot();
        let mut rows = Vec::new();
        if jobs.is_empty() {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("No jobs"));
        }
        for (i, j) in jobs.iter().enumerate() {
            let Ok(sub) = CreatePopupMenu() else { continue };
            let base = JOB_BASE + i * 4;
            let _ = AppendMenuW(sub, MF_STRING, base, w!("Open in Deck"));
            let _ = AppendMenuW(sub, MF_STRING, base + 1, w!("Restart"));
            let _ = AppendMenuW(sub, MF_STRING, base + 2, w!("Stop"));
            let text = item(&label(j));
            let _ = AppendMenuW(menu, MF_STRING | MF_POPUP, sub.0 as usize, PCWSTR(text.as_ptr()));
            rows.push(j.name.clone());
        }
        *ROWS.lock().unwrap_or_else(|e| e.into_inner()) = rows;
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, OPEN_DECK, w!("Open Deck"));
        let _ = AppendMenuW(menu, MF_STRING | if jobs.is_empty() { MF_GRAYED } else { MF_STRING }, STOP_ALL, w!("Stop All"));
        let _ = AppendMenuW(menu, MF_STRING, QUIT, w!("Quit service"));
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Without this the menu stays open when clicked away from.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, at.x, at.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        act(picked.0 as usize);
    }

    fn act(id: usize) {
        match id {
            0 => {}
            OPEN_DECK => crate::desktop::open("deck://open"),
            STOP_ALL => {
                std::thread::spawn(|| {
                    for j in daemon::snapshot() {
                        daemon::stop_named(&j.name);
                    }
                });
            }
            QUIT => daemon::quit(),
            _ => {
                let row = (id - JOB_BASE) / 4;
                let Some(name) = ROWS.lock().unwrap_or_else(|e| e.into_inner()).get(row).cloned() else { return };
                match (id - JOB_BASE) % 4 {
                    0 => crate::desktop::open(&format!("deck://job/{}", crate::browser::encode_query(&name))),
                    // Both wait for the job to end, up to seconds: not on
                    // the window's thread.
                    1 => drop(std::thread::spawn(move || daemon::restart_named(&name))),
                    _ => drop(std::thread::spawn(move || daemon::stop_named(&name))),
                }
            }
        }
    }

    /// "dev: Running 3m", "Terminal (api): Exited 1".
    fn label(j: &JobInfo) -> String {
        let state = match (j.running, j.exit) {
            (true, _) => format!("Running {}", uptime(j.started)),
            (false, Some(0)) => "Done".to_string(),
            (false, Some(c)) => format!("Exited {c}"),
            (false, None) => "Stopped".to_string(),
        };
        let agent = j.agent.as_ref().map(|a| {
            crate::workspace::AGENTS.iter().find(|x| x.cmd == *a).map_or_else(|| a.clone(), |x| x.name.to_string())
        });
        if j.tab {
            let folder = j.cwd.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "~".into());
            format!("{} ({folder}): {state}", agent.unwrap_or_else(|| "Terminal".into()))
        } else {
            format!("{}: {state}", j.name)
        }
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
}
