//! The native bridge (ADR-0038 §6, "Never in the webview"): this process
//! carries the webview's `/v1/ws` sessions over the server's unix socket
//! `flock-ws.sock`, so the app-session bearer it mints never leaves it. The
//! init script's `openBridge` (`config.rs`) drives three commands, open, send
//! and close; each session's frames and close come back on its own Tauri
//! channel, the [`Sink`]. A session belongs to the webview that opened it and
//! ends when that page goes away.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use futures_util::{SinkExt, StreamExt};
use serde::{de, Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};
use tokio_tungstenite::{
    tungstenite::{self, protocol::CloseFrame, Message},
    WebSocketStream,
};

use crate::mint::{Minted, Token, DEADLINE, NO_ANSWER, REMOTE};

/// One blocking mint over `flock.sock` (`mint::mint`), answered by the instant given.
pub type Mint = Arc<dyn Fn(std::time::Instant) -> Result<Minted, String> + Send + Sync>;
/// Milliseconds on a clock that counts suspended time, as the server's TTL does.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// One session's channel to the webview; `false` when it could not deliver,
/// because the page that opened the session is gone.
pub type Sink = Arc<dyn Fn(Event) -> bool + Send + Sync>;

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

/// A mint's outcome, the same for every open waiting on it.
type Flight = Option<Result<Arc<Bearer>, String>>;

/// The one current bearer ("When the native side mints").
#[derive(Default)]
struct Current {
    bearer: Option<Arc<Bearer>>,
    /// The last bearer ended with a 4401 or 4403 the webview saw, so the
    /// client caused the next mint and it is not `fresh`.
    ended_in_view: bool,
    /// The one mint in flight: every open that needs a bearer meanwhile waits on it.
    minting: Option<watch::Sender<Flight>>,
}

/// The webview's side of a session, queued for its task.
enum Out {
    Frame(String),
    Close(Option<CloseFrame>),
    /// The first frame was not a token-free `hello`: 1008, not forwarded.
    Refuse(&'static str),
    /// The page that opened the session is gone: 1001, and nobody to tell.
    Gone,
}

struct Session {
    /// The label of the webview that opened it, and that webview's page.
    owner: String,
    page: u64,
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
    ws: PathBuf,
    mint: Mint,
    now: Clock,
    current: Mutex<Current>,
    table: Mutex<Table>,
    next: AtomicU64,
}

#[derive(Default)]
struct Table {
    sessions: HashMap<u64, Session>,
    /// Each webview's page, counted up each time its page goes away.
    pages: HashMap<String, u64>,
}

impl Table {
    fn page(&self, owner: &str) -> u64 {
        self.pages.get(owner).copied().unwrap_or_default()
    }
}

impl fmt::Debug for Bridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bridge")
            .field("ws", &self.0.ws)
            .field("sessions", &self.table().sessions.len())
            .finish_non_exhaustive()
    }
}

const ENDED: &str = "this bridged session has ended";
const MINT_FAILED: &str = "The mint task failed.";
const GONE: &str = "the page that opened this session went away";

/// The one place a rejection's message is made: why, then the remote path
/// (ADR-0038 §6, "A rejection is a refusal, not an outage").
fn rejection(why: &str) -> String {
    format!("{why} {REMOTE}")
}

impl Bridge {
    /// `ws` is `flock-ws.sock` (`mint::ws_socket_path`).
    pub fn new(ws: PathBuf, mint: Mint, now: Clock) -> Self {
        Self(Arc::new(Inner {
            ws,
            mint,
            now,
            current: Default::default(),
            table: Default::default(),
            next: AtomicU64::new(1),
        }))
    }

    /// One session for the webview `owner` (the contract's `openBridge`).
    /// It rejects only for a refusal, saying why and offering the remote
    /// path; an unreachable or silent socket resolves and `sink` gets 1006.
    /// The mint, its retry and the upgrade share one [`DEADLINE`].
    pub async fn open(&self, owner: &str, renew: bool, sink: Sink) -> Result<Opened, String> {
        let deadline = Instant::now() + DEADLINE;
        let opened = self.start(owner, renew, sink, deadline).await;
        opened.map_err(|why| rejection(&why))
    }

    async fn start(
        &self,
        owner: &str,
        renew: bool,
        sink: Sink,
        deadline: Instant,
    ) -> Result<Opened, String> {
        let page = self.table().page(owner);
        let (bearer, fresh) = self.bearer(renew, deadline).await?;
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let opened = Opened {
            id,
            expires_in_ms: bearer.remaining((self.0.now)()),
            fresh,
        };
        let scrubbed = |text: String| scrub(text, &bearer.token);
        let mut retried = false;
        let ws = loop {
            let attempt = tokio::time::timeout_at(deadline, upgrade(&self.0.ws)).await;
            match attempt.unwrap_or(Upgrade::Unreachable) {
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
                            "The flk server could not bind this app's connection to its \
                             process, twice (peer_process_changed: {message})."
                        )))
                    }
                    (503, Some("preauth_capacity")) => {
                        return Ok(closed(opened, &sink, 1013, "preauth_capacity"))
                    }
                    (status, code) => {
                        return Err(scrubbed(format!(
                            "The flk server refused this app's connection with HTTP {status} \
                             ({}), which the app does not know: a defect to report.",
                            code.unwrap_or("no error code")
                        )))
                    }
                },
            }
        };
        let (out, rx) = mpsc::unbounded_channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        let session = Session {
            owner: owner.to_owned(),
            page,
            bearer: bearer.clone(),
            hello_sent: false,
            out,
            abandoned: abandoned.clone(),
        };
        let mut table = self.table();
        if table.page(owner) != page {
            // The page that asked went away meanwhile; a new page shares its
            // label, so the session never joins the table and nobody hears of it.
            drop(table);
            tokio::spawn(self.clone().gone(id, ws));
            return Ok(opened);
        }
        table.sessions.insert(id, session);
        drop(table);
        tokio::spawn(self.clone().run(id, ws, rx, sink, bearer, abandoned));
        Ok(opened)
    }

    /// A frame from the webview. The first must be a `hello` without a
    /// token; it gets the bearer. Every later one passes unchanged.
    pub fn send(&self, id: u64, text: String) -> Result<(), String> {
        let mut table = self.table();
        let session = table.sessions.get_mut(&id).ok_or(ENDED)?;
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
        let table = self.table();
        let session = table.sessions.get(&id).ok_or(ENDED)?;
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

    /// The webview `owner` reloaded, navigated or closed: its page is over,
    /// so the sessions of its older pages end with 1001 at the server and
    /// leave the table now, and an open still in flight never joins it.
    pub fn webview_gone(&self, owner: &str) {
        let mut table = self.table();
        let page = table.pages.entry(owner.to_owned()).or_default();
        *page += 1;
        let page = *page;
        let ids: Vec<u64> = table
            .sessions
            .iter()
            .filter(|(_, session)| session.owner == owner && session.page < page)
            .map(|(id, _)| *id)
            .collect();
        for session in ids.iter().filter_map(|id| table.sessions.remove(id)) {
            session.abandoned.store(true, Ordering::Relaxed);
            drop(session.out.send(Out::Gone));
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        let table = self.0.table.lock();
        table.unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn current(&self) -> MutexGuard<'_, Current> {
        let current = self.0.current.lock();
        current.unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current bearer, or a new one when there is none, it has run out,
    /// or the open renews. `true` when the client did not cause the mint.
    /// At most one mint is in flight: an open that needs a bearer meanwhile,
    /// renewing or not, waits for that mint's bearer or its refusal.
    async fn bearer(&self, renew: bool, deadline: Instant) -> Result<(Arc<Bearer>, bool), String> {
        let now = (self.0.now)();
        let (mut flight, fresh) = {
            let mut current = self.current();
            let kept = current
                .bearer
                .as_ref()
                .filter(|b| b.remaining(now) != Some(0));
            if let (false, Some(bearer)) = (renew, kept) {
                return Ok((bearer.clone(), false));
            }
            let fresh = !renew && !current.ended_in_view;
            let minting = current
                .minting
                .get_or_insert_with(|| self.fly(deadline, now));
            (minting.subscribe(), fresh)
        };
        let outcome = flight.wait_for(Option::is_some).await;
        let outcome = outcome.map_err(|_| MINT_FAILED.to_owned())?.clone();
        let bearer = outcome.unwrap_or_else(|| Err(MINT_FAILED.into()))?;
        Ok((bearer, fresh))
    }

    /// Starts the one mint, which must answer by `deadline`. No lock is held
    /// while it runs; its outcome becomes the current bearer, or the refusal
    /// every waiter rejects with. A late answer is dropped unused.
    fn fly(&self, deadline: Instant, minted_at: u64) -> watch::Sender<Flight> {
        let (done, _) = watch::channel(None);
        let mint = self.0.mint.clone();
        let until = std::time::Instant::now() + deadline.saturating_duration_since(Instant::now());
        let minting = tokio::task::spawn_blocking(move || mint(until));
        let (bridge, publish) = (self.clone(), done.clone());
        tokio::spawn(async move {
            let minted = match tokio::time::timeout_at(deadline, minting).await {
                Ok(Ok(minted)) => minted,
                Ok(Err(_)) => Err(MINT_FAILED.into()),
                Err(_) => Err(NO_ANSWER.into()),
            };
            let mut current = bridge.current();
            current.minting = None;
            let outcome = match minted {
                Ok(minted) => {
                    let bearer = Arc::new(Bearer {
                        token: minted.token,
                        expires_in_ms: minted.expires_in_ms,
                        minted_at,
                    });
                    current.bearer = Some(bearer.clone());
                    current.ended_in_view = false;
                    Ok(bearer)
                }
                Err(message) => Err(match &current.bearer {
                    Some(bearer) => scrub(message, &bearer.token),
                    None => message,
                }),
            };
            publish.send_replace(Some(outcome));
        });
        done
    }

    /// The server refused or ended `bearer` (4401, 4403): never offer it
    /// again. An end on an older bearer leaves the current one.
    fn ended(&self, bearer: &Arc<Bearer>, seen: bool) {
        let mut current = self.current();
        if current
            .bearer
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, bearer))
        {
            current.bearer = None;
            current.ended_in_view = seen;
        }
    }

    /// One session's task: frames both ways until either side closes, or
    /// the page that opened it goes away.
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
            // What the server has sent goes first: a 4401 or 4403 it sent
            // before the page went away still ends the bearer.
            tokio::select! {
                biased;
                frame = ws.next() => match frame {
                    Some(Ok(Message::Text(text))) => {
                        let data = scrub(text.as_str().to_owned(), &bearer.token);
                        if !sink(Event::Message { data }) {
                            return self.gone(id, ws).await;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame.map_or((1005, String::new()), |f| {
                            (f.code.into(), f.reason.as_str().to_owned())
                        });
                        if matches!(code, 4401 | 4403) {
                            self.ended(&bearer, !abandoned.load(Ordering::Relaxed));
                        }
                        break (code, reason);
                    }
                    // `/v1/ws` carries JSON text frames only (§6), so a binary
                    // frame is RFC 6455's 1003, unsupported data, never text.
                    Some(Ok(Message::Binary(_))) => {
                        let reason = "the server sent a binary frame";
                        let frame = CloseFrame { code: 1003.into(), reason: reason.into() };
                        drop(ws.close(Some(frame)).await);
                        break (1003, reason.to_owned());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break (1006, String::new()),
                },
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
                    Some(Out::Gone) | None => return self.gone(id, ws).await,
                },
            }
        };
        self.table().sessions.remove(&id);
        let reason = scrub(reason, &bearer.token);
        sink(Event::Close { code, reason });
    }

    /// The session's page is gone: out of the table, and a 1001 to the
    /// server, whose close reply ends it, awaited for at most the deadline.
    async fn gone(self, id: u64, mut ws: WebSocketStream<Stream>) {
        self.table().sessions.remove(&id);
        let frame = CloseFrame {
            code: 1001.into(),
            reason: GONE.into(),
        };
        if ws.close(Some(frame)).await.is_ok() {
            let reply = async {
                while let Some(Ok(frame)) = ws.next().await {
                    if frame.is_close() {
                        break;
                    }
                }
            };
            drop(tokio::time::timeout(DEADLINE, reply).await);
        }
    }

    /// The sessions in the table, for tests.
    #[cfg(test)]
    fn live(&self) -> usize {
        self.table().sessions.len()
    }

    /// The opens waiting on the mint in flight, for tests.
    #[cfg(test)]
    fn joined(&self) -> usize {
        let current = self.current();
        current.minting.as_ref().map_or(0, |m| m.receiver_count())
    }
}

/// 403 `operator_only`: not the local operator, so the pane trap (`README.md`).
fn operator_only() -> String {
    "The flk server refused this app's connection: it is not running as the local operator \
     (it was started from inside a flk pane, or not from your own session). Open it from \
     the Dock, Finder or your launcher."
        .into()
}

/// A session that ends before it starts: 1006 for an unreachable or silent
/// socket, 1013 for the server's `preauth_capacity`. The bridge makes the close.
fn closed(opened: Opened, sink: &Sink, code: u16, reason: &str) -> Opened {
    sink(Event::Close {
        code,
        reason: reason.into(),
    });
    opened
}

/// A JSON object with unique top-level keys. serde_json keeps the last of a
/// duplicate, so `{"t":"cmd","t":"hello"}` would otherwise read as a `hello`.
struct Strict(serde_json::Map<String, serde_json::Value>);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Keys;
        impl<'de> de::Visitor<'de> for Keys {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
                let mut object = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if object.insert(key, value).is_some() {
                        return Err(de::Error::custom("a duplicate key"));
                    }
                }
                Ok(Strict(object))
            }
        }
        deserializer.deserialize_map(Keys)
    }
}

/// The `hello` with the bearer added, or why the frame is not forwarded.
fn hello(text: &str, token: &Token) -> Result<String, &'static str> {
    let Ok(Strict(mut hello)) = serde_json::from_str(text) else {
        return Err("the first frame is not a JSON object with unique keys, so not a hello");
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
/// Any other failure, before or during the handshake, is an outage.
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

/// No unix socket here: the mint has already refused.
#[cfg(not(unix))]
async fn upgrade(_: &Path) -> Upgrade {
    Upgrade::Refused {
        status: 0,
        code: None,
        message: String::new(),
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
