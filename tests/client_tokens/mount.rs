//! Full headless-server paths; every executable, socket, config and token is isolated.
use super::*;
use std::{
    io::Read,
    net::{SocketAddr, TcpStream},
};

// Test expectations use the same declared, immutable build input as the server.
include!(concat!(env!("OUT_DIR"), "/gui_assets.rs"));

struct Endpoint {
    address: SocketAddr,
    code: String,
}
fn endpoint(server: &Server) -> Endpoint {
    let response = server.api("client.open", json!({}));
    let url = response["result"]["url"].as_str().expect("operator URL");
    let (authority, code) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once("/#code=")
        .unwrap();
    let address: SocketAddr = authority.parse().unwrap();
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0);
    Endpoint {
        address,
        code: code.into(),
    }
}
fn stream(address: SocketAddr) -> TcpStream {
    let socket = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
}
fn http(
    endpoint: &Endpoint,
    path: &str,
    host: &str,
    origin: Option<&str>,
    body: Option<&str>,
) -> String {
    let mut socket = stream(endpoint.address);
    let method = if body.is_some() { "POST" } else { "GET" };
    let origin = origin
        .map(|v| format!("Origin: {v}\r\n"))
        .unwrap_or_default();
    let body = body.unwrap_or("");
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    response
}
fn exchange(endpoint: &Endpoint, code: &str) -> String {
    http(
        endpoint,
        "/v1/client/exchange",
        &endpoint.address.to_string(),
        Some(&format!("http://{}", endpoint.address)),
        Some(&json!({"code":code}).to_string()),
    )
}
fn refusal(response: &str) -> (&str, &str) {
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(headers
        .lines()
        .any(|line| line == "cache-control: no-store"));
    (headers.lines().next().unwrap(), body)
}
fn bearer(endpoint: &Endpoint) -> String {
    let response = exchange(endpoint, &endpoint.code);
    assert!(response.starts_with("HTTP/1.1 200"));
    serde_json::from_str::<Value>(response.split_once("\r\n\r\n").unwrap().1).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned()
}
struct WebSocket(TcpStream);
impl WebSocket {
    fn upgrade(endpoint: &Endpoint, host: &str, origin: Option<&str>) -> (Self, String) {
        let mut socket = stream(endpoint.address);
        let origin = origin
            .map(|v| format!("Origin: {v}\r\n"))
            .unwrap_or_default();
        write!(socket, "GET /v1/ws HTTP/1.1\r\nHost: {host}\r\n{origin}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").unwrap(); // gitleaks:allow -- RFC 6455 public handshake example nonce
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
            assert!(response.len() < 8192);
        }
        (Self(socket), String::from_utf8(response).unwrap())
    }
    fn open(endpoint: &Endpoint) -> Self {
        let (socket, response) = Self::upgrade(
            endpoint,
            &endpoint.address.to_string(),
            Some(&format!("http://{}", endpoint.address)),
        );
        assert!(response.starts_with("HTTP/1.1 101"));
        socket
    }
    fn send(&mut self, opcode: u8, bytes: &[u8]) {
        assert!(bytes.len() <= u16::MAX as usize);
        let mut frame = vec![0x80 | opcode];
        if bytes.len() < 126 {
            frame.push(0x80 | bytes.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        }
        let mask = [1, 2, 3, 4];
        frame.extend_from_slice(&mask);
        frame.extend(bytes.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.0.write_all(&frame).unwrap();
    }
    fn receive(&mut self) -> (u8, Vec<u8>) {
        let mut header = [0; 2];
        self.0.read_exact(&mut header).unwrap();
        assert_eq!(header[1] & 0x80, 0);
        let mut len = u64::from(header[1] & 127);
        if len == 126 {
            let mut b = [0; 2];
            self.0.read_exact(&mut b).unwrap();
            len = u16::from_be_bytes(b).into();
        } else if len == 127 {
            let mut b = [0; 8];
            self.0.read_exact(&mut b).unwrap();
            len = u64::from_be_bytes(b);
        }
        assert!(len <= 65_536);
        let mut bytes = vec![0; len as usize];
        self.0.read_exact(&mut bytes).unwrap();
        (header[0] & 15, bytes)
    }
    fn hello(&mut self, token: &str) -> (u8, Vec<u8>) {
        self.send(1, json!({"t":"hello","protocol":1,"client":{"name":"fixture","version":"1"},"token":token}).to_string().as_bytes());
        self.receive()
    }
}
#[test]
fn real_server_mounts_gui_csp_code_exchange_and_authenticated_wire() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    assert_eq!(
        server.api("ping", json!({}))["result"]["client_listener"],
        "listening"
    );
    let index = http(&endpoint, "/", &endpoint.address.to_string(), None, None);
    if let Some(expected) = GuiAssets::get("index.html") {
        assert!(index.starts_with("HTTP/1.1 200"));
        assert_eq!(
            index.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            expected.data.as_ref()
        );
    } else {
        assert!(index.starts_with("HTTP/1.1 503"));
        assert!(index.contains("server_unavailable"));
    }
    let csp = index
        .lines()
        .find(|line| line.starts_with("content-security-policy:"))
        .unwrap();
    for part in [
        "default-src 'self'",
        "script-src 'self'",
        "style-src 'self'",
        "object-src 'none'",
        "base-uri 'none'",
        "frame-ancestors 'none'",
        "form-action 'none'",
    ] {
        assert!(csp.contains(part));
    }
    assert!(csp.contains(&format!(
        "connect-src 'self' ws://{}/v1/ws",
        endpoint.address
    )));
    let config = http(
        &endpoint,
        "/config.js",
        &endpoint.address.to_string(),
        None,
        None,
    );
    let expected = if GuiAssets::get("index.html").is_some() {
        "HTTP/1.1 404"
    } else {
        "HTTP/1.1 503"
    };
    assert!(config.starts_with(expected));

    let token = bearer(&endpoint);
    let used = exchange(&endpoint, &endpoint.code);
    let unknown = exchange(&endpoint, &"0".repeat(64));
    assert!(used.starts_with("HTTP/1.1 401"));
    assert_eq!(refusal(&used), refusal(&unknown));
    let mut socket = WebSocket::open(&endpoint);
    let (kind, welcome) = socket.hello(&token);
    assert_eq!(kind, 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&welcome).unwrap()["t"],
        "welcome"
    );
    socket.send(2, b"unsupported");
    let (kind, close) = socket.receive();
    assert_eq!(kind, 8);
    assert_eq!(u16::from_be_bytes(close[..2].try_into().unwrap()), 1008);
}
#[test]
fn real_server_refuses_host_origin_and_duplicate_origin_before_upgrade() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let origin = format!("http://{}", endpoint.address);
    for (host, origin) in [
        ("other.invalid".into(), Some(origin.as_str())),
        (endpoint.address.to_string(), Some("http://other.invalid")),
        (endpoint.address.to_string(), None),
        (endpoint.address.to_string(), Some("null")),
        (
            endpoint.address.to_string(),
            Some("http://other.invalid\r\nOrigin: http://other.invalid"),
        ),
    ] {
        let (_, response) = WebSocket::upgrade(&endpoint, &host, origin);
        assert!(response.starts_with("HTTP/1.1 403"));
    }
    let (_, response) = WebSocket::upgrade(&endpoint, &endpoint.address.to_string(), Some(&origin));
    assert!(response.starts_with("HTTP/1.1 101"));
}
#[test]
fn real_server_bounds_one_principal_and_pending_hellos_without_evicting_a_sibling() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token = bearer(&endpoint);
    let mut active = Vec::new();
    for _ in 0..4 {
        let mut socket = WebSocket::open(&endpoint);
        assert_eq!(socket.hello(&token).0, 1);
        active.push(socket);
    }
    let mut excess = WebSocket::open(&endpoint);
    let (kind, close) = excess.hello(&token);
    assert_eq!(kind, 8);
    assert_eq!(u16::from_be_bytes(close[..2].try_into().unwrap()), 1001);
    // EOF proves the refused handshake released its probation lease before filling it.
    assert_eq!(excess.0.read(&mut [0]).unwrap(), 0);
    let mut pending = Vec::new();
    for _ in 0..4 {
        pending.push(WebSocket::open(&endpoint));
    }
    let (_, response) = WebSocket::upgrade(
        &endpoint,
        &endpoint.address.to_string(),
        Some(&format!("http://{}", endpoint.address)),
    );
    assert!(response.starts_with("HTTP/1.1 503"));
    active[0].send(2, b"policy");
    let (kind, close) = active[0].receive();
    assert_eq!(kind, 8);
    assert_eq!(u16::from_be_bytes(close[..2].try_into().unwrap()), 1008);
}
#[test]
fn real_server_caps_incomplete_http_connections_and_shutdown_closes_them() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let mut held = Vec::new();
    for _ in 0..32 {
        held.push(stream(endpoint.address));
    }
    let mut excess = stream(endpoint.address);
    let sent = write!(
        excess,
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        endpoint.address
    );
    let mut byte = [0];
    match sent.and_then(|_| excess.read(&mut byte)) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) => {}
        outcome => panic!("over-capacity HTTP admission must close: {outcome:?}"),
    }
    assert!(server.api("server.stop", json!({})).get("result").is_some());
    server.owner.wait_exited(Duration::from_secs(10)).unwrap();
    for mut socket in held {
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    }
    drop(server);
    let _rebound = std::net::TcpListener::bind(endpoint.address)
        .expect("shutdown releases listener ownership");
}
#[test]
fn real_server_exchange_internal_failure_is_typed_uniform_and_secret_free() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token_record = fs::read_to_string(server.directory().join("operator.token")).unwrap();
    let secret = serde_json::from_str::<Value>(&token_record).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let baseline = exchange(&endpoint, &"0".repeat(64));
    fs::set_permissions(server.directory(), fs::Permissions::from_mode(0o777)).unwrap();
    let refused = exchange(&endpoint, &endpoint.code);
    fs::set_permissions(server.directory(), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(refusal(&baseline), refusal(&refused));
    let log = fs::read_to_string(
        server
            .config
            .join(support::paths::app_dir_name())
            .join("flock-server.log"),
    )
    .unwrap();
    assert!(log.contains("client.exchange.internal_failure"));
    assert!(log.contains("unsafe_store"));
    assert!(!log.contains(&endpoint.code));
    assert!(!log.contains(&secret));
}
#[test]
fn real_server_refuses_active_and_unknown_funnel_producers() {
    for probe in [
        "#!/bin/sh\nprintf '%s\\n' 'not-json'\n",
        "#!/bin/sh\nexit 1\n",
        "#!/bin/sh\nprintf '%s\\n' 'null'\n",
        "#!/bin/sh\nprintf '%s\\n' '{\"AllowFunnel\":{\"fixture.example:443\":\"unknown\"}}'\n",
    ] {
        assert!(Server::with_probe(probe).is_err());
    }
}
#[test]
fn real_server_refuses_serve_and_funnel_targeting_its_bound_port() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token = bearer(&endpoint);
    for public in [false, true] {
        let config = json!({"AllowFunnel":{"fixture.example:443":public},"Web":{"fixture.example:443":{"Handlers":{"/":{"Proxy":format!("http://{}",endpoint.address)}}}}});
        let probe = format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", config);
        fs::write(server.root.join("tools/tailscale"), probe).unwrap();
        // The importer adopts the real port-0 listener, so no reservation/rebind race exists.
        let refused = server.api("server.live_handoff", json!({}));
        assert_eq!(refused["error"]["code"], "handoff_failed", "{refused}");
        assert_eq!(
            refused["error"]["data"]["kind"], "import_refused",
            "{refused}"
        );
        assert!(refused.get("result").is_none(), "{refused}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("tailscale serve/funnel may expose the client port"),
            "{refused}"
        );
        // Park until rollback recovery is processed, then prove the exporter still serves.
        assert!(server.api("msg.list", json!({})).get("result").is_some());
        assert_eq!(
            server.api("ping", json!({}))["result"]["client_listener"],
            "listening"
        );
        assert_eq!(self::endpoint(&server).address, endpoint.address);
        let mut socket = WebSocket::open(&endpoint);
        assert_eq!(socket.hello(&token).0, 1);
    }
}

#[test]
fn real_handoff_transfers_the_bound_listener_and_survives_import_refusal() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token = bearer(&endpoint);
    assert!(
        server.api("workspace.list", json!({}))["result"]["workspaces"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(server.api("msg.list", json!({})).get("result").is_some());
    let failure = server.api("server.live_handoff", json!({"expected_protocol":u32::MAX}));
    assert_eq!(failure["error"]["code"], "handoff_failed");
    let mut old = WebSocket::open(&endpoint);
    assert_eq!(old.hello(&token).0, 1);
    // Transfer a PTY before the listener, rather than testing only the empty FD batch.
    let created = server.api("workspace.create", json!({"cwd":server.root,"focus":true}));
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("isolated shell pane");
    // This read parks until the real rollback-recovery event is handled.
    assert!(server.api("msg.list", json!({})).get("result").is_some());
    let success = server.api("server.live_handoff", json!({}));
    assert!(success.get("result").is_some(), "{success}");
    // Ownership acknowledgement precedes exporter shutdown; use its real exit event.
    server.owner.wait_exited(Duration::from_secs(10)).unwrap();
    assert!(std::net::TcpListener::bind(endpoint.address).is_err());
    let mut replacement = WebSocket::open(&endpoint);
    assert_eq!(replacement.hello(&token).0, 1);
    let new_endpoint = self::endpoint(&server);
    assert_eq!(new_endpoint.address, endpoint.address);
    assert!(server
        .api("pane.get", json!({"pane_id":pane}))
        .get("result")
        .is_some());
}

#[test]
fn real_server_client_open_keeps_actual_endpoint_after_config_reload() {
    let server = Server::start();
    let before = endpoint(&server);
    assert!(server
        .api("server.reload_config", json!({}))
        .get("result")
        .is_some());
    let after = endpoint(&server);
    assert_eq!(before.address, after.address);
    let token = bearer(&after);
    let mut socket = WebSocket::open(&after);
    assert_eq!(socket.hello(&token).0, 1);
}

#[test]
fn real_server_completed_http_releases_slot_even_if_client_keeps_write_half_open() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let mut retained = Vec::new();
    for _ in 0..33 {
        let mut socket = stream(endpoint.address);
        write!(
            socket,
            "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            endpoint.address
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let expected = if GuiAssets::get("index.html").is_some() {
            "HTTP/1.1 200"
        } else {
            "HTTP/1.1 503"
        };
        assert!(response.starts_with(expected));
        retained.push(socket);
    }
}
#[test]
fn real_server_fin_after_complete_request_still_receives_response() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let mut socket = stream(endpoint.address);
    write!(
        socket,
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        endpoint.address
    )
    .unwrap();
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    let expected = if GuiAssets::get("index.html").is_some() {
        "HTTP/1.1 200"
    } else {
        "HTTP/1.1 503"
    };
    assert!(response.starts_with(expected));
}

#[test]
fn real_server_silent_funnel_producer_fails_before_startup_ceiling() {
    let probe = r#"#!/bin/sh
exec 3<> "$FUNNEL_FIXTURE_GATE"
printf '%s\n' "$$" > "$FUNNEL_FIXTURE_ENTERED"
IFS= read -r blocked <&3
"#;
    let error = Server::with_probe(probe)
        .err()
        .expect("silent producer must refuse startup");
    assert!(
        error.to_string().contains("fixture_probe_entered=true"),
        "producer must reach its silent stage"
    );
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::TimedOut,
        "product timeout must terminate the producer before the fixture ceiling"
    );
}
#[test]
fn real_server_incomplete_http_header_has_a_deadline() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let mut socket = stream(endpoint.address);
    // Ten seconds is only a failure ceiling for the named five-second header deadline.
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(socket, "GET / HTTP/1.1\r\nHost: {}\r\n", endpoint.address).unwrap();
    let mut byte = [0];
    assert_eq!(socket.read(&mut byte).unwrap(), 0);
}
#[test]
fn real_server_oversized_exchange_body_is_the_same_uniform_refusal() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let unknown = exchange(&endpoint, &"0".repeat(64));
    let oversized = exchange(&endpoint, &"0".repeat(2048));
    assert_eq!(refusal(&unknown), refusal(&oversized));
    let token = bearer(&endpoint);
    let mut socket = WebSocket::open(&endpoint);
    assert_eq!(socket.hello(&token).0, 1);
}
#[test]
fn real_legacy_importer_refuses_descriptor_extension_before_any_commit() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token = bearer(&endpoint);
    assert!(
        server.api("workspace.list", json!({}))["result"]["workspaces"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(server.api("msg.list", json!({})).get("result").is_some());
    let observation = server.root.join("legacy-version.json");
    let importer = server.root.join("legacy-importer");
    let program=r#"#!/bin/sh
exec python3 - "$@" <<'PY'
import json,socket,sys
from pathlib import Path
peer=socket.socket(socket.AF_UNIX)
peer.settimeout(5)
peer.connect(sys.argv[3])
peer.sendall(sys.argv[4].encode()+b'\n')
manifest=json.loads(peer.makefile('rb').readline(65536))
Path(OBSERVATION).write_text(json.dumps({'version':manifest['version'],'listener':manifest.get('client_listener',False),'panes':len(manifest['panes'])}))
if manifest['version'] != 1:
    peer.sendall(b'error: legacy importer refuses descriptor extension\n')
else:
    peer.sendall(b'validated\n')
PY
"#.replace("OBSERVATION",&format!("{:?}",observation.to_str().unwrap()));
    fs::write(&importer, program).unwrap();
    fs::set_permissions(&importer, fs::Permissions::from_mode(0o700)).unwrap();
    let response = server.api("server.live_handoff", json!({"import_exe":importer}));
    assert_eq!(response["error"]["code"], "handoff_failed");
    assert_eq!(response["error"]["data"]["kind"], "import_refused");
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("legacy importer refuses descriptor extension"));
    let observed: Value = serde_json::from_slice(&fs::read(observation).unwrap()).unwrap();
    assert_eq!(observed, json!({"version":2,"listener":true,"panes":0}));
    let mut original = WebSocket::open(&endpoint);
    assert_eq!(original.hello(&token).0, 1);
}

#[test]
fn real_server_refuses_wildcard_and_ipv6_client_binds() {
    for bind in ["0.0.0.0:0", "[::1]:0"] {
        let result = Server::with_probe_bind(
            "#!/bin/sh\nprintf '%s\n' '{\"AllowFunnel\":{}}'\n",
            Some(bind.parse().unwrap()),
        );
        assert!(result.is_err(), "only numeric IPv4 loopback is permitted");
    }
}

#[test]
fn real_server_stalled_http_body_expires_but_upgraded_socket_stays_usable() {
    let server = Server::start();
    let endpoint = endpoint(&server);
    let token = bearer(&endpoint);
    let mut active = WebSocket::open(&endpoint);
    assert_eq!(active.hello(&token).0, 1);
    let mut stalled = stream(endpoint.address);
    stalled
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(stalled, "POST /v1/client/exchange HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{{", endpoint.address, endpoint.address).unwrap();
    match stalled.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        outcome => panic!("HTTP deadline must close the peer: {outcome:?}"),
    }
    active.send(2, b"still usable after HTTP ownership ends");
    let (kind, close) = active.receive();
    assert_eq!(kind, 8);
    assert_eq!(u16::from_be_bytes(close[..2].try_into().unwrap()), 1008);
}
