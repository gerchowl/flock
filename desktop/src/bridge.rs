//! The native bridge (ADR-0038 §6, "Never in the webview"): this process
//! carries the webview's `/v1/ws` sessions over the server's unix socket
//! `flock-ws.sock`, so the app-session bearer it mints never leaves it. The
//! init script's `openBridge` (`config.rs`) drives three commands, open, send
//! and close; each session's frames and close come back on its own Tauri
//! channel, the [`Sink`].

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    tungstenite::{self, protocol::CloseFrame, Message},
    WebSocketStream,
};

use crate::mint::{Minted, Token, REMOTE};

/// One blocking mint over `flock.sock` (`mint::mint`).
pub type Mint = Arc<dyn Fn() -> Result<Minted, String> + Send + Sync>;
/// Milliseconds on a clock that counts suspended time, as the server's TTL does.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// One session's channel to the webview.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// What the webview's socket receives: a text frame, or the session's end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Event {
    Message { data: String },
    Close { code: u16, reason: String },
}

/// The open's result: the contract's `expiresInMs` and `fresh`, never the bearer.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fresh: bool,
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(not(unix))]
type Stream = tokio::net::TcpStream;

struct Bearer {
    token: Token,
    expires_in_ms: Option<u64>,
    minted_at: u64,
}

impl Bearer {
    /// What is left of the server's `expires_in_ms` at `now`.
    fn remaining(&self, now: u64) -> Option<u64> {
        let spent = now.saturating_sub(self.minted_at);
        self.expires_in_ms.map(|ms| ms.saturating_sub(spent))
    }
}

/// The one current bearer ("When the native side mints").
#[derive(Default)]
struct Current {
    bearer: Option<Arc<Bearer>>,
    /// The last bearer ended with a 4401 or 4403 the webview saw, so the
    /// client caused the next mint and it is not `fresh`.
    ended_in_view: bool,
}

/// The webview's side of a session, queued for its task.
enum Out {
    Frame(String),
    Close(Option<CloseFrame>),
    /// The first frame was not a token-free `hello`: 1008, not forwarded.
    Refuse(&'static str),
}

struct Session {
    bearer: Arc<Bearer>,
    hello_sent: bool,
    out: mpsc::UnboundedSender<Out>,
    /// The webview closed it, so an end the server sends now goes unseen.
    abandoned: Arc<AtomicBool>,
}

/// The bridge: the current bearer and the live sessions, shared by the commands.
#[derive(Clone)]
pub struct Bridge(Arc<Inner>);

struct Inner {
    ws: Result<PathBuf, String>,
    mint: Mint,
    now: Clock,
    current: tokio::sync::Mutex<Current>,
    sessions: Mutex<HashMap<u64, Session>>,
    next: AtomicU64,
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sessions = self.0.sessions.lock().map_or(0, |s| s.len());
        f.debug_struct("Bridge")
            .field("ws", &self.0.ws)
            .field("sessions", &sessions)
            .finish_non_exhaustive()
    }
}

const ENDED: &str = "this bridged session has ended";

impl Bridge {
    /// `ws` is `flock-ws.sock`, or why its path is unknown.
    pub fn new(ws: Result<PathBuf, String>, mint: Mint, now: Clock) -> Self {
        Self(Arc::new(Inner {
            ws,
            mint,
            now,
            current: Default::default(),
            sessions: Default::default(),
            next: AtomicU64::new(1),
        }))
    }

    /// One session (the contract's `openBridge`). It rejects only for a
    /// refusal; an unreachable socket resolves and `sink` gets 1006.
    pub async fn open(&self, renew: bool, sink: Sink) -> Result<Opened, String> {
        let (bearer, fresh) = self.bearer(renew).await?;
        let path = self.0.ws.clone()?;
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let opened = Opened {
            id,
            expires_in_ms: bearer.remaining((self.0.now)()),
            fresh,
        };
        let scrubbed = |text: String| scrub(text, &bearer.token);
        let mut retried = false;
        let ws = loop {
            match upgrade(&path).await {
                Upgrade::Open(ws) => break *ws,
                Upgrade::Unreachable => return Ok(closed(opened, &sink, 1006, "")),
                Upgrade::Refused {
                    status,
                    code,
                    message,
                } => match (status, code.as_deref()) {
                    (403, Some("operator_only")) => return Err(operator_only()),
                    (403, Some("peer_process_changed")) if !retried => retried = true,
                    (403, Some("peer_process_changed")) => {
                        return Err(scrubbed(format!(
                            "The flk server refused this app's connection twice \
                             (peer_process_changed: {message}). Restart the app."
                        )))
                    }
                    (503, Some("preauth_capacity")) => {
                        return Ok(closed(opened, &sink, 1013, "preauth_capacity"))
                    }
                    (status, code) => {
                        return Err(scrubbed(format!(
                            "The flk server refused this app's connection with HTTP {status} \
                             ({}). That is a defect: please report it.",
                            code.unwrap_or("no error code")
                        )))
                    }
                },
            }
        };
        let (out, rx) = mpsc::unbounded_channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        let session = Session {
            bearer: bearer.clone(),
            hello_sent: false,
            out,
            abandoned: abandoned.clone(),
        };
        self.sessions().insert(id, session);
        tokio::spawn(self.clone().run(id, ws, rx, sink, bearer, abandoned));
        Ok(opened)
    }

    /// A frame from the webview. The first must be a `hello` without a
    /// token; it gets the bearer. Every later one passes unchanged.
    pub fn send(&self, id: u64, text: String) -> Result<(), String> {
        let mut sessions = self.sessions();
        let session = sessions.get_mut(&id).ok_or(ENDED)?;
        let out = if session.hello_sent {
            Out::Frame(text)
        } else {
            session.hello_sent = true;
            hello(&text, &session.bearer.token).map_or_else(Out::Refuse, Out::Frame)
        };
        session.out.send(out).map_err(|_| ENDED.into())
    }

    /// The webview closes its socket; the server's close still comes back.
    pub fn close(&self, id: u64, code: Option<u16>, reason: Option<String>) -> Result<(), String> {
        let sessions = self.sessions();
        let session = sessions.get(&id).ok_or(ENDED)?;
        session.abandoned.store(true, Ordering::Relaxed);
        let frame = code.map(|code| CloseFrame {
            code: code.into(),
            reason: reason.unwrap_or_default().into(),
        });
        session
            .out
            .send(Out::Close(frame))
            .map_err(|_| ENDED.into())
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Session>> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current bearer, or a new one when there is none, it has run out,
    /// or the open renews. `true` when the client did not cause the mint.
    async fn bearer(&self, renew: bool) -> Result<(Arc<Bearer>, bool), String> {
        let mut current = self.0.current.lock().await;
        let now = (self.0.now)();
        let kept = current
            .bearer
            .as_ref()
            .filter(|b| b.remaining(now) != Some(0));
        if let (false, Some(bearer)) = (renew, kept) {
            return Ok((bearer.clone(), false));
        }
        let fresh = !renew && !current.ended_in_view;
        let mint = self.0.mint.clone();
        let minted = tokio::task::spawn_blocking(move || mint())
            .await
            .map_err(|error| format!("The mint task failed: {error}"))?;
        let minted = minted.map_err(|message| match &current.bearer {
            Some(bearer) => scrub(message, &bearer.token),
            None => message,
        })?;
        let bearer = Arc::new(Bearer {
            token: minted.token,
            expires_in_ms: minted.expires_in_ms,
            minted_at: now,
        });
        *current = Current {
            bearer: Some(bearer.clone()),
            ended_in_view: false,
        };
        Ok((bearer, fresh))
    }

    /// The server refused or ended `bearer` (4401, 4403): never offer it again.
    async fn ended(&self, bearer: &Arc<Bearer>, seen: bool) {
        let mut current = self.0.current.lock().await;
        if current
            .bearer
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, bearer))
        {
            *current = Current {
                bearer: None,
                ended_in_view: seen,
            };
        }
    }

    /// One session's task: frames both ways until either side closes.
    async fn run(
        self,
        id: u64,
        mut ws: WebSocketStream<Stream>,
        mut rx: mpsc::UnboundedReceiver<Out>,
        sink: Sink,
        bearer: Arc<Bearer>,
        abandoned: Arc<AtomicBool>,
    ) {
        let (code, reason) = loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(Out::Frame(text)) => {
                        if ws.send(Message::Text(text.into())).await.is_err() {
                            break (1006, String::new());
                        }
                    }
                    Some(Out::Close(frame)) => drop(ws.close(frame).await),
                    Some(Out::Refuse(reason)) => {
                        let frame = CloseFrame { code: 1008.into(), reason: reason.into() };
                        drop(ws.close(Some(frame)).await);
                        break (1008, reason.to_owned());
                    }
                    None => break (1006, String::new()),
                },
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(text))) => sink(Event::Message {
                        data: scrub(text.as_str().to_owned(), &bearer.token),
                    }),
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map_or((1005, String::new()), |f| {
                            (f.code.into(), f.reason.as_str().to_owned())
                        });
                        if matches!(code, 4401 | 4403) {
                            self.ended(&bearer, !abandoned.load(Ordering::Relaxed)).await;
                        }
                        break (code, reason);
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let reason = "the server sent a binary frame";
                        let frame = CloseFrame { code: 1003.into(), reason: reason.into() };
                        drop(ws.close(Some(frame)).await);
                        break (1003, reason.to_owned());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break (1006, String::new()),
                },
            }
        };
        self.sessions().remove(&id);
        let reason = scrub(reason, &bearer.token);
        sink(Event::Close { code, reason });
    }
}

/// 403 `operator_only`: not the local operator, so the pane trap (`README.md`).
fn operator_only() -> String {
    format!(
        "The flk server refused this app's connection: it is not running as the local \
         operator (it was started from inside a flk pane, or not from your own session). \
         Open it from the Dock, Finder or your launcher. {REMOTE}"
    )
}

/// A session that ends before it starts: 1006 for an unreachable socket,
/// 1013 for the server's `preauth_capacity`. The bridge makes the close.
fn closed(opened: Opened, sink: &Sink, code: u16, reason: &str) -> Opened {
    sink(Event::Close {
        code,
        reason: reason.into(),
    });
    opened
}

/// The `hello` with the bearer added, or why the frame is not forwarded.
fn hello(text: &str, token: &Token) -> Result<String, &'static str> {
    let Ok(serde_json::Value::Object(mut hello)) = serde_json::from_str(text) else {
        return Err("the first frame is not a JSON object, so not a hello");
    };
    if hello.get("t").and_then(|t| t.as_str()) != Some("hello") {
        return Err("the first frame is not a hello");
    }
    if hello.contains_key("token") {
        return Err("the first frame is a hello that carries a token");
    }
    hello.insert("token".into(), token.secret().into());
    Ok(serde_json::Value::Object(hello).to_string())
}

/// `text` with the bearer cut out: nothing the webview gets may carry it.
fn scrub(text: String, token: &Token) -> String {
    if text.contains(token.secret()) {
        text.replace(token.secret(), "<redacted>")
    } else {
        text
    }
}

enum Upgrade {
    Open(Box<WebSocketStream<Stream>>),
    Unreachable,
    Refused {
        status: u16,
        code: Option<String>,
        message: String,
    },
}

/// The HTTP/1.1 upgrade to `/v1/ws` on a fresh connection to `path`, with
/// the typed refusal the server sends instead ("Refusals before the upgrade").
#[cfg(unix)]
async fn upgrade(path: &Path) -> Upgrade {
    let Ok(stream) = Stream::connect(path).await else {
        return Upgrade::Unreachable;
    };
    match tokio_tungstenite::client_async("ws://localhost/v1/ws", stream).await {
        Ok((ws, _)) => Upgrade::Open(Box::new(ws)),
        Err(tungstenite::Error::Http(response)) => {
            let body = response.body().as_deref().unwrap_or_default();
            let error = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
            let text = |key| error["error"][key].as_str().map(str::to_owned);
            Upgrade::Refused {
                status: response.status().as_u16(),
                code: text("code"),
                message: text("message").unwrap_or_default(),
            }
        }
        Err(_) => Upgrade::Unreachable,
    }
}

/// No unix socket here: the mint has already refused, offering [`REMOTE`].
#[cfg(not(unix))]
async fn upgrade(_: &Path) -> Upgrade {
    Upgrade::Refused {
        status: 0,
        code: None,
        message: REMOTE.into(),
    }
}

/// [`Clock`] for the app: macOS's `CLOCK_MONOTONIC` and Linux's
/// `CLOCK_BOOTTIME` both count sleep, like the server's TTL (#2489).
#[cfg(unix)]
pub fn suspend_inclusive_ms() -> u64 {
    #[cfg(target_os = "linux")]
    const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
    #[cfg(not(target_os = "linux"))]
    const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(CLOCK, &mut now) };
    (now.tv_sec as u64) * 1000 + (now.tv_nsec as u64) / 1_000_000
}

#[cfg(not(unix))]
pub fn suspend_inclusive_ms() -> u64 {
    0
}

#[cfg(test)]
mod tests;
