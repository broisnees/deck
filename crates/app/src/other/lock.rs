//! Lock (⇧⌘L) off macOS. On Linux the unlock asks polkit, which shows the
//! desktop's own password prompt through its authentication agent. On
//! Windows it asks Windows Hello (face, fingerprint or PIN), parented to
//! Deck's window; with Hello not set up, the lock screen only hides the
//! window's contents.

/// Ask the system to confirm it is you. True when it did.
pub async fn authenticate(reason: &str) -> bool {
    let _ = reason;
    #[cfg(target_os = "linux")]
    {
        let pid = std::process::id().to_string();
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let ok = std::process::Command::new("pkcheck")
                .args(["--action-id", "org.freedesktop.policykit.exec", "--allow-user-interaction", "--process", &pid])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            let _ = tx.send(ok);
        });
        rx.await.unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // Found here, on the main thread, while Deck is in front.
        let hwnd = crate::notify::deck_window().map(|h| h.0 as isize).unwrap_or(0);
        let reason = reason.to_string();
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(hello(hwnd, &reason));
        });
        rx.await.unwrap_or(false)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        tracing::warn!("lock: no system authentication on this platform; unlocking");
        true
    }
}

/// Windows Hello, blocking: off the main thread.
#[cfg(windows)]
fn hello(hwnd: isize, reason: &str) -> bool {
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
    use windows_future::IAsyncOperation;

    let available = UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join());
    match available {
        Ok(UserConsentVerifierAvailability::Available) => {}
        other => {
            tracing::warn!("lock: Windows Hello is not available ({other:?}); unlocking");
            return true;
        }
    }
    let message = HSTRING::from(reason);
    let result = (|| {
        let op: IAsyncOperation<UserConsentVerificationResult> = if hwnd != 0 {
            let interop = windows::core::factory::<UserConsentVerifier, IUserConsentVerifierInterop>()?;
            // SAFETY: Deck's own window, alive while it is locked.
            unsafe { interop.RequestVerificationForWindowAsync(HWND(hwnd as *mut _), &message)? }
        } else {
            UserConsentVerifier::RequestVerificationAsync(&message)?
        };
        op.join()
    })();
    match result {
        Ok(r) => r == UserConsentVerificationResult::Verified,
        Err(e) => {
            tracing::warn!("lock: Windows Hello failed: {e}");
            false
        }
    }
}
