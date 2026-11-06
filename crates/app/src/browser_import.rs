//! Bring in what another browser knows: history and bookmarks (so the
//! address bar completes your usual sites), and its cookies, so sites you are
//! signed in to there are signed in here too.
//!
//! Chromium browsers (Chrome, Arc, Brave, Edge, Vivaldi) keep history and
//! cookies in SQLite and bookmarks in JSON. Their cookies are encrypted with
//! a key in the login keychain ("Chrome Safe Storage"), so macOS asks once
//! to let Deck read it. Safari's history and bookmarks need Full Disk Access
//! for Deck; its cookies are sealed in Safari's container and are not read.
//! Saved passwords are not imported.
//!
//! On Windows the key is in the profile's `Local State`, sealed with DPAPI
//! for the account; Chrome 127 and later seals new cookies to Chrome itself
//! (app-bound), and those are skipped.
//!
//! Databases are copied before reading, so a running browser is never
//! disturbed, and read with the system's `sqlite3` (on Windows, the
//! `winsqlite3.dll` it ships). Windows keeps a running browser's files
//! locked, so there it has to be closed first.

use crate::history::Visit;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub name: &'static str,
    /// A Chromium profile folder, or Safari's `~/Library/Safari`.
    pub dir: PathBuf,
    /// The keychain item holding a Chromium browser's cookie key.
    keychain: Option<&'static str>,
}

impl Source {
    pub fn safari(&self) -> bool {
        self.keychain.is_none()
    }
}

/// The browsers installed with a profile to read, most common first.
/// Their cookie key sits in `Local State` beside the profile, sealed with
/// DPAPI for this Windows account.
#[cfg(windows)]
pub fn sources() -> Vec<Source> {
    let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else { return Vec::new() };
    let chromium: [(&str, &str); 5] = [
        ("Chrome", r"Google\Chrome\User Data"),
        ("Edge", r"Microsoft\Edge\User Data"),
        ("Brave", r"BraveSoftware\Brave-Browser\User Data"),
        ("Vivaldi", r"Vivaldi\User Data"),
        ("Chromium", r"Chromium\User Data"),
    ];
    chromium
        .iter()
        .map(|(name, dir)| Source { name, dir: local.join(dir).join("Default"), keychain: Some("Local State") })
        .filter(|s| s.dir.join("History").exists())
        .collect()
}

/// The browsers installed with a profile to read, most common first.
#[cfg(not(windows))]
pub fn sources() -> Vec<Source> {
    let Some(home) = agent_core::dirs::home_dir() else { return Vec::new() };
    let support = home.join("Library/Application Support");
    let chromium: [(&str, &str, &str); 6] = [
        ("Chrome", "Google/Chrome", "Chrome Safe Storage"),
        ("Arc", "Arc/User Data", "Arc Safe Storage"),
        ("Brave", "BraveSoftware/Brave-Browser", "Brave Safe Storage"),
        ("Edge", "Microsoft Edge", "Microsoft Edge Safe Storage"),
        ("Vivaldi", "Vivaldi", "Vivaldi Safe Storage"),
        ("Chromium", "Chromium", "Chromium Safe Storage"),
    ];
    let mut out: Vec<Source> = chromium
        .iter()
        .map(|(name, dir, key)| Source {
            name,
            dir: support.join(dir).join("Default"),
            keychain: Some(key),
        })
        .filter(|s| s.dir.join("History").exists())
        .collect();
    let safari = home.join("Library/Safari");
    if safari.join("History.db").exists() {
        out.push(Source { name: "Safari", dir: safari, keychain: None });
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub history: usize,
    pub bookmarks: usize,
    pub cookies: usize,
    /// What could not be read, in words.
    pub notes: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.history > 0 {
            parts.push(format!("{} pages", self.history));
        }
        if self.bookmarks > 0 {
            parts.push(format!("{} bookmarks", self.bookmarks));
        }
        if self.cookies > 0 {
            parts.push(format!("{} sign-in cookies", self.cookies));
        }
        let mut s = if parts.is_empty() { "Nothing imported".to_string() } else { format!("Imported {}", parts.join(", ")) };
        for n in &self.notes {
            s.push_str(". ");
            s.push_str(n);
        }
        s
    }
}

/// A cookie, ready to hand to WebKit.
#[derive(Clone, Debug, PartialEq)]
pub struct Cookie {
    pub domain: String,
    pub name: String,
    pub value: String,
    pub path: String,
    /// Unix seconds; `None` for a session cookie.
    pub expires: Option<f64>,
    pub secure: bool,
    pub http_only: bool,
}

/// History and bookmarks into Deck's history, and the cookies read (to be
/// installed on the main thread with [`install_cookies`]). Blocking: run it
/// off the main thread. The keychain prompt shows during it.
pub fn import(src: &Source, cookies: bool) -> (Report, Vec<Cookie>) {
    let mut report = Report::default();
    let mut visits = Vec::new();
    match read_history(src) {
        Ok(v) => {
            report.history = v.len();
            visits.extend(v);
        }
        Err(e) => report.notes.push(e),
    }
    match read_bookmarks(src) {
        Ok(v) => {
            report.bookmarks = v.len();
            visits.extend(v);
        }
        Err(e) => report.notes.push(e),
    }
    crate::history::import(visits);
    let mut jar = Vec::new();
    if cookies && !src.safari() {
        match read_cookies(src) {
            Ok((c, note)) => {
                report.cookies = c.len();
                jar = c;
                report.notes.extend(note);
            }
            Err(e) => report.notes.push(e),
        }
    } else if cookies {
        match read_safari_cookies() {
            Ok(c) => {
                report.cookies = c.len();
                jar = c;
            }
            Err(e) => report.notes.push(e),
        }
    }
    // Windows keeps a running browser's databases locked.
    let mut locked = false;
    report.notes.retain(|n| {
        let hit = n == LOCKED;
        locked |= hit;
        !hit
    });
    if locked {
        report.notes.push(format!("{0} keeps its files locked while it runs: close {0} first", src.name));
    }
    (report, jar)
}

/// `query`'s error for a database another process holds open.
const LOCKED: &str = "locked";

/// The rows of `sql` against a copy of `db`, fields split on 0x1f.
fn query(db: &Path, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let tmp = std::env::temp_dir().join(format!("deck-import-{}-{}", std::process::id(), db.file_name().and_then(|n| n.to_str()).unwrap_or("db")));
    std::fs::copy(db, &tmp).map_err(|e| {
        // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
        if cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)) {
            LOCKED.to_string()
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            "Deck needs Full Disk Access to read this (System Settings › Privacy & Security)".to_string()
        } else {
            format!("could not read {}: {e}", db.display())
        }
    })?;
    let wal = db.with_extension("db-wal");
    let wal = if wal.exists() { wal } else { PathBuf::from(format!("{}-wal", db.display())) };
    if wal.exists() {
        let _ = std::fs::copy(&wal, format!("{}-wal", tmp.display()));
    }
    let rows = run_sql(&tmp, sql);
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
    #[cfg(windows)]
    let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    rows
}

#[cfg(windows)]
fn run_sql(tmp: &Path, sql: &str) -> Result<Vec<Vec<String>>, String> {
    win::rows(tmp, sql)
}

/// `sql` against `tmp` with the system's `sqlite3`.
#[cfg(not(windows))]
fn run_sql(tmp: &Path, sql: &str) -> Result<Vec<Vec<String>>, String> {
    let out = Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-separator", "\u{1f}", "-newline", "\u{1e}"])
        .arg(&tmp)
        .arg(sql)
        .output()
        .map_err(|e| format!("sqlite3: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\u{1e}')
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\u{1f}').map(str::to_string).collect())
        .collect())
}

fn read_history(src: &Source) -> Result<Vec<Visit>, String> {
    let rows = if src.safari() {
        query(
            &src.dir.join("History.db"),
            "SELECT i.url, COALESCE(MAX(v.title),''), i.visit_count, MAX(v.visit_time) FROM history_items i \
             JOIN history_visits v ON v.history_item = i.id GROUP BY i.id ORDER BY 4 DESC LIMIT 3000",
        )?
    } else {
        query(
            &src.dir.join("History"),
            "SELECT url, title, visit_count, last_visit_time FROM urls WHERE hidden = 0 ORDER BY last_visit_time DESC LIMIT 3000",
        )?
    };
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let [url, title, visits, at] = <[String; 4]>::try_from(r).ok()?;
            let at: f64 = at.parse().ok()?;
            // Safari: seconds since 2001; Chromium: µs since 1601.
            let last = if src.safari() {
                ((at + 978_307_200.) * 1000.) as i64
            } else {
                (at / 1000.) as i64 - 11_644_473_600_000
            };
            Some(Visit {
                url,
                title,
                visits: visits.parse().unwrap_or(1).max(1),
                last,
            })
        })
        .collect())
}

fn read_bookmarks(src: &Source) -> Result<Vec<Visit>, String> {
    let json: serde_json::Value = if src.safari() {
        let out = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(src.dir.join("Bookmarks.plist"))
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?
    } else {
        match std::fs::read(src.dir.join("Bookmarks")) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| e.to_string())?,
            Err(_) => return Ok(Vec::new()),
        }
    };
    let mut out = Vec::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    fn walk(v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        // Chromium: {type: "url", url, name}; Safari: {URLString, URIDictionary: {title}}.
        if let Some(url) = v.get("url").or_else(|| v.get("URLString")).and_then(|u| u.as_str()) {
            let title = v
                .get("name")
                .or_else(|| v.get("URIDictionary").and_then(|d| d.get("title")))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            out.push((url.to_string(), title.to_string()));
        }
        for key in ["children", "Children"] {
            for c in v.get(key).and_then(|c| c.as_array()).into_iter().flatten() {
                walk(c, out);
            }
        }
        if let Some(roots) = v.get("roots").and_then(|r| r.as_object()) {
            for r in roots.values() {
                walk(r, out);
            }
        }
    }
    let mut found = Vec::new();
    walk(&json, &mut found);
    // A bookmark counts as a site you go to often.
    out.extend(
        found
            .into_iter()
            .filter(|(u, _)| u.starts_with("http"))
            .map(|(url, title)| Visit { url, title, visits: 8, last: now }),
    );
    Ok(out)
}

/// The cookies a Chromium profile holds, and a note on any it could not
/// open.
fn read_cookies(src: &Source) -> Result<(Vec<Cookie>, Option<String>), String> {
    let db = [src.dir.join("Cookies"), src.dir.join("Network").join("Cookies")]
        .into_iter()
        .find(|p| p.exists())
        .ok_or("no cookie database")?;
    let key = cookie_key(src)?;
    // Since Chrome 130 (meta version 24) each value starts with a 32-byte
    // hash of its domain.
    let version: u32 = query(&db, "SELECT value FROM meta WHERE key = 'version'")?
        .first()
        .and_then(|r| r.first())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rows = query(
        &db,
        "SELECT host_key, name, path, expires_utc, is_secure, is_httponly, hex(encrypted_value), value FROM cookies",
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64());
    let mut jar = Vec::new();
    let mut sealed = 0;
    for r in rows {
        let Ok([domain, name, path, expires, secure, http_only, enc, plain]) = <[String; 8]>::try_from(r) else {
            continue;
        };
        let expires: f64 = expires.parse().unwrap_or(0.);
        let expires = (expires > 0.).then(|| expires / 1e6 - 11_644_473_600.);
        if expires.is_some_and(|e| e < now) {
            continue;
        }
        let value = if !plain.is_empty() {
            plain
        } else {
            let Some(bytes) = unhex(&enc) else { continue };
            let mut clear = match open_value(&key, &bytes) {
                Opened::Clear(c) => c,
                Opened::Sealed => {
                    sealed += 1;
                    continue;
                }
                Opened::Unreadable => continue,
            };
            if version >= 24 && clear.len() >= 32 {
                clear.drain(..32);
            }
            match String::from_utf8(clear) {
                Ok(v) => v,
                Err(_) => continue,
            }
        };
        jar.push(Cookie {
            domain,
            name,
            value,
            path: if path.is_empty() { "/".into() } else { path },
            expires,
            secure: secure == "1",
            http_only: http_only == "1",
        });
    }
    let note = (sealed > 0).then(|| {
        format!(
            "{sealed} of {}'s sign-ins are sealed to {0} itself (app-bound encryption, Chrome 127 and later) and were skipped: sign in to those sites again here",
            src.name
        )
    });
    Ok((jar, note))
}

enum Opened {
    Clear(Vec<u8>),
    /// Readable only by the browser that wrote it.
    #[cfg_attr(not(windows), allow(dead_code))]
    Sealed,
    Unreadable,
}

/// Chromium's cookie key on macOS: from the keychain item `src` names.
#[cfg(not(windows))]
type Key = [u8; 16];

#[cfg(not(windows))]
fn cookie_key(src: &Source) -> Result<Key, String> {
    let service = src.keychain.ok_or("no keychain item")?;
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-w", "-s", service])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("sign-ins skipped: the keychain did not give out \"{service}\""));
    }
    let password = String::from_utf8_lossy(&out.stdout).trim().to_string();
    derive_key(password.as_bytes()).ok_or_else(|| "could not derive the cookie key".into())
}

#[cfg(not(windows))]
fn open_value(key: &Key, bytes: &[u8]) -> Opened {
    match bytes.strip_prefix(b"v10").and_then(|body| decrypt(key, body)) {
        Some(clear) => Opened::Clear(clear),
        None => Opened::Unreadable,
    }
}

/// Chromium's cookie key on Windows: `os_crypt.encrypted_key` in the
/// profile's `Local State`, base64 of "DPAPI" and a DPAPI blob for this
/// account, 32 bytes for AES-256-GCM.
#[cfg(windows)]
type Key = Vec<u8>;

#[cfg(windows)]
fn cookie_key(src: &Source) -> Result<Key, String> {
    let state = src.dir.parent().ok_or("no profile folder")?.join("Local State");
    let json: serde_json::Value = std::fs::read(&state)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| format!("sign-ins skipped: could not read {}'s Local State", src.name))?;
    let sealed = json["os_crypt"]["encrypted_key"]
        .as_str()
        .and_then(unbase64)
        .ok_or("sign-ins skipped: no cookie key in Local State")?;
    let blob = sealed.strip_prefix(b"DPAPI").ok_or("sign-ins skipped: the cookie key is not DPAPI-sealed")?;
    let key = win::unprotect(blob).ok_or("sign-ins skipped: Windows did not unseal the cookie key")?;
    if key.len() != 32 {
        return Err("sign-ins skipped: the cookie key is not 32 bytes".into());
    }
    Ok(key)
}

/// `v10`/`v11`: AES-256-GCM, a 12-byte nonce, the text, a 16-byte tag.
/// `v20` is app-bound: only the browser's own elevated service opens it.
#[cfg(windows)]
fn open_value(key: &Key, bytes: &[u8]) -> Opened {
    if bytes.starts_with(b"v20") {
        return Opened::Sealed;
    }
    let Some(body) = bytes.strip_prefix(b"v10").or_else(|| bytes.strip_prefix(b"v11")) else {
        // Before Chrome 80: the value is a DPAPI blob of its own.
        return win::unprotect(bytes).map_or(Opened::Unreadable, Opened::Clear);
    };
    if body.len() < 12 + 16 {
        return Opened::Unreadable;
    }
    let (nonce, rest) = body.split_at(12);
    let (text, tag) = rest.split_at(rest.len() - 16);
    win::aes_gcm_open(key, nonce, text, tag).map_or(Opened::Unreadable, Opened::Clear)
}

/// Standard base64, padding optional; `None` on anything else.
#[cfg(windows)]
fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(windows)]
mod win {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use windows::core::{s, w, PCWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptDecrypt, BCryptDestroyKey, BCryptGenerateSymmetricKey,
        BCryptOpenAlgorithmProvider, BCryptSetProperty, CryptUnprotectData, BCRYPT_AES_ALGORITHM,
        BCRYPT_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
        BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_CHAINING_MODE, BCRYPT_FLAGS,
        BCRYPT_KEY_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS, CRYPT_INTEGER_BLOB,
    };
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    /// DPAPI's CryptUnprotectData, for the signed-in account.
    pub fn unprotect(blob: &[u8]) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `blob` for the call; `out` is DPAPI's
        // LocalAlloc'd buffer, copied and then freed.
        unsafe {
            CryptUnprotectData(&input, None, None, None, None, 0, &mut out).ok()?;
            let clear = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(Some(HLOCAL(out.pbData as *mut c_void)));
            Some(clear)
        }
    }

    /// AES-GCM through CNG: `text` decrypted when `tag` checks out.
    pub fn aes_gcm_open(key: &[u8], nonce: &[u8], text: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        // SAFETY: CNG handles opened and closed here; every buffer outlives
        // the call that reads or writes it.
        unsafe {
            if BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_AES_ALGORITHM, PCWSTR::null(), BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0)).is_err() {
                return None;
            }
            let gcm: Vec<u8> = "ChainingModeGCM\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut out = None;
            if BCryptSetProperty(alg.into(), BCRYPT_CHAINING_MODE, &gcm, 0).is_ok() {
                let mut hkey = BCRYPT_KEY_HANDLE::default();
                if BCryptGenerateSymmetricKey(alg, &mut hkey, None, key, 0).is_ok() {
                    let mut nonce = nonce.to_vec();
                    let mut tag = tag.to_vec();
                    let info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
                        cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
                        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
                        pbNonce: nonce.as_mut_ptr(),
                        cbNonce: nonce.len() as u32,
                        pbTag: tag.as_mut_ptr(),
                        cbTag: tag.len() as u32,
                        ..Default::default()
                    };
                    let mut clear = vec![0u8; text.len()];
                    let mut n = 0u32;
                    let st = BCryptDecrypt(
                        hkey,
                        Some(text),
                        Some(&info as *const _ as *const c_void),
                        None,
                        Some(&mut clear),
                        &mut n,
                        BCRYPT_FLAGS(0),
                    );
                    if st.is_ok() {
                        clear.truncate(n as usize);
                        out = Some(clear);
                    }
                    let _ = BCryptDestroyKey(hkey);
                }
            }
            let _ = BCryptCloseAlgorithmProvider(alg, 0);
            out
        }
    }

    type Open = unsafe extern "system" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
    type Prepare = unsafe extern "system" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int;
    type Step = unsafe extern "system" fn(*mut c_void) -> c_int;
    type Count = unsafe extern "system" fn(*mut c_void) -> c_int;
    type Text = unsafe extern "system" fn(*mut c_void, c_int) -> *const u8;
    type Bytes = unsafe extern "system" fn(*mut c_void, c_int) -> c_int;
    type Finalize = unsafe extern "system" fn(*mut c_void) -> c_int;
    type Close = unsafe extern "system" fn(*mut c_void) -> c_int;
    type ErrMsg = unsafe extern "system" fn(*mut c_void) -> *const c_char;

    /// The rows of `sql` against `db`, through `winsqlite3.dll`, the SQLite
    /// every Windows 10 and later ships, loaded when first needed.
    pub fn rows(db: &std::path::Path, sql: &str) -> Result<Vec<Vec<String>>, String> {
        const OK: c_int = 0;
        const ROW: c_int = 100;
        const DONE: c_int = 101;
        // SQLITE_OPEN_READWRITE: it is our own copy, and WAL wants to write
        // its index beside it.
        const OPEN_READWRITE: c_int = 0x2;
        let path = CString::new(db.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        let sql = CString::new(sql).map_err(|e| e.to_string())?;
        // SAFETY: the functions are SQLite's C API with the signatures
        // above; the connection and statement live only inside this block.
        unsafe {
            let lib = LoadLibraryW(w!("winsqlite3.dll")).map_err(|e| format!("winsqlite3: {e}"))?;
            macro_rules! f {
                ($name:literal, $ty:ty) => {
                    std::mem::transmute::<_, $ty>(GetProcAddress(lib, s!($name)).ok_or(concat!("winsqlite3 has no ", $name))?)
                };
            }
            let open = f!("sqlite3_open_v2", Open);
            let prepare = f!("sqlite3_prepare_v2", Prepare);
            let step = f!("sqlite3_step", Step);
            let count = f!("sqlite3_column_count", Count);
            let text = f!("sqlite3_column_text", Text);
            let bytes = f!("sqlite3_column_bytes", Bytes);
            let finalize = f!("sqlite3_finalize", Finalize);
            let close = f!("sqlite3_close", Close);
            let errmsg = f!("sqlite3_errmsg", ErrMsg);
            let err = |conn: *mut c_void| {
                let m = errmsg(conn);
                if m.is_null() { "sqlite error".to_string() } else { CStr::from_ptr(m).to_string_lossy().into_owned() }
            };
            let mut conn = std::ptr::null_mut();
            if open(path.as_ptr(), &mut conn, OPEN_READWRITE, std::ptr::null()) != OK {
                let e = err(conn);
                close(conn);
                return Err(e);
            }
            let mut stmt = std::ptr::null_mut();
            if prepare(conn, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) != OK {
                let e = err(conn);
                close(conn);
                return Err(e);
            }
            let cols = count(stmt);
            let mut out = Vec::new();
            let result = loop {
                match step(stmt) {
                    ROW => out.push(
                        (0..cols)
                            .map(|i| {
                                let p = text(stmt, i);
                                let n = bytes(stmt, i).max(0) as usize;
                                if p.is_null() {
                                    String::new()
                                } else {
                                    String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned()
                                }
                            })
                            .collect(),
                    ),
                    DONE => break Ok(out),
                    _ => break Err(err(conn)),
                }
            };
            finalize(stmt);
            close(conn);
            result
        }
    }
}

/// Safari's cookies, from its `Cookies.binarycookies` (inside Safari's
/// container: needs Full Disk Access for Deck).
fn read_safari_cookies() -> Result<Vec<Cookie>, String> {
    let home = agent_core::dirs::home_dir().ok_or("no home folder")?;
    let paths = [
        home.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies"),
        home.join("Library/Cookies/Cookies.binarycookies"),
    ];
    let mut denied = false;
    for p in &paths {
        match std::fs::read(p) {
            Ok(bytes) => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0., |d| d.as_secs_f64());
                return Ok(parse_binarycookies(&bytes).into_iter().filter(|c| c.expires.is_none_or(|e| e > now)).collect());
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            Err(_) => {}
        }
    }
    Err(if denied {
        "Safari's sign-ins need Full Disk Access for Deck (System Settings › Privacy & Security)".into()
    } else {
        "no Safari cookies found".into()
    })
}

/// `Cookies.binarycookies`: "cook", page count and sizes (big-endian), then
/// pages of cookie records (little-endian), each with its flags, the
/// offsets of its domain, name, path and value, and its expiry in seconds
/// since 2001.
fn parse_binarycookies(b: &[u8]) -> Vec<Cookie> {
    let be = |at: usize| -> Option<u32> { Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)) };
    let mut out = Vec::new();
    if b.get(0..4) != Some(b"cook") {
        return out;
    }
    let Some(pages) = be(4) else { return out };
    let mut sizes = Vec::new();
    for i in 0..pages as usize {
        match be(8 + i * 4) {
            Some(n) => sizes.push(n as usize),
            None => return out,
        }
    }
    let mut at = 8 + pages as usize * 4;
    for size in sizes {
        let Some(page) = b.get(at..at + size) else { break };
        at += size;
        let le = |p: &[u8], o: usize| -> Option<u32> { Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?)) };
        let Some(n) = le(page, 4) else { continue };
        for i in 0..n as usize {
            let Some(off) = le(page, 8 + i * 4) else { break };
            let off = off as usize;
            let Some(len) = le(page, off) else { continue };
            let Some(rec) = page.get(off..off + len as usize) else { continue };
            let flags = le(rec, 8).unwrap_or(0);
            let text = |o: usize| -> String {
                let start = le(rec, o).unwrap_or(0) as usize;
                rec.get(start..)
                    .map(|t| String::from_utf8_lossy(&t[..t.iter().position(|c| *c == 0).unwrap_or(t.len())]).into_owned())
                    .unwrap_or_default()
            };
            let expiry = rec.get(40..48).and_then(|x| x.try_into().ok()).map(f64::from_le_bytes).unwrap_or(0.);
            let domain = text(16);
            if domain.is_empty() {
                continue;
            }
            out.push(Cookie {
                domain,
                name: text(20),
                path: Some(text(24)).filter(|p| !p.is_empty()).unwrap_or_else(|| "/".into()),
                value: text(28),
                expires: (expiry > 0.).then_some(expiry + 978_307_200.),
                secure: flags & 1 != 0,
                http_only: flags & 4 != 0,
            });
        }
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(target_os = "macos")]
#[link(name = "System", kind = "dylib")]
extern "C" {
    fn CCKeyDerivationPBKDF(
        algorithm: u32,
        password: *const u8,
        password_len: usize,
        salt: *const u8,
        salt_len: usize,
        prf: u32,
        rounds: u32,
        derived: *mut u8,
        derived_len: usize,
    ) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn CCCrypt(
        op: u32,
        alg: u32,
        options: u32,
        key: *const u8,
        key_len: usize,
        iv: *const u8,
        data_in: *const u8,
        data_in_len: usize,
        data_out: *mut u8,
        data_out_avail: usize,
        data_out_moved: *mut usize,
    ) -> i32;
}

/// Chromium's cookie key on macOS: PBKDF2-SHA1 of the keychain password,
/// salt "saltysalt", 1003 rounds, 16 bytes.
#[cfg(target_os = "macos")]
fn derive_key(password: &[u8]) -> Option<[u8; 16]> {
    let mut key = [0u8; 16];
    let salt = b"saltysalt";
    // SAFETY: every pointer is valid for the length passed with it.
    let rc = unsafe {
        CCKeyDerivationPBKDF(2, password.as_ptr(), password.len(), salt.as_ptr(), salt.len(), 1, 1003, key.as_mut_ptr(), key.len())
    };
    (rc == 0).then_some(key)
}

/// Chromium's Linux store is not read yet.
#[cfg(not(any(target_os = "macos", windows)))]
fn derive_key(_password: &[u8]) -> Option<[u8; 16]> {
    None
}

#[cfg(not(any(target_os = "macos", windows)))]
fn decrypt(_key: &[u8; 16], _data: &[u8]) -> Option<Vec<u8>> {
    None
}

/// AES-128-CBC with an IV of sixteen spaces and PKCS#7 padding.
#[cfg(target_os = "macos")]
fn decrypt(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let iv = [b' '; 16];
    let mut out = vec![0u8; data.len() + 16];
    let mut moved = 0usize;
    // SAFETY: as above; `out` has room for the input plus a block.
    let rc = unsafe {
        CCCrypt(1, 0, 1, key.as_ptr(), key.len(), iv.as_ptr(), data.as_ptr(), data.len(), out.as_mut_ptr(), out.len(), &mut moved)
    };
    (rc == 0).then(|| {
        out.truncate(moved);
        out
    })
}

/// Hand the cookies to WebKit's default store, the one Deck's pages use.
/// Main thread only. On Linux there is no store of ours to fill yet.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn install_cookies(_jar: &[Cookie]) {}

#[cfg(windows)]
thread_local! {
    /// Cookies imported before any page was open, for the first one.
    static WAITING: std::cell::RefCell<Vec<Cookie>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Into WebView2's profile, which every page shares (one data folder):
/// through any open page's cookie manager, or the next page made.
#[cfg(windows)]
pub fn install_cookies(jar: &[Cookie]) {
    WAITING.with(|w| w.borrow_mut().extend_from_slice(jar));
    if let Some(page) = crate::browser::any_page() {
        install_waiting(&page);
    }
}

/// The cookies imported while no page was open, into `page`'s profile.
#[cfg(windows)]
pub fn install_waiting(page: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_2;
    use windows::core::{Interface, HSTRING};
    let jar = WAITING.with(|w| std::mem::take(&mut *w.borrow_mut()));
    if jar.is_empty() {
        return;
    }
    // SAFETY: WebView2 calls on the main thread, where the page lives.
    unsafe {
        let Ok(manager) = page.cast::<ICoreWebView2_2>().and_then(|p| p.CookieManager()) else {
            return;
        };
        for c in &jar {
            let made = manager.CreateCookie(
                &HSTRING::from(c.name.as_str()),
                &HSTRING::from(c.value.as_str()),
                &HSTRING::from(c.domain.as_str()),
                &HSTRING::from(c.path.as_str()),
            );
            let Ok(cookie) = made else { continue };
            if let Some(at) = c.expires {
                let _ = cookie.SetExpires(at);
            }
            let _ = cookie.SetIsSecure(c.secure);
            let _ = cookie.SetIsHttpOnly(c.http_only);
            let _ = manager.AddOrUpdateCookie(&cookie);
        }
    }
}

#[cfg(target_os = "macos")]
pub fn install_cookies(jar: &[Cookie]) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSDate, NSMutableDictionary, NSString};
    // SAFETY: plain Foundation and WebKit calls on the main thread.
    unsafe {
        let store: Option<Retained<AnyObject>> = msg_send![class!(WKWebsiteDataStore), defaultDataStore];
        let Some(store) = store else { return };
        let cookies: Option<Retained<AnyObject>> = msg_send![&*store, httpCookieStore];
        let Some(cookies) = cookies else { return };
        for c in jar {
            let props: Retained<NSMutableDictionary<NSString, AnyObject>> = NSMutableDictionary::new();
            let put = |k: &str, v: &AnyObject| {
                let key = NSString::from_str(k);
                let _: () = msg_send![&*props, setObject: v, forKey: &*key];
            };
            put("Domain", &NSString::from_str(&c.domain));
            put("Name", &NSString::from_str(&c.name));
            put("Value", &NSString::from_str(&c.value));
            put("Path", &NSString::from_str(&c.path));
            if c.secure {
                put("Secure", &NSString::from_str("TRUE"));
            }
            if c.http_only {
                put("HttpOnly", &NSString::from_str("TRUE"));
            }
            if let Some(at) = c.expires {
                let date = NSDate::dateWithTimeIntervalSince1970(at);
                put("Expires", &date);
            }
            let cookie: Option<Retained<AnyObject>> = msg_send![class!(NSHTTPCookie), cookieWithProperties: &*props];
            if let Some(cookie) = cookie {
                let none: *const AnyObject = std::ptr::null();
                let _: () = msg_send![&*cookies, setCookie: &*cookie, completionHandler: none];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safari_cookie_file_parses() {
        // One page with one cookie, built as Safari writes it.
        let mut rec = vec![0u8; 56];
        let strings = b"example.com\0sid\0/\0abc\0";
        let base = rec.len() as u32;
        rec.extend_from_slice(strings);
        let len = rec.len() as u32;
        rec[0..4].copy_from_slice(&len.to_le_bytes());
        rec[8..12].copy_from_slice(&5u32.to_le_bytes()); // secure + httponly
        rec[16..20].copy_from_slice(&base.to_le_bytes());
        rec[20..24].copy_from_slice(&(base + 12).to_le_bytes());
        rec[24..28].copy_from_slice(&(base + 16).to_le_bytes());
        rec[28..32].copy_from_slice(&(base + 18).to_le_bytes());
        rec[40..48].copy_from_slice(&900_000_000f64.to_le_bytes());
        let mut page = vec![0, 0, 1, 0];
        page.extend_from_slice(&1u32.to_le_bytes());
        page.extend_from_slice(&16u32.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]);
        page.extend_from_slice(&rec);
        let mut file = b"cook".to_vec();
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(&(page.len() as u32).to_be_bytes());
        file.extend_from_slice(&page);
        let c = parse_binarycookies(&file);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].domain.as_str(), c[0].name.as_str(), c[0].path.as_str(), c[0].value.as_str()), ("example.com", "sid", "/", "abc"));
        assert!(c[0].secure && c[0].http_only);
        assert_eq!(c[0].expires, Some(900_000_000. + 978_307_200.));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn chromium_cookies_decrypt() {
        // Encrypt with the same scheme, then read it back.
        let key = derive_key(b"peanuts").unwrap();
        let iv = [b' '; 16];
        let plain = b"session=abc123";
        let mut enc = vec![0u8; 64];
        let mut moved = 0usize;
        let rc = unsafe {
            CCCrypt(0, 0, 1, key.as_ptr(), 16, iv.as_ptr(), plain.as_ptr(), plain.len(), enc.as_mut_ptr(), enc.len(), &mut moved)
        };
        assert_eq!(rc, 0);
        enc.truncate(moved);
        assert_eq!(decrypt(&key, &enc).unwrap(), plain);
        assert_eq!(unhex("7631"), Some(b"v1".to_vec()));
    }
}

#[cfg(test)]
mod real {
    /// Reads this Mac's browsers' history and bookmarks, never cookies:
    /// `cargo test -- --ignored real_browsers --nocapture`.
    #[test]
    #[ignore]
    fn real_browsers() {
        for s in super::sources() {
            let h = super::read_history(&s).map(|v| v.len());
            let b = super::read_bookmarks(&s).map(|v| v.len());
            println!("{}: history {h:?} bookmarks {b:?}", s.name);
        }
    }
}
