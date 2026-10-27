//! The bridge against a fake `flock-ws.sock` (ADR-0038 §6's "the bridge: …"
//! and "the bridge's pre-upgrade mapping" tests). Never the live server.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize},
        mpsc::{Receiver, SyncSender},
    },
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc::{error::TryRecvError, UnboundedReceiver},
};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::*;
use crate::mint::parse_reply;

type Server = WebSocketStream<UnixStream>;

/// What the fake does with one connection instead of upgrading it.
enum Script {
    /// An HTTP refusal: the status and the JSON body.
    Refuse(u16, String),
    /// Accept, read nothing, answer nothing: a hung server.
    Hang,
    /// Answer the upgrade with something that is not HTTP.
    Garbage,
}

/// A fake `flock-ws.sock`: each connection takes the next [`Script`], or
/// else upgrades and is handed to the test.
struct Fake {
    dir: PathBuf,
    accepted: UnboundedReceiver<Server>,
    refusals: Arc<Mutex<VecDeque<Script>>>,
    connections: Arc<AtomicUsize>,
}

impl Fake {
    fn path(&self) -> PathBuf {
        self.dir.join("flock-ws.sock")
    }

    fn refuse(&self, status: u16, code: &str, message: &str) {
        let body = json!({ "error": { "code": code, "message": message } }).to_string();
        self.script(Script::Refuse(status, body));
    }

    fn script(&self, script: Script) {
        self.refusals.lock().unwrap().push_back(script);
    }

    async fn accept(&mut self) -> Server {
        settled(self.accepted.recv()).await.unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.dir));
    }
}

/// A bridge over a fake socket, a counting mint and a hand-moved clock.
struct Harness {
    bridge: Bridge,
    fake: Fake,
    mints: Arc<AtomicUsize>,
    refuse_mint: Arc<Mutex<Option<String>>>,
    hold_mint: MintGate,
    clock: Arc<AtomicU64>,
}

/// When set, the next mint signals the sender, then waits on the receiver.
type MintGate = Arc<Mutex<Option<(SyncSender<()>, Receiver<()>)>>>;

/// The webview that opens the harness's sessions.
const OWNER: &str = "main";

impl Harness {
    /// `listening: false` leaves the socket's path without a server.
    fn new(expires_in_ms: Option<u64>, listening: bool) -> Self {
        static DIRS: AtomicUsize = AtomicUsize::new(0);
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fdb-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, accepted) = mpsc::unbounded_channel();
        let fake = Fake {
            dir,
            accepted,
            refusals: Default::default(),
            connections: Default::default(),
        };
        if listening {
            let listener = UnixListener::bind(fake.path()).unwrap();
            let (refusals, connections) = (fake.refusals.clone(), fake.connections.clone());
            tokio::spawn(async move {
                let mut hung = Vec::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    connections.fetch_add(1, Ordering::Relaxed);
                    let script = refusals.lock().unwrap().pop_front();
                    match script {
                        Some(Script::Refuse(status, body)) => respond(stream, status, &body).await,
                        Some(Script::Hang) => hung.push(stream),
                        Some(Script::Garbage) => garbage(stream).await,
                        None => {
                            drop(tx.send(tokio_tungstenite::accept_async(stream).await.unwrap()))
                        }
                    }
                }
            });
        }
        let (mints, refuse_mint) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
        let hold_mint: MintGate = Default::default();
        let (count, refuse, hold) = (mints.clone(), refuse_mint.clone(), hold_mint.clone());
        let mint: Mint = Arc::new(move || {
            let held = hold.lock().unwrap().take();
            if let Some((entered, release)) = held {
                entered.send(()).unwrap();
                release.recv().unwrap();
            }
            if let Some(message) = refuse.lock().unwrap().clone() {
                return Err(message);
            }
            let n = count.fetch_add(1, Ordering::Relaxed) + 1;
            let info = expires_in_ms.map_or(json!({}), |ms| json!({ "expires_in_ms": ms }));
            let issued = json!({ "info": info, "token": format!("bearer-{n}") });
            let line = json!({ "result": { "type": "client_token_issued", "issued": issued } });
            Ok(parse_reply(&line.to_string()).unwrap())
        });
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let now = clock.clone();
        let bridge = Bridge::new(
            fake.path(),
            mint,
            Arc::new(move || now.load(Ordering::Relaxed)),
        );
        Self {
            bridge,
            fake,
            mints,
            refuse_mint,
            hold_mint,
            clock,
        }
    }

    async fn open(&self, renew: bool) -> (Result<Opened, String>, UnboundedReceiver<Event>) {
        let (tx, events) = mpsc::unbounded_channel();
        let sink: Sink = Arc::new(move |event| tx.send(event).is_ok());
        (self.bridge.open(OWNER, renew, sink).await, events)
    }

    /// An open session past its `hello`, and the bearer the server got in it.
    async fn session(&mut self, renew: bool) -> (Opened, UnboundedReceiver<Event>, Server, String) {
        let (opened, events) = self.open(renew).await;
        let opened = opened.unwrap();
        let mut server = self.fake.accept().await;
        self.bridge.send(opened.id, HELLO.into()).unwrap();
        let hello: Value = serde_json::from_str(&text(&mut server).await).unwrap();
        let token = hello["token"].as_str().unwrap().to_owned();
        (opened, events, server, token)
    }

    fn mints(&self) -> usize {
        self.mints.load(Ordering::Relaxed)
    }
}

const HELLO: &str = r#"{"t":"hello","protocol":1,"client":{"name":"flock-gui"}}"#;

/// A pre-upgrade refusal, after reading the request's head.
async fn respond(stream: UnixStream, status: u16, body: &str) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).await.unwrap() > 2 {
        line.clear();
    }
    let head = format!(
        "HTTP/1.1 {status} Refused\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    reader
        .into_inner()
        .write_all(head.as_bytes())
        .await
        .unwrap();
}

/// An answer to the upgrade that is not HTTP at all.
async fn garbage(stream: UnixStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).await.unwrap() > 2 {
        line.clear();
    }
    let mut stream = reader.into_inner();
    stream.write_all(b"not http at all\r\n\r\n").await.unwrap();
}

/// A wait that fails the test instead of hanging it when the bridge never acts.
async fn settled<T>(wait: impl std::future::Future<Output = T>) -> T {
    let guard = std::time::Duration::from_secs(10);
    tokio::time::timeout(guard, wait)
        .await
        .expect("the bridge never acted")
}

async fn text(server: &mut Server) -> String {
    match settled(server.next()).await {
        Some(Ok(Message::Text(text))) => text.as_str().to_owned(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// The server's side of a close the bridge began: its reply, then the end.
async fn finish_close(server: &mut Server) {
    while let Some(Ok(_)) = settled(server.next()).await {}
}

async fn close_from_server(server: &mut Server, code: u16, reason: &str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    };
    server.close(Some(frame)).await.unwrap();
}

async fn closed(events: &mut UnboundedReceiver<Event>) -> (u16, String) {
    match settled(events.recv()).await {
        Some(Event::Close { code, reason }) => (code, reason),
        other => panic!("expected a close, got {other:?}"),
    }
}

fn no_events(events: &mut UnboundedReceiver<Event>) {
    assert!(matches!(
        events.try_recv(),
        Err(TryRecvError::Empty | TryRecvError::Disconnected)
    ));
}

#[tokio::test]
async fn the_hello_gets_the_bearer_and_every_other_frame_passes_byte_for_byte() {
    let mut h = Harness::new(Some(43_200_000), true);
    let (opened, mut events) = h.open(false).await;
    let opened = opened.unwrap();
    let wire = serde_json::to_string(&opened).unwrap();
    assert_eq!(wire, r#"{"id":1,"expiresInMs":43200000,"fresh":true}"#);
    let mut server = h.fake.accept().await;
    h.bridge.send(opened.id, HELLO.into()).unwrap();
    let hello: Value = serde_json::from_str(&text(&mut server).await).unwrap();
    let expected = json!({ "t": "hello", "protocol": 1, "client": { "name": "flock-gui" }, "token": "bearer-1" });
    assert_eq!(hello, expected);
    // Whitespace, key order and escapes stay as they were, both ways.
    let up = "{ \"t\" : \"cmd\",\"id\":7,  \"text\":\"a\\u0022b\" }";
    h.bridge.send(opened.id, up.into()).unwrap();
    assert_eq!(text(&mut server).await, up);
    let down = " {\"t\":\"welcome\",\"principal\":\"app:operator:501\"}\n";
    server.send(Message::Text(down.into())).await.unwrap();
    assert_eq!(
        events.recv().await,
        Some(Event::Message { data: down.into() })
    );
    close_from_server(&mut server, 1000, "bye").await;
    assert_eq!(closed(&mut events).await, (1000, "bye".into()));
    assert!(!wire.contains("bearer"));
    assert_eq!(h.mints(), 1);
}

#[tokio::test]
async fn a_first_frame_that_is_not_a_token_free_hello_closes_1008_unforwarded() {
    let mut h = Harness::new(None, true);
    let bad = [
        r#"{"t":"cmd","id":1}"#,
        r#"{"t":"hello","protocol":1,"token":"mine"}"#,
        "not json",
        r#"["hello"]"#,
        // serde_json keeps the last duplicate key, so this would read as a hello.
        r#"{"t":"cmd","t":"hello","protocol":1}"#,
    ];
    for first in bad {
        let (opened, mut events) = h.open(false).await;
        let id = opened.unwrap().id;
        let mut server = h.fake.accept().await;
        h.bridge.send(id, first.into()).unwrap();
        let (code, reason) = closed(&mut events).await;
        assert_eq!(code, 1008, "{first}");
        assert!(reason.contains("first frame"), "{reason}");
        // The server sees only the close, never the frame.
        match settled(server.next()).await {
            Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1008),
            other => panic!("{first} reached the server: {other:?}"),
        }
        assert_eq!(h.bridge.send(id, HELLO.into()), Err(ENDED.into()));
    }
    assert_eq!(h.mints(), 1, "a 1008 keeps the bearer");
}

#[tokio::test]
async fn each_auth_end_and_each_renewal_mints_once_and_never_reoffers_the_bearer() {
    let mut h = Harness::new(None, true);
    let (_, mut events, mut server, first) = h.session(false).await;
    let mut offered = vec![first];
    // 4401 after a welcome (a restart), 4403 (revocation), 4401 before a welcome.
    let ends: [(u16, bool); 3] = [(4401, true), (4403, false), (4401, false)];
    for (code, welcomed) in ends {
        if welcomed {
            server
                .send(Message::Text(r#"{"t":"welcome"}"#.into()))
                .await
                .unwrap();
            assert!(matches!(events.recv().await, Some(Event::Message { .. })));
        }
        close_from_server(&mut server, code, "auth").await;
        assert_eq!(closed(&mut events).await, (code, "auth".into()));
        let (opened, next_events, next_server, token) = h.session(false).await;
        assert!(
            !opened.fresh,
            "the client saw the {code}, so it caused the mint"
        );
        assert!(
            !offered.contains(&token),
            "{code}: a refused bearer was offered again"
        );
        offered.push(token);
        (events, server) = (next_events, next_server);
    }
    assert_eq!(h.mints(), 4);
    let (opened, _, _, token) = h.session(true).await;
    assert!(!opened.fresh);
    assert!(!offered.contains(&token));
    assert_eq!(h.mints(), 5);
}

#[tokio::test]
async fn a_1006_or_1001_keeps_the_bearer() {
    let mut h = Harness::new(None, true);
    let (_, mut events, mut server, first) = h.session(false).await;
    close_from_server(&mut server, 1001, "going away").await;
    assert_eq!(closed(&mut events).await, (1001, "going away".into()));
    let (opened, mut events, server, second) = h.session(false).await;
    drop(server);
    assert_eq!(closed(&mut events).await, (1006, String::new()));
    let (_, _, _, third) = h.session(false).await;
    assert_eq!(
        (first.as_str(), second.as_str(), third.as_str()),
        ("bearer-1", "bearer-1", "bearer-1")
    );
    assert!(!opened.fresh);
    assert_eq!(h.mints(), 1);
}

#[tokio::test]
async fn fresh_marks_only_a_mint_the_client_did_not_cause() {
    let mut h = Harness::new(Some(1_000), true);
    let (opened, _, _, _) = h.session(false).await;
    assert!(opened.fresh, "the first bearer");
    // A 4403 on a socket the webview had already closed: it never saw it.
    let (opened, mut events, mut server, _) = h.session(false).await;
    assert_eq!((opened.fresh, opened.expires_in_ms), (false, Some(1_000)));
    h.bridge
        .close(opened.id, Some(1000), Some("replaced".into()))
        .unwrap();
    close_from_server(&mut server, 4403, "revoked").await;
    assert_eq!(closed(&mut events).await.0, 4403);
    let (opened, _, _, token) = h.session(false).await;
    assert_eq!((opened.fresh, token.as_str()), (true, "bearer-2"));
    // The bearer runs out on the suspend-inclusive clock: a lost bearer.
    h.clock.fetch_add(300, Ordering::Relaxed);
    let (opened, _) = h.open(false).await;
    assert_eq!(opened.unwrap().expires_in_ms, Some(700));
    h.clock.fetch_add(700, Ordering::Relaxed);
    let (opened, _) = h.open(false).await;
    let opened = opened.unwrap();
    assert_eq!((opened.fresh, opened.expires_in_ms), (true, Some(1_000)));
    assert_eq!(h.mints(), 3);
}

#[tokio::test]
async fn an_unreachable_socket_resolves_then_closes_1006() {
    let h = Harness::new(None, false);
    let (opened, mut events) = h.open(false).await;
    assert!(opened.unwrap().fresh);
    assert_eq!(closed(&mut events).await, (1006, String::new()));
    let (opened, mut events) = h.open(false).await;
    assert!(!opened.unwrap().fresh);
    assert_eq!(closed(&mut events).await, (1006, String::new()));
    assert_eq!(h.mints(), 1, "an outage keeps the bearer");
}

#[tokio::test]
async fn a_mint_refusal_rejects_with_its_message_and_never_the_bearer() {
    let mut h = Harness::new(None, true);
    *h.refuse_mint.lock().unwrap() = Some("The flk server refused the mint: pane.".into());
    let (opened, mut events) = h.open(false).await;
    assert_eq!(
        opened,
        Err(rejection("The flk server refused the mint: pane."))
    );
    no_events(&mut events);
    *h.refuse_mint.lock().unwrap() = None;
    let (_, _, _, token) = h.session(false).await;
    *h.refuse_mint.lock().unwrap() = Some(format!("busy (echo: {token})"));
    let (opened, _) = h.open(true).await;
    assert_eq!(opened, Err(rejection("busy (echo: <redacted>)")));
    // A failed renewal keeps the bearer the session runs on.
    *h.refuse_mint.lock().unwrap() = None;
    let (_, _, _, kept) = h.session(false).await;
    assert_eq!(kept, token);
    assert_eq!(h.fake.connections.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn refusals_before_the_upgrade_map_as_section_6_says() {
    let mut h = Harness::new(None, true);
    let connections = |h: &Harness| h.fake.connections.load(Ordering::Relaxed);
    // 403 operator_only: rejects, no retry, no frame.
    h.fake
        .refuse(403, "operator_only", "Local operator required");
    let (opened, mut events) = h.open(false).await;
    let message = opened.unwrap_err();
    assert!(
        message.contains("local operator")
            && message.contains("flk pane")
            && message.contains("#2293"),
        "{message}"
    );
    assert_eq!(connections(&h), 1);
    no_events(&mut events);
    // 403 peer_process_changed: one retry on a fresh connection.
    h.fake.refuse(403, "peer_process_changed", "changed");
    let (opened, _) = h.open(false).await;
    assert!(opened.is_ok());
    assert_eq!(connections(&h), 3);
    drop(h.fake.accept().await);
    // Twice: rejects with the server's message, never the bearer it echoes.
    h.fake.refuse(403, "peer_process_changed", "changed");
    h.fake
        .refuse(403, "peer_process_changed", "changed for bearer-1");
    let (opened, mut events) = h.open(false).await;
    let message = opened.unwrap_err();
    assert!(
        message.contains("peer_process_changed: changed for <redacted>"),
        "{message}"
    );
    assert_eq!(connections(&h), 5);
    no_events(&mut events);
    // 503 preauth_capacity: resolves, then the bridge's own 1013.
    h.fake.refuse(
        503,
        "preauth_capacity",
        "Pre-authentication capacity exhausted",
    );
    let (opened, mut events) = h.open(false).await;
    assert!(opened.is_ok());
    assert_eq!(closed(&mut events).await, (1013, "preauth_capacity".into()));
    assert_eq!(connections(&h), 6);
    // Any other status, or an unknown code: a defect, rejected with both.
    let defects = [
        (500, "internal_error"),
        (403, "nope"),
        (503, "auth_rate_limited"),
        (429, "auth_rate_limited"),
    ];
    for (status, code) in defects {
        h.fake.refuse(status, code, "x bearer-1");
        let (opened, mut events) = h.open(false).await;
        let message = opened.unwrap_err();
        assert!(
            message.contains(&format!("HTTP {status} ({code})")),
            "{message}"
        );
        assert!(!message.contains("bearer-1"), "{message}");
        no_events(&mut events);
    }
    assert_eq!(connections(&h), 10);
    assert_eq!(
        h.mints(),
        1,
        "no refusal before the upgrade ends the bearer"
    );
}

#[tokio::test]
async fn the_bearer_never_reaches_the_webview_or_debug() {
    let mut h = Harness::new(Some(5_000), true);
    let (opened, mut events, mut server, token) = h.session(false).await;
    assert_eq!(token, "bearer-1");
    let echoed = format!(r#"{{"t":"error","said":"{token}"}}"#);
    server.send(Message::Text(echoed.into())).await.unwrap();
    close_from_server(&mut server, 4400, &format!("bad {token}")).await;
    let mut delivered = vec![serde_json::to_string(&opened).unwrap()];
    while let Some(event) = events.recv().await {
        delivered.push(serde_json::to_string(&event).unwrap());
    }
    assert_eq!(delivered.len(), 3, "{delivered:?}");
    for text in &delivered {
        assert!(!text.contains(&token), "{text}");
    }
    let (_, _, _, _) = h.session(false).await;
    let debug = format!("{:?}", h.bridge);
    assert!(!debug.contains("bearer-"), "{debug}");
}

#[tokio::test]
async fn a_reload_ends_the_old_pages_sessions_at_the_server() {
    let mut h = Harness::new(None, true);
    let (_, mut events, mut server, _) = h.session(false).await;
    // Another webview's session is not the reloaded page's.
    let (tx, mut other_events) = mpsc::unbounded_channel();
    let sink: Sink = Arc::new(move |event| tx.send(event).is_ok());
    let other = h.bridge.open("other", false, sink).await.unwrap();
    let mut other_server = h.fake.accept().await;
    assert_eq!(h.bridge.live(), 2);
    // The hook path: `on_page_load` Started, or the window destroyed.
    h.bridge.webview_gone(OWNER);
    assert_eq!(
        h.bridge.live(),
        1,
        "the reloaded page's session left the table"
    );
    match settled(server.next()).await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1001),
        other => panic!("the server never saw the close: {other:?}"),
    }
    finish_close(&mut server).await;
    // Nobody is left to tell: the gone page gets no event.
    assert!(settled(events.recv()).await.is_none());
    h.bridge.webview_gone("other");
    assert_eq!(h.bridge.live(), 0);
    assert!(matches!(
        settled(other_server.next()).await,
        Some(Ok(Message::Close(Some(_))))
    ));
    finish_close(&mut other_server).await;
    assert!(settled(other_events.recv()).await.is_none());
    assert_eq!(h.bridge.send(other.id, HELLO.into()), Err(ENDED.into()));
}

#[tokio::test]
async fn a_send_to_a_dead_channel_closes_and_removes_the_session() {
    let mut h = Harness::new(None, true);
    let (opened, events, mut server, _) = h.session(false).await;
    drop(events); // the webview's channel is gone: every send now fails
    server
        .send(Message::Text(r#"{"t":"welcome"}"#.into()))
        .await
        .unwrap();
    match settled(server.next()).await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1001),
        other => panic!("the server never saw the close: {other:?}"),
    }
    assert_eq!(h.bridge.live(), 0);
    assert_eq!(h.bridge.send(opened.id, "{}".into()), Err(ENDED.into()));
}

#[tokio::test]
async fn every_rejection_says_why_and_offers_the_remote_path_never_the_bearer() {
    let mut h = Harness::new(None, true);
    let (_, _, _, token) = h.session(false).await;
    let echo = format!("echoes {token}");
    // Every mint failure, as `mint::mint_with` and the ptrace guard word it.
    let refused = |code: &str| crate::mint::Failure::Refused {
        code: code.into(),
        message: echo.clone(),
    };
    let failures = [
        vec![Err(refused("operator_only"))],
        vec![
            Err(refused("peer_process_changed")),
            Err(refused("peer_process_changed")),
        ],
        vec![Err(refused("session_busy"))],
        vec![Err(refused("invalid_params"))],
        vec![Err(refused("internal_error"))],
        vec![
            Err(crate::mint::Failure::NoServer),
            Err(crate::mint::Failure::NoServer),
        ],
        vec![Err(crate::mint::Failure::Timeout)],
        vec![Err(crate::mint::Failure::Io(echo.clone()))],
    ];
    let mut whys: Vec<String> = failures
        .into_iter()
        .map(|mut replies| {
            replies.reverse();
            crate::mint::mint_with(|| replies.pop().unwrap(), || Ok(())).unwrap_err()
        })
        .collect();
    let mut no_start = vec![Err(crate::mint::Failure::NoServer)];
    whys.push(
        crate::mint::mint_with(|| no_start.pop().unwrap(), || Err(echo.clone())).unwrap_err(),
    );
    whys.push(crate::mint::ptrace_guard(|| Ok("0".into())).unwrap_err());
    let mut messages = Vec::new();
    for why in whys {
        *h.refuse_mint.lock().unwrap() = Some(why);
        messages.push(h.open(true).await.0.unwrap_err());
    }
    *h.refuse_mint.lock().unwrap() = None;
    // Every refusal before the upgrade.
    h.fake.refuse(403, "operator_only", &echo);
    h.fake.refuse(403, "peer_process_changed", &echo);
    h.fake.refuse(403, "peer_process_changed", &echo);
    h.fake.refuse(500, "internal_error", &echo);
    h.fake.refuse(403, "nope", &echo);
    for _ in 0..4 {
        messages.push(h.open(false).await.0.unwrap_err());
    }
    assert_eq!(messages.len(), 14);
    for message in &messages {
        assert!(message.ends_with(REMOTE), "{message}");
        assert_eq!(message.matches("#2293").count(), 1, "{message}");
        assert!(!message.contains(&token), "{message}");
    }
}

#[tokio::test]
async fn an_end_on_an_older_bearer_leaves_the_current_one() {
    let mut h = Harness::new(None, true);
    let (_, mut old_events, mut old_server, old) = h.session(false).await;
    let (_, _, _, current) = h.session(true).await;
    assert_ne!(old, current);
    close_from_server(&mut old_server, 4403, "revoked").await;
    assert_eq!(closed(&mut old_events).await.0, 4403);
    let (opened, _, _, kept) = h.session(false).await;
    assert_eq!(kept, current, "the refusal was for an older bearer");
    assert!(!opened.fresh);
    assert_eq!(h.mints(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_answers_the_upgrade_is_an_outage_within_the_deadline() {
    let h = Harness::new(None, true);
    h.fake.script(Script::Hang);
    let started = tokio::time::Instant::now();
    // The paused clock runs only the bridge's own deadline; this one bounds a hang.
    let outcome = tokio::time::timeout(DEADLINE * 2, h.open(false)).await;
    let (opened, mut events) = outcome.expect("the open hung past its deadline");
    assert!(opened.is_ok());
    assert_eq!(
        events.try_recv(),
        Ok(Event::Close {
            code: 1006,
            reason: String::new()
        })
    );
    assert!(started.elapsed() >= DEADLINE && started.elapsed() < DEADLINE * 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_mint_does_not_block_another_open() {
    let mut h = Harness::new(None, true);
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release, release_rx) = std::sync::mpsc::sync_channel(1);
    *h.hold_mint.lock().unwrap() = Some((entered_tx, release_rx));
    let bridge = h.bridge.clone();
    let (tx, _first_events) = mpsc::unbounded_channel();
    let sink: Sink = Arc::new(move |event| tx.send(event).is_ok());
    let first = tokio::spawn(async move { bridge.open(OWNER, false, sink).await });
    tokio::task::spawn_blocking(move || entered.recv().unwrap())
        .await
        .unwrap();
    // The first open's mint is stuck; this one mints and connects regardless.
    let (opened, _, _, token) = settled(h.session(false)).await;
    assert_eq!(token, "bearer-1");
    assert!(opened.fresh);
    release.send(()).unwrap();
    assert!(settled(first).await.unwrap().is_ok());
    assert_eq!(h.mints(), 2);
}

#[tokio::test]
async fn a_closed_session_leaves_the_table() {
    let mut h = Harness::new(None, true);
    let (opened, mut events, mut server, _) = h.session(false).await;
    assert_eq!(h.bridge.live(), 1);
    close_from_server(&mut server, 1000, "done").await;
    assert_eq!(closed(&mut events).await.0, 1000);
    assert_eq!(h.bridge.live(), 0);
    assert_eq!(h.bridge.send(opened.id, "{}".into()), Err(ENDED.into()));
    assert_eq!(h.bridge.close(opened.id, None, None), Err(ENDED.into()));
}

#[tokio::test]
async fn a_handshake_that_is_not_http_is_an_outage() {
    let h = Harness::new(None, true);
    h.fake.script(Script::Garbage);
    let (opened, mut events) = h.open(false).await;
    assert!(opened.is_ok(), "{opened:?}");
    assert_eq!(closed(&mut events).await, (1006, String::new()));
}

#[tokio::test]
async fn a_binary_frame_is_closed_1003_and_never_forwarded_as_text() {
    let mut h = Harness::new(None, true);
    let (_, mut events, mut server, _) = h.session(false).await;
    server
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    assert_eq!(
        closed(&mut events).await,
        (1003, "the server sent a binary frame".into())
    );
    match settled(server.next()).await {
        Some(Ok(Message::Close(Some(frame)))) => assert_eq!(u16::from(frame.code), 1003),
        other => panic!("{other:?}"),
    }
}
