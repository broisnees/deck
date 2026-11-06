//! Your Deck account. A build made with a backend (`DECK_API`) asks you to
//! sign in, through your browser since Google refuses sign-in inside an
//! embedded web view, and runs while your plan is active. The backend sends
//! the browser back to `deck://auth/callback` with a one-time code, which
//! deck trades for a session token kept by the system (Keychain, Credential
//! Manager, Secret Service). A build without a backend (one you made
//! yourself) asks for nothing.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the Keychain keeps the session token.
const SERVICE: &str = "com.broisnischal.deck.session";

/// How long deck keeps running on the last answer when the backend cannot
/// be reached (a plane, a train).
const OFFLINE_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Account {
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    /// The plan's name ("Pro"), when there is one.
    #[serde(default)]
    pub plan: Option<String>,
    /// The plan is paid up, or still in its trial.
    #[serde(default)]
    pub active: bool,
    /// When the free trial ends (unix seconds), while there is no paid plan.
    #[serde(default)]
    pub trial_ends: Option<u64>,
    /// When the backend last said all this (unix seconds).
    #[serde(default)]
    pub checked: u64,
}

impl Account {
    /// What the avatar shows: the first letter of the name, else the email.
    pub fn initial(&self) -> String {
        self.name
            .chars()
            .chain(self.email.chars())
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
    }

    /// Days of the free trial left, counting today, while on it without a
    /// paid plan; `Some(0)` once it has run out.
    pub fn trial_days_left(&self) -> Option<u64> {
        self.trial_days_left_at(now())
    }

    fn trial_days_left_at(&self, now: u64) -> Option<u64> {
        let ends = self.trial_ends?;
        Some(ends.saturating_sub(now).div_ceil(24 * 3600))
    }

    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

/// The backend this build signs in to, if any: `DECK_API` when deck runs,
/// else the one it was built with.
pub fn api_base() -> Option<String> {
    std::env::var("DECK_API")
        .ok()
        .or_else(|| option_env!("DECK_API").map(str::to_string))
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
}

/// This build asks for an account.
pub fn required() -> bool {
    api_base().is_some()
}

static CURRENT: Mutex<Option<Account>> = Mutex::new(None);
static PENDING: Mutex<Option<String>> = Mutex::new(None);

fn cache_path() -> std::path::PathBuf {
    crate::control::socket_dir().join("account.json")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The signed-in account as last known (read from disk on first use).
pub fn current() -> Option<Account> {
    static LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    // The file exists only while signed in (signing out deletes it).
    if !LOADED.swap(true, std::sync::atomic::Ordering::Relaxed) && required() {
        *cur = std::fs::read(cache_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
    }
    cur.clone()
}

fn remember(a: Option<Account>) {
    match &a {
        Some(a) => {
            if let Ok(b) = serde_json::to_vec_pretty(a) {
                let _ = std::fs::write(cache_path(), b);
            }
        }
        None => {
            let _ = std::fs::remove_file(cache_path());
        }
    }
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = a;
}

/// Whether deck may run: always without a backend; with one, when signed in
/// with an active plan, checked within the offline grace. A trial ends on its
/// day even offline.
pub fn allowed() -> bool {
    if !required() {
        return true;
    }
    current().is_some_and(|a| {
        a.active
            && now().saturating_sub(a.checked) < OFFLINE_GRACE.as_secs()
            && a.trial_days_left() != Some(0)
    })
}

// -- the session token, kept by the system (see secret.rs) ----------------------------

fn token() -> Option<String> {
    crate::secret::get(SERVICE)
}

fn store_token(t: &str) -> Result<(), String> {
    crate::secret::set(SERVICE, t).map_err(|e| format!("could not keep the session: {e}"))
}

fn clear_token() {
    crate::secret::delete(SERVICE);
}

// -- signing in -----------------------------------------------------------------------

/// The page that signs you in, in your browser. Remembers a fresh state
/// that the callback must carry back.
pub fn sign_in_url() -> Option<String> {
    let api = api_base()?;
    let state = random_hex(16);
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
    Some(format!("{api}/desktop/sign-in?state={state}"))
}

/// Where you choose and pay for a plan, and where you manage it.
pub fn checkout_url() -> Option<String> {
    api_base().map(|a| format!("{a}/desktop/checkout"))
}

pub fn billing_url() -> Option<String> {
    api_base().map(|a| format!("{a}/desktop/billing"))
}

fn random_hex(n: usize) -> String {
    // OS CSPRNG on every platform; one UUID covers up to 16 bytes.
    let hex = uuid::Uuid::new_v4().simple().to_string();
    hex[..(2 * n).min(hex.len())].to_string()
}

/// `deck://auth/callback?code=…&state=…`: the one-time code, when the state
/// is the one this deck sent out.
pub fn callback(url: &str) -> Option<Result<String, String>> {
    let rest = url.strip_prefix("deck://auth/callback")?;
    let query = rest.trim_start_matches('?');
    let param = |k: &str| {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
    };
    if let Some(e) = param("error") {
        return Some(Err(e.replace('+', " ")));
    }
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (param("code"), param("state")) {
        (Some(code), Some(state)) if pending.as_deref() == Some(state.as_str()) => Some(Ok(code)),
        (Some(_), _) => Some(Err("that sign-in was not started here; try again".into())),
        _ => Some(Err("the sign-in came back without a code".into())),
    }
}

/// Trade the one-time code for a session, then load the account.
pub async fn exchange(code: &str) -> Result<Account, String> {
    let api = api_base().ok_or("this build has no account server")?;
    let body =
        serde_json::to_vec(&serde_json::json!({ "code": code })).map_err(|e| e.to_string())?;
    let (status, reply) = crate::net::request(
        "POST",
        &format!("{api}/desktop/token"),
        &[("Content-Type", "application/json")],
        Some(body),
        20.,
    )
    .await
    .ok_or("the account server did not answer")?;
    if status != 200 {
        return Err(format!("sign-in failed ({status})"));
    }
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    let t: Token =
        serde_json::from_slice(&reply).map_err(|_| "the sign-in answer did not parse")?;
    store_token(&t.token)?;
    refresh().await
}

/// Ask the backend who is signed in and what their plan is.
pub async fn refresh() -> Result<Account, String> {
    let api = api_base().ok_or("this build has no account server")?;
    let token = token().ok_or("not signed in")?;
    let auth = format!("Bearer {token}");
    let (status, reply) = crate::net::request(
        "GET",
        &format!("{api}/v1/me"),
        &[("Authorization", &auth), ("Accept", "application/json")],
        None,
        20.,
    )
    .await
    .ok_or("the account server did not answer")?;
    if status == 401 {
        // The session ended (signed out elsewhere, or expired).
        clear_token();
        remember(None);
        return Err("your session ended; sign in again".into());
    }
    if status != 200 {
        return Err(format!("the account server answered {status}"));
    }
    let mut a: Account =
        serde_json::from_slice(&reply).map_err(|_| "the account answer did not parse")?;
    a.checked = now();
    remember(Some(a.clone()));
    Ok(a)
}

pub async fn sign_out() {
    if let (Some(api), Some(token)) = (api_base(), token()) {
        let auth = format!("Bearer {token}");
        let _ = crate::net::request(
            "POST",
            &format!("{api}/api/auth/sign-out"),
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
            ],
            Some(b"{}".to_vec()),
            10.,
        )
        .await;
    }
    clear_token();
    remember(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_must_carry_the_state_sent_out() {
        *PENDING.lock().unwrap() = Some("abc".into());
        assert_eq!(
            callback("deck://auth/callback?code=c1&state=abc"),
            Some(Ok("c1".into()))
        );
        // Used once: the same callback again is refused.
        assert!(matches!(
            callback("deck://auth/callback?code=c1&state=abc"),
            Some(Err(_))
        ));
        assert!(matches!(
            callback("deck://auth/callback?error=access+denied"),
            Some(Err(e)) if e == "access denied"
        ));
        assert_eq!(callback("ssh://x"), None);
    }

    #[test]
    fn the_backends_me_reply_parses() {
        // As `backend/src/index.ts` answers /v1/me (checked against it).
        let me = br#"{"email":"nees@example.com","name":"Nees","image":null,"plan":"Pro","active":true,"status":"active","renews":"2026-11-14T12:52:51.833Z","trial_ends":null}"#;
        let a: Account = serde_json::from_slice(me).unwrap();
        assert!(a.active);
        assert_eq!(a.plan.as_deref(), Some("Pro"));
        assert_eq!(a.trial_days_left(), None);
        assert_eq!(a.initial(), "N");

        let trial = br#"{"email":"n@x.com","name":"","image":null,"plan":"Trial","active":true,"status":"trialing","renews":null,"trial_ends":1800000000}"#;
        let t: Account = serde_json::from_slice(trial).unwrap();
        assert_eq!(t.trial_ends, Some(1_800_000_000));
    }

    #[test]
    fn trial_days_count_today_and_stop_at_zero() {
        let day = 24 * 3600;
        let a = Account {
            trial_ends: Some(100 * day),
            ..Default::default()
        };
        assert_eq!(a.trial_days_left_at(86 * day), Some(14));
        assert_eq!(a.trial_days_left_at(100 * day - 60), Some(1));
        assert_eq!(a.trial_days_left_at(100 * day), Some(0));
        assert_eq!(a.trial_days_left_at(120 * day), Some(0));
    }

    #[test]
    fn initials_and_names() {
        let a = Account {
            email: "nees@example.com".into(),
            ..Default::default()
        };
        assert_eq!(a.initial(), "N");
        assert_eq!(a.display_name(), "nees@example.com");
        assert_eq!(random_hex(16).len(), 32);
    }
}
