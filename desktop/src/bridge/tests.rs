//! The bridge against a fake `flock-ws.sock` (ADR-0038 §6's "the bridge: …"
//! and "the bridge's pre-upgrade mapping" tests). Never the live server.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, AtomicUsize},
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

/// A fake `flock-ws.sock`: each connection takes the next scripted HTTP
/// refusal, or else upgrades and is handed to the test.
struct Fake {
    dir: PathBuf,
    accepted: UnboundedReceiver<Server>,
    refusals: Arc<Mutex<VecDeque<(u16, String)>>>,
    connections: Arc<AtomicUsize>,
}

impl Fake {
    fn path(&self) -> PathBuf {
        self.dir.join("flock-ws.sock")
    }

    fn refuse(&self, status: u16, code: &str, message: &str) {
        let body = json!({ "error": { "code": code, "message": message } }).to_string();
        self.refusals.lock().unwrap().push_back((status, body));
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
    clock: Arc<AtomicU64>,
}

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
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    connections.fetch_add(1, Ordering::Relaxed);
                    let refusal = refusals.lock().unwrap().pop_front();
                    match refusal {
                        Some((status, body)) => respond(stream, status, &body).await,
                        None => {
                            drop(tx.send(tokio_tungstenite::accept_async(stream).await.unwrap()))
                        }
                    }
                }
            });
        }
        let (mints, refuse_mint) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
        let (count, refuse) = (mints.clone(), refuse_mint.clone());
        let mint: Mint = Arc::new(move || {
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
            Ok(fake.path()),
            mint,
            Arc::new(move || now.load(Ordering::Relaxed)),
        );
        Self {
            bridge,
            fake,
            mints,
            refuse_mint,
            clock,
        }
    }

    async fn open(&self, renew: bool) -> (Result<Opened, String>, UnboundedReceiver<Event>) {
        let (tx, events) = mpsc::unbounded_channel();
        let opened = self
            .bridge
            .open(renew, Arc::new(move |event| drop(tx.send(event))))
            .await;
        (opened, events)
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
    assert_eq!(opened, Err("The flk server refused the mint: pane.".into()));
    no_events(&mut events);
    *h.refuse_mint.lock().unwrap() = None;
    let (_, _, _, token) = h.session(false).await;
    *h.refuse_mint.lock().unwrap() = Some(format!("busy (echo: {token})"));
    let (opened, _) = h.open(true).await;
    assert_eq!(opened, Err("busy (echo: <redacted>)".into()));
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
