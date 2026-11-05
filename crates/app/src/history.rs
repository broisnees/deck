//! Pages visited in deck's browser, kept across launches. The address bar
//! completes and suggests from it, and the new tab page shows the sites you
//! go back to most.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Visit {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub visits: u32,
    /// Unix milliseconds of the last visit.
    #[serde(default)]
    pub last: i64,
}

/// Oldest, least visited pages fall off past this.
const MAX_ENTRIES: usize = 3000;

struct Store {
    entries: Vec<Visit>,
    /// Bumped on every change; a pending save only writes if no newer change
    /// has come in, so a burst of visits costs one write.
    generation: u64,
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| {
    let entries = std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    Mutex::new(Store {
        entries,
        generation: 0,
    })
});

fn path() -> PathBuf {
    crate::control::socket_dir().join("browser-history.json")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Search result pages are where you were going, not where you went.
fn worth_keeping(url: &str) -> bool {
    let rest = strip(url);
    (url.starts_with("http://") || url.starts_with("https://"))
        && !(rest.contains("google.") && rest.contains("/search?"))
        && !rest.starts_with("duckduckgo.com/?")
}

/// `https://www.github.com/a` → `github.com/a`: what typing is matched against.
pub fn strip(url: &str) -> &str {
    let s = url.split_once("://").map_or(url, |(_, r)| r);
    s.strip_prefix("www.").unwrap_or(s)
}

/// The host of a URL, without `www.`.
pub fn host(url: &str) -> &str {
    let s = strip(url);
    s.split(['/', '?', '#']).next().unwrap_or(s)
}

/// A page finished loading.
pub fn record(url: &str, title: &str) {
    if !worth_keeping(url) {
        return;
    }
    let generation = {
        let mut s = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let t = now();
        match s.entries.iter_mut().find(|v| v.url == url) {
            Some(v) => {
                v.visits += 1;
                v.last = t;
                if !title.is_empty() {
                    v.title = title.to_string();
                }
            }
            None => s.entries.push(Visit {
                url: url.to_string(),
                title: title.to_string(),
                visits: 1,
                last: t,
            }),
        }
        if s.entries.len() > MAX_ENTRIES {
            s.entries
                .sort_by(|a, b| score(b, t).total_cmp(&score(a, t)));
            s.entries.truncate(MAX_ENTRIES);
        }
        s.generation += 1;
        s.generation
    };
    save_later(generation);
}

/// The title arrived after the load finished (most pages set it late).
pub fn set_title(url: &str, title: &str) {
    if title.is_empty() {
        return;
    }
    let generation = {
        let mut s = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = s.entries.iter_mut().find(|v| v.url == url) else {
            return;
        };
        if v.title == title {
            return;
        }
        v.title = title.to_string();
        s.generation += 1;
        s.generation
    };
    save_later(generation);
}

/// Pages another browser knew, merged in: more visits and the later visit
/// win; the best kept when the list overflows.
pub fn import(visits: Vec<Visit>) {
    let generation = {
        let mut s = STORE.lock().unwrap_or_else(|e| e.into_inner());
        for v in visits.into_iter().filter(|v| worth_keeping(&v.url)) {
            match s.entries.iter_mut().find(|e| e.url == v.url) {
                Some(e) => {
                    e.visits = e.visits.max(v.visits);
                    e.last = e.last.max(v.last);
                    if e.title.is_empty() {
                        e.title = v.title;
                    }
                }
                None => s.entries.push(v),
            }
        }
        let t = now();
        s.entries.sort_by(|a, b| score(b, t).total_cmp(&score(a, t)));
        s.entries.truncate(MAX_ENTRIES);
        s.generation += 1;
        s.generation
    };
    save_later(generation);
}

fn save_later(generation: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        let s = STORE.lock().unwrap_or_else(|e| e.into_inner());
        if s.generation != generation {
            return;
        }
        let Ok(json) = serde_json::to_vec(&s.entries) else {
            return;
        };
        let path = path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    });
}

/// Frecency: visits, discounted by a week's half-life since the last one.
fn score(v: &Visit, now: i64) -> f64 {
    let days = ((now - v.last).max(0) as f64) / 86_400_000.;
    v.visits as f64 * 0.5f64.powf(days / 7.)
}

/// Pages matching every word of `query` in their address or title, best
/// first. An address that starts with the query ranks above one that only
/// contains it.
pub fn search(query: &str, limit: usize) -> Vec<Visit> {
    let s = STORE.lock().unwrap_or_else(|e| e.into_inner());
    rank(&s.entries, query, limit, now())
}

fn rank(entries: &[Visit], query: &str, limit: usize, now: i64) -> Vec<Visit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<(f64, &Visit)> = entries
        .iter()
        .filter_map(|v| {
            let addr = strip(&v.url).to_lowercase();
            let title = v.title.to_lowercase();
            if !words.iter().all(|w| addr.contains(w) || title.contains(w)) {
                return None;
            }
            let boost = if addr.starts_with(&q) {
                100.
            } else if host(&v.url).to_lowercase().contains(&q) {
                10.
            } else {
                1.
            };
            Some((boost * (1. + score(v, now)), v))
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    hits.into_iter()
        .take(limit)
        .map(|(_, v)| v.clone())
        .collect()
}

/// What to complete `typed` to as you type, Safari-style: the host of the
/// best page whose address starts with it, or the whole address once the
/// typing reaches past the host. Returns the completed text and the URL it
/// opens.
pub fn complete(typed: &str) -> Option<(String, String)> {
    let s = STORE.lock().unwrap_or_else(|e| e.into_inner());
    completion(&s.entries, typed, now())
}

fn completion(entries: &[Visit], typed: &str, now: i64) -> Option<(String, String)> {
    let t = typed.to_lowercase();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    let past_host = t.contains('/');
    let best = entries
        .iter()
        .filter(|v| strip(&v.url).to_lowercase().starts_with(&t))
        .max_by(|a, b| score(a, now).total_cmp(&score(b, now)))?;
    let scheme = best.url.split_once("://").map_or("https", |(s, _)| s);
    let text = if past_host {
        strip(&best.url).trim_end_matches('/').to_string()
    } else {
        host(&best.url).to_string()
    };
    if text.len() <= typed.len() {
        return None;
    }
    // Keep the case the user typed; complete only the rest.
    let shown = format!("{typed}{}", text.get(typed.len()..)?);
    let url = if past_host {
        best.url.clone()
    } else {
        format!("{scheme}://{}", host(&best.url))
    };
    Some((shown, url))
}

/// The sites you go back to most, one page per host, for the new tab page.
pub fn top_sites(limit: usize) -> Vec<Visit> {
    let s = STORE.lock().unwrap_or_else(|e| e.into_inner());
    top(&s.entries, limit, now())
}

fn top(entries: &[Visit], limit: usize, now: i64) -> Vec<Visit> {
    let mut by_host: Vec<(String, f64, Visit)> = Vec::new();
    for v in entries {
        let h = host(&v.url).to_string();
        let sc = score(v, now);
        match by_host.iter_mut().find(|(k, _, _)| *k == h) {
            Some((_, total, best)) => {
                *total += sc;
                if sc > score(best, now) {
                    *best = v.clone();
                }
            }
            None => by_host.push((h, sc, v.clone())),
        }
    }
    by_host.sort_by(|a, b| b.1.total_cmp(&a.1));
    by_host.into_iter().take(limit).map(|(_, _, v)| v).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(url: &str, title: &str, visits: u32, last: i64) -> Visit {
        Visit {
            url: url.into(),
            title: title.into(),
            visits,
            last,
        }
    }

    const NOW: i64 = 1_000 * 86_400_000;

    #[test]
    fn search_pages_are_not_kept() {
        assert!(!worth_keeping("https://www.google.com/search?q=rust"));
        assert!(!worth_keeping("about:blank"));
        assert!(worth_keeping("https://github.com/"));
    }

    #[test]
    fn completes_host_then_full_address() {
        let e = vec![
            v("https://github.com/broisnischal/deck", "deck", 5, NOW),
            v("https://www.gitlab.com/", "GitLab", 1, NOW),
        ];
        assert_eq!(
            completion(&e, "gi", NOW),
            Some(("github.com".into(), "https://github.com".into()))
        );
        assert_eq!(
            completion(&e, "Git", NOW),
            Some(("Github.com".into(), "https://github.com".into()))
        );
        assert_eq!(
            completion(&e, "github.com/b", NOW),
            Some((
                "github.com/broisnischal/deck".into(),
                "https://github.com/broisnischal/deck".into()
            ))
        );
        assert_eq!(completion(&e, "github.com", NOW), None);
        assert_eq!(completion(&e, "git hub", NOW), None);
    }

    #[test]
    fn prefix_matches_rank_first_and_old_visits_fade() {
        let e = vec![
            v(
                "https://docs.rs/gpui",
                "gpui docs",
                50,
                NOW - 60 * 86_400_000,
            ),
            v("https://gpui.rs/", "GPUI", 2, NOW),
            v("https://example.com/", "Example", 9, NOW),
        ];
        let hits = rank(&e, "gpui", 5, NOW);
        assert_eq!(hits[0].url, "https://gpui.rs/");
        assert_eq!(hits.len(), 2);
        assert!(rank(&e, "gpui docs", 5, NOW).len() == 1);
    }

    #[test]
    fn top_sites_one_per_host() {
        let e = vec![
            v("https://github.com/a", "a", 3, NOW),
            v("https://github.com/b", "b", 4, NOW),
            v("https://news.ycombinator.com/", "HN", 2, NOW),
        ];
        let t = top(&e, 8, NOW);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].url, "https://github.com/b");
    }
}
