//! The macOS permissions an app cannot ask for in Info.plist: whether Deck
//! has them, where macOS lets an app find out, and the System Settings pane
//! for each. Terminals and agents run under Deck's background service
//! (`deckd`), which macOS treats as a program of its own: Full Disk Access
//! matters for both.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    On,
    Off,
    /// macOS gives an app no way to tell.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    FullDisk,
    Accessibility,
    Screen,
    DevTools,
    AppManagement,
    Automation,
    LocalNetwork,
    Camera,
    Files,
    Notifications,
}

impl Permission {
    /// The ones only you can turn on, then the ones macOS asks about the
    /// first time something needs them.
    pub const MANUAL: [Permission; 5] = [
        Permission::FullDisk,
        Permission::Accessibility,
        Permission::Screen,
        Permission::DevTools,
        Permission::AppManagement,
    ];
    pub const ASKED: [Permission; 5] = [
        Permission::Automation,
        Permission::LocalNetwork,
        Permission::Camera,
        Permission::Files,
        Permission::Notifications,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Permission::FullDisk => "Full Disk Access",
            Permission::Accessibility => "Accessibility",
            Permission::Screen => "Screen Recording",
            Permission::DevTools => "Developer Tools",
            Permission::AppManagement => "App Management",
            Permission::Automation => "Automation",
            Permission::LocalNetwork => "Local Network",
            Permission::Camera => "Camera and Microphone",
            Permission::Files => "Files and Folders",
            Permission::Notifications => "Notifications",
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            Permission::FullDisk => {
                "For terminals and agents that read Mail, Messages, Safari data or other apps' folders in Library."
            }
            Permission::Accessibility => {
                "For agents and scripts that control other apps: clicking, typing, UI scripting."
            }
            Permission::Screen => "For screencapture and agents taking screenshots of other apps' windows.",
            Permission::DevTools => {
                "Programs you build start without a malware scan on every launch, so cargo run and npm start faster."
            }
            Permission::AppManagement => "For brew upgrade --cask and updating apps in Applications from a terminal.",
            Permission::Automation => "Each app a script drives (osascript, open -a) is asked for once.",
            Permission::LocalNetwork => "Dev servers and devices on your Wi-Fi, in the browser and terminals.",
            Permission::Camera => "Pages and programs that use them are asked for once.",
            Permission::Files => "Desktop, Documents, Downloads and drives are asked for once each.",
            Permission::Notifications => "When an agent finishes or needs you while you look elsewhere.",
        }
    }

    /// Whether Deck has it, where an app can tell.
    pub fn state(self) -> State {
        match self {
            Permission::FullDisk => full_disk(),
            Permission::Accessibility => {
                if unsafe { AXIsProcessTrusted() } {
                    State::On
                } else {
                    State::Off
                }
            }
            Permission::Screen => {
                if unsafe { CGPreflightScreenCaptureAccess() } {
                    State::On
                } else {
                    State::Off
                }
            }
            _ => State::Unknown,
        }
    }

    /// Whether Deck can ask for it itself: macOS then shows its own prompt
    /// and lists Deck in the pane, ready to switch on.
    pub fn askable(self) -> bool {
        matches!(self, Permission::Accessibility | Permission::Screen)
    }

    pub fn ask(self) {
        match self {
            Permission::Accessibility => ask_accessibility(),
            Permission::Screen => unsafe {
                CGRequestScreenCaptureAccess();
            },
            _ => {}
        }
    }

    /// Its pane in System Settings.
    pub fn pane(self) -> &'static str {
        match self {
            Permission::FullDisk => "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
            Permission::Accessibility => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            Permission::Screen => "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            Permission::DevTools => "x-apple.systempreferences:com.apple.preference.security?Privacy_DevTools",
            Permission::AppManagement => "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles",
            Permission::Automation => "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation",
            Permission::LocalNetwork => "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork",
            Permission::Camera => "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera",
            Permission::Files => "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders",
            Permission::Notifications => "x-apple.systempreferences:com.apple.Notifications-Settings.extension",
        }
    }

    pub fn open(self) {
        let _ = std::process::Command::new("/usr/bin/open").arg(self.pane()).spawn();
    }
}

/// Whether the background service has Full Disk Access: it runs the
/// terminals and agents that outlive Deck, and macOS counts it as a program
/// of its own, so Deck having it is not enough. `Unknown` while it is not
/// running (or is a build that cannot say).
pub fn service_full_disk() -> State {
    match crate::daemon::ask(&serde_json::json!({ "op": "privacy" })) {
        Ok(v) => match v.get("full_disk").and_then(serde_json::Value::as_bool) {
            Some(true) => State::On,
            Some(false) => State::Off,
            None => State::Unknown,
        },
        Err(_) => State::Unknown,
    }
}

/// The Full Disk Access pane, and the service's program shown in Finder
/// beside it, to drag into the list (or add with +).
pub fn grant_service() {
    Permission::FullDisk.open();
    let exe = crate::control::socket_dir().join("bin").join("deckd");
    let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(exe).spawn();
}

/// Only an app with Full Disk Access can open the system's own privacy
/// database; trying is how to tell, and reads nothing.
pub fn full_disk() -> State {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return State::Unknown;
    };
    match std::fs::File::open(home.join("Library/Application Support/com.apple.TCC/TCC.db")) {
        Ok(_) => State::On,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => State::Off,
        Err(_) => State::Unknown,
    }
}

type CFTypeRef = *const std::ffi::c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const u8,
        value_callbacks: *const u8,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

/// macOS's own "Deck would like to control this computer" prompt, which
/// also lists Deck in the Accessibility pane.
fn ask_accessibility() {
    // SAFETY: CoreFoundation's documented constants and calls; the
    // dictionary is released after use.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let dict = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        if !dict.is_null() {
            AXIsProcessTrustedWithOptions(dict);
            CFRelease(dict);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_has_a_pane_and_a_reason() {
        for p in Permission::MANUAL.iter().chain(Permission::ASKED.iter()) {
            assert!(p.pane().starts_with("x-apple.systempreferences:"), "{p:?}");
            assert!(p.why().ends_with('.'), "{p:?}");
            // Checking never prompts or fails.
            let _ = p.state();
        }
        assert!(Permission::Accessibility.askable() && !Permission::FullDisk.askable());
    }
}
