//! The control plane: newline-delimited JSON over a Unix socket.
//!
//! Every terminal pane starts with `DECK_SOCKET`, `DECK_TOKEN` and `DECK_PANE`
//! in its environment. Tokens are per pane and die with it, so every caller
//! is a known pane in a known window: that is what lets `pane.split` default
//! to "beside me", and what stops one window from driving another. The socket
//! itself is 0600 inside a 0700 directory, so only this user can connect.

use futures::channel::{mpsc, oneshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use crate::os::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub token: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Response {
    Ok(Value),
    Error(String),
}

/// The answer slot for one request. Dropping it unsent tells the caller the
/// request was abandoned rather than leaving it hanging.
pub struct Reply(oneshot::Sender<Response>);

impl Reply {
    pub fn send(self, r: Result<Value, String>) {
        let _ = self.0.send(match r {
            Ok(v) => Response::Ok(v),
            Err(e) => Response::Error(e),
        });
    }
}

pub type Incoming = (Request, Reply);

/// Removes the socket file when the window that owns it goes away.
pub struct Server {
    pub path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn newest_socket() -> Option<PathBuf> {
    std::fs::read_dir(socket_dir())
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .filter(|e| UnixStream::connect(e.path()).is_ok())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Where deck keeps its socket, settings and threads. `DECK_DATA_DIR` moves
/// it, so a test instance never touches the user's own.
pub fn socket_dir() -> PathBuf {
    agent_core::dirs::data_dir()
}

pub fn stable_socket() -> PathBuf {
    socket_dir().join("deck.sock")
}

pub fn listen() -> std::io::Result<(Server, mpsc::UnboundedReceiver<Incoming>)> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir)?;
    crate::os::private(&dir)?;

    // A crashed instance leaves its socket behind; nothing answers on it.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sock") && UnixStream::connect(&p).is_err() {
                let _ = std::fs::remove_file(&p);
            }
        }
    }

    // Per process, so two instances never fight over one path.
    let path = dir.join(format!("deck-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    crate::os::private(&path)?;
    // A name that outlives this process: shells kept by the background
    // service across relaunches find the Deck of the moment through it.
    let stable = stable_socket();
    let _ = std::fs::remove_file(&stable);
    let _ = crate::os::symlink(&path, &stable);

    let (tx, rx) = mpsc::unbounded();
    std::thread::Builder::new()
        .name("deck-control".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                // phobos: a thread per connection — fine for a local CLI; move
                // to an async accept loop if something holds hundreds open.
                std::thread::spawn(move || serve(stream, tx));
            }
        })?;
    Ok((Server { path }, rx))
}

fn serve(stream: UnixStream, tx: mpsc::UnboundedSender<Incoming>) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let (otx, orx) = oneshot::channel();
                if tx.unbounded_send((req, Reply(otx))).is_err() {
                    return;
                }
                futures::executor::block_on(orx).unwrap_or_else(|_| {
                    Response::Error("no answer: the pane went away or the call failed".into())
                })
            }
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let mut buf = serde_json::to_vec(&resp).unwrap_or_default();
        buf.push(b'\n');
        if out.write_all(&buf).is_err() {
            return;
        }
    }
}

/// Client side: one request, one answer.
///
/// Inside a deck pane the socket and token come from the environment. From
/// anywhere else we find the newest running window and call without a token,
/// which that window only accepts for `browser.open` and `app.open_url`.
pub fn call(method: &str, params: Value) -> Result<Value, String> {
    let (socket, token) = match std::env::var("DECK_SOCKET") {
        Ok(s) => (s, std::env::var("DECK_TOKEN").unwrap_or_default()),
        Err(_) => (
            newest_socket()
                .ok_or("no running deck window found")?
                .display()
                .to_string(),
            String::new(),
        ),
    };

    let mut s =
        UnixStream::connect(&socket).map_err(|e| format!("cannot reach deck at {socket}: {e}"))?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));

    let req = Request {
        token,
        method: method.into(),
        params,
    };
    let mut buf = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    buf.push(b'\n');
    s.write_all(&buf).map_err(|e| e.to_string())?;

    let mut line = String::new();
    BufReader::new(s)
        .read_line(&mut line)
        .map_err(|e| format!("no answer from deck: {e}"))?;
    match serde_json::from_str(&line).map_err(|e| format!("bad answer from deck: {e}"))? {
        Response::Ok(v) => Ok(v),
        Response::Error(e) => Err(e),
    }
}
