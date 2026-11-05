//! Lock (⇧⌘L) off macOS. On Linux the unlock asks polkit, which shows the
//! desktop's own password prompt through its authentication agent. Windows
//! has no prompt wired up yet, so the lock screen there only hides the
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
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!("lock: no system authentication on this platform; unlocking");
        true
    }
}
