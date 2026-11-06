//! Where Deck keeps its files on each platform. `DECK_DATA_DIR` moves the
//! data folder, so a test instance never touches the user's own.

use std::path::PathBuf;

fn home() -> PathBuf {
    home_dir().unwrap_or_else(std::env::temp_dir)
}

/// The user's home folder: `HOME`, else `USERPROFILE` (Windows sets only
/// that one).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// Settings, threads, sockets and the background service's state:
/// `~/Library/Application Support/deck` on macOS, `$XDG_DATA_HOME/deck`
/// (`~/.local/share/deck`) on Linux, `%APPDATA%\deck` on Windows.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DECK_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        return home().join("Library/Application Support/deck");
    }
    if cfg!(windows) {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return PathBuf::from(dir).join("deck");
        }
    }
    xdg("XDG_DATA_HOME", ".local/share").join("deck")
}

/// What can be thrown away and fetched again: `~/Library/Caches/deck`,
/// `$XDG_CACHE_HOME/deck` (`~/.cache/deck`), `%LOCALAPPDATA%\deck\cache`.
pub fn cache_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home().join("Library/Caches/deck");
    }
    if cfg!(windows) {
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(dir).join("deck").join("cache");
        }
    }
    xdg("XDG_CACHE_HOME", ".cache").join("deck")
}

/// An XDG base directory: the variable when it holds an absolute path,
/// else its default under the home folder.
fn xdg(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(default))
}
