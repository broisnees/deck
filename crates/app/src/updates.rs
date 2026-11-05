//! Updates: deck asks for its newest release (the backend's, when this build
//! has one; else the GitHub repository's), downloads it when you say so, and
//! swaps it in on restart. The swap runs from a small script outside deck,
//! once deck has quit, the way `scripts/install.sh` replaces a running copy.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB: &str = "https://api.github.com/repos/broisnischal/deck/releases/latest";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// What changed, as a page to open.
    pub notes_url: String,
    /// A zip of Deck.app, when the release has one.
    pub download: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum State {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(Release),
    /// Downloaded and unpacked here: a restart puts it in place.
    Ready(Release, PathBuf),
    Failed(String),
}

static STATE: Mutex<State> = Mutex::new(State::Idle);

pub fn state() -> State {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn set(s: State) {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

/// The newest release, when it is newer than this build.
pub async fn check() -> Result<Option<Release>, String> {
    let backend = crate::account::api_base();
    let url = match &backend {
        Some(api) => format!("{api}/v1/releases/latest"),
        None => GITHUB.to_string(),
    };
    let (status, body) = crate::net::request(
        "GET",
        &url,
        &[("Accept", "application/json"), ("User-Agent", "Deck")],
        None,
        15.,
    )
    .await
    .ok_or("Couldn't reach the update server. Check your connection.")?;
    if status == 404 {
        return Ok(None); // nothing released yet
    }
    if !(200..300).contains(&status) {
        return Err(match status {
            403 | 429 => {
                "GitHub is limiting requests from this network. Try again in a while.".into()
            }
            s => format!("The update server answered {s}."),
        });
    }
    let release = if backend.is_some() {
        parse_backend(&body)
    } else {
        parse_github(&body)
    }
    .ok_or("the update server's answer did not parse")?;
    Ok(newer(&release.version, VERSION).then_some(release))
}

fn parse_backend(body: &[u8]) -> Option<Release> {
    #[derive(Deserialize)]
    struct Latest {
        version: String,
        #[serde(default)]
        notes_url: String,
        #[serde(default)]
        download: Option<String>,
    }
    let l: Latest = serde_json::from_slice(body).ok()?;
    Some(Release {
        version: l.version.trim_start_matches('v').to_string(),
        notes_url: l.notes_url,
        download: l.download,
    })
}

fn parse_github(body: &[u8]) -> Option<Release> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    let l: Latest = serde_json::from_slice(body).ok()?;
    Some(Release {
        version: l.tag_name.trim_start_matches('v').to_string(),
        notes_url: l.html_url,
        // The zip holds Deck.app: only macOS installs it in place. Elsewhere
        // the release page is offered instead.
        download: l
            .assets
            .into_iter()
            .find(|a| cfg!(target_os = "macos") && a.name.ends_with(".zip"))
            .map(|a| a.browser_download_url),
    })
}

/// `a` is a later version than `b`, by its numbers ("0.10.0" > "0.9.3").
pub fn newer(a: &str, b: &str) -> bool {
    let nums = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map_while(|p| p.parse::<u64>().ok())
            .collect()
    };
    let (a, b) = (nums(a), nums(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Download the release and unpack it; returns the new Deck.app, checked to
/// be deck by its bundle id.
pub async fn download(r: &Release) -> Result<PathBuf, String> {
    let url = r
        .download
        .as_deref()
        .ok_or("this release has no app to download")?;
    let bytes = crate::net::get(url, 600.)
        .await
        .ok_or("the download failed")?;
    let dir = crate::control::socket_dir().join("update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join("Deck.zip");
    std::fs::write(&zip, bytes).map_err(|e| e.to_string())?;
    let ok = std::process::Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&zip)
        .arg(&dir)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if !ok {
        return Err("the download did not unpack".into());
    }
    let app = dir.join("Deck.app");
    if bundle_id(&app).as_deref() != Some(crate::handler::BUNDLE_ID) {
        return Err("the download is not Deck".into());
    }
    Ok(app)
}

fn bundle_id(app: &Path) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The app bundle this deck runs from.
pub fn running_app() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?.to_path_buf();
    app.extension().is_some_and(|e| e == "app").then_some(app)
}

/// Hand the swap to a script that waits for deck to quit, moves the new app
/// into place and (when `reopen`) opens it. The caller quits right after.
pub fn install_on_quit(new_app: &Path, reopen: bool) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let app = running_app().ok_or("deck is not running from an app bundle")?;
    let pid = std::process::id();
    let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; \
         rm -rf {old}; mv {app} {old} && mv {new} {app} && rm -rf {old}{open}",
        old = q(&app.with_extension("app.old")),
        app = q(&app),
        new = q(new_app),
        open = if reopen { format!("; open {}", q(&app)) } else { String::new() },
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own session, so it outlives deck.
    // SAFETY: setsid is async-signal-safe and touches nothing else.
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn libc_setsid() {
    extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: no arguments, no memory.
    unsafe {
        setsid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99"));
        assert!(newer("0.1.1", "0.1.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0-beta", "0.1.0"));
    }

    #[test]
    fn releases_parse_from_github_and_the_backend() {
        let gh = br#"{"tag_name":"v0.2.0","html_url":"https://x/r","assets":[{"name":"Deck.dmg","browser_download_url":"d"},{"name":"Deck-0.2.0.zip","browser_download_url":"z"}]}"#;
        let r = parse_github(gh).unwrap();
        assert_eq!(r.version, "0.2.0");
        if cfg!(target_os = "macos") {
            assert_eq!(r.download.as_deref(), Some("z"));
        } else {
            assert_eq!(r.download, None);
        }
        let b = br#"{"version":"0.3.1","notes_url":"n"}"#;
        assert_eq!(parse_backend(b).unwrap().download, None);
    }
}
