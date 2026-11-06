//! A secret kept by the system for this user, by service name: the
//! Keychain through `security` on macOS, Credential Manager on Windows, the
//! Secret Service through `secret-tool` on Linux (a file only this user can
//! read when no Secret Service runs).

/// The secret stored for `service`, if any.
pub fn get(service: &str) -> Option<String> {
    imp::get(service).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub fn set(service: &str, secret: &str) -> Result<(), String> {
    imp::set(service, secret)
}

pub fn delete(service: &str) {
    imp::delete(service)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::{Command, Stdio};

    pub fn get(service: &str) -> Option<String> {
        let out = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The secret goes in through `security`'s arguments, so it shows in the
    /// process list for the moment the call takes, to this user only.
    pub fn set(service: &str, secret: &str) -> Result<(), String> {
        let ok = Command::new("/usr/bin/security")
            .args(["add-generic-password", "-U", "-a", "deck", "-s", service, "-w", secret])
            .status()
            .map_err(|e| e.to_string())?
            .success();
        ok.then_some(()).ok_or_else(|| "the Keychain refused it".into())
    }

    pub fn delete(service: &str) {
        let _ = Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-s", service])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::{HSTRING, PWSTR};
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    pub fn get(service: &str) -> Option<String> {
        let mut found: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: CredReadW fills `found` with a block CredFree releases;
        // the blob is read before that.
        unsafe {
            CredReadW(&HSTRING::from(service), CRED_TYPE_GENERIC, None, &mut found).ok()?;
            let c = &*found;
            let blob = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
            CredFree(found as *const _);
            String::from_utf8(blob).ok()
        }
    }

    /// Kept for this user on this machine; Windows encrypts it with the
    /// user's logon key.
    pub fn set(service: &str, secret: &str) -> Result<(), String> {
        let mut target: Vec<u16> = service.encode_utf16().chain([0]).collect();
        let mut user: Vec<u16> = "deck".encode_utf16().chain([0]).collect();
        let mut blob = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: every pointer in `cred` outlives the call.
        unsafe { CredWriteW(&cred, 0) }.map_err(|e| format!("Credential Manager refused it: {e}"))
    }

    pub fn delete(service: &str) {
        // SAFETY: a plain call with an owned string.
        let _ = unsafe { CredDeleteW(&HSTRING::from(service), CRED_TYPE_GENERIC, None) };
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    fn file(service: &str) -> PathBuf {
        agent_core::dirs::data_dir().join("secrets").join(service)
    }

    pub fn get(service: &str) -> Option<String> {
        let out = Command::new("secret-tool")
            .args(["lookup", "service", service])
            .stderr(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
            _ => std::fs::read_to_string(file(service)).ok(),
        }
    }

    /// Through `secret-tool`'s input, not its arguments, so it never shows
    /// in the process list.
    pub fn set(service: &str, secret: &str) -> Result<(), String> {
        let stored = Command::new("secret-tool")
            .args(["store", "--label", &format!("Deck ({service})"), "service", service])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut c| {
                c.stdin.take().map(|mut i| i.write_all(secret.as_bytes())).transpose()?;
                c.wait()
            })
            .is_ok_and(|s| s.success());
        if stored {
            let _ = std::fs::remove_file(file(service));
            return Ok(());
        }
        let path = file(service);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            let _ = crate::os::private(dir);
        }
        crate::os::create_private(&path)
            .and_then(|mut f| f.write_all(secret.as_bytes()))
            .map_err(|e| e.to_string())
    }

    pub fn delete(service: &str) {
        let _ = Command::new("secret-tool")
            .args(["clear", "service", service])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(file(service));
    }
}
