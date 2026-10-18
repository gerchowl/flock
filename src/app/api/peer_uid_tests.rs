use super::spawn;
use crate::api::schema::*;
use crate::app::App;
use crate::structured::identity::UidPolicy;
use std::time::Duration;

fn fixture(uid: Option<u32>) -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.runtime_config.uid_policy = UidPolicy {
        operator: 7,
        dedicated: Ok(Some(42)).into(),
    };
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(crate::platform::process_start_time(std::process::id()));
    app.current_api_peer_uid = Some(uid);
    app
}
fn owner() -> Request {
    Request {
        id: "owner".into(),
        method: Method::OwnerPolicyAdopt(OwnerPolicyParams {
            repo: "fixture-repo".into(),
            policy_version: None,
        }),
    }
}
fn send() -> Request {
    Request {
        id: "send".into(),
        method: Method::MsgSend(MsgSendParams {
            from_agent: Some("agent_atlas_claim".into()),
            to: MessageTarget::Pane {
                pane: "fixture-pane".into(),
            },
            body: "fixture".into(),
            correlation_id: Some("uid-fixture".into()),
            in_reply_to: None,
            intent: MsgIntent::Fyi,
        }),
    }
}
fn read() -> Request {
    Request {
        id: "read".into(),
        method: Method::WorkspaceList(EmptyParams {}),
    }
}
fn response(app: &mut App, request: Request) -> serde_json::Value {
    serde_json::from_str(&app.handle_api_request_after_internal_events_drained(request)).unwrap()
}
#[tokio::test]
async fn anonymous_dedicated_and_unknown_uid_callers_can_read_but_not_send_or_operate() {
    for uid in [Some(42), Some(99), None] {
        let mut app = fixture(uid);
        assert!(!app.caller_is_local_operator());
        assert_eq!(
            response(&mut app, owner())["error"]["code"],
            "operator_only"
        );
        assert_eq!(
            response(&mut app, send())["error"]["code"],
            "msg_not_allowed"
        );
        let raw = Request {
            id: "raw".into(),
            method: Method::PaneSendText(PaneSendTextParams {
                pane_id: "fixture-pane".into(),
                text: "fixture".into(),
                raw: true,
                write_timeout_ms: None,
            }),
        };
        assert_eq!(response(&mut app, raw)["error"]["code"], "msg_not_allowed");
        let close = Request {
            id: "close".into(),
            method: Method::PaneClose(PaneTarget {
                pane_id: "fixture-pane".into(),
            }),
        };
        assert_eq!(response(&mut app, close)["error"]["code"], "operator_only");

        assert!(response(&mut app, read()).get("result").is_some());
    }
    let mut app = fixture(Some(7));
    assert!(app.caller_is_local_operator());
    app.current_api_peer_pid = None;
    assert_eq!(
        response(&mut app, owner())["error"]["code"],
        "operator_only"
    );
    app.current_api_peer_pid = Some(std::process::id());
    app.runtime_config.uid_policy.dedicated = Err(()).into();
    assert!(!app.caller_is_local_operator());
    assert_eq!(
        response(&mut app, send())["error"]["code"],
        "msg_not_allowed"
    );
}
fn pane(app: &mut App, agent: bool) {
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("uid-pane")];
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    let terminal = app.state.workspaces[0]
        .pane_state(pane)
        .unwrap()
        .attached_terminal_id
        .clone();
    if agent {
        app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
    }
    crate::test_support::attest_pane_caller(app, pane);
    app.current_api_peer_birth = Some(
        app.current_api_peer_pid
            .and_then(crate::platform::process_start_time),
    );
}
#[tokio::test]
async fn uid_boundary_preserves_trusted_pty_agents_without_operator_authority() {
    for uid in [7, 42] {
        let mut app = fixture(Some(uid));
        pane(&mut app, true);
        assert!(matches!(
            app.spawn_caller(),
            spawn::SpawnCaller::Agent { .. }
        ));
        assert!(app.attested_sender_agent().is_some());
        assert!(!app.caller_is_local_operator());
    }
    let mut app = fixture(Some(42));
    pane(&mut app, false);
    assert_eq!(
        response(&mut app, owner())["error"]["code"],
        "operator_only"
    );
}
#[tokio::test]
async fn recovery_replay_preserves_uid_and_restores_the_outer_context() {
    let mut app = fixture(Some(7));
    let (respond_to, rx) = std::sync::mpsc::channel();
    app.park_until_mesh_recovered(
        owner(),
        app.current_api_peer_pid,
        app.current_api_peer_birth,
        Some(Some(42)),
        None,
        respond_to,
    );
    app.replay_recovery_parked();
    let reply: serde_json::Value =
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
    assert_eq!(reply["error"]["code"], "operator_only");
    assert_eq!(app.current_api_peer_uid, Some(Some(7)));
}

struct OwnedClient(std::process::Child);
impl Drop for OwnedClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn detached_socket_client_carries_kernel_uid_through_production_dispatch() {
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    let root = crate::test_support::TempDir::socket();
    let socket = root.path().join("uid.sock");
    assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let uid = crate::platform::peer::effective_uid();
    // Pure account policy maps the owned client's actual UID to the dedicated
    // role; this never changes the machine's accounts or needs a privileged test.
    app.runtime_config.uid_policy = UidPolicy {
        operator: uid.wrapping_add(1),
        dedicated: Ok(Some(uid)).into(),
    };
    let script = r#"
import os, socket, json, sys
os.setsid()
responses = []
for request in json.loads(sys.argv[2]):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10)
        client.connect(sys.argv[1])
        client.sendall((json.dumps(request) + '\n').encode())
        with client.makefile('r') as reader:
            responses.append(json.loads(reader.readline()))
print(json.dumps(responses), flush=True)
sys.stdin.read()
"#;
    let mut child = OwnedClient(
        crate::process::TracedCommand::new("python3", "test")
            .args(["-c", script])
            .arg(&socket)
            .arg(serde_json::to_string(&[owner(), send(), read()]).unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn_traced()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    let output = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = line_tx.send(result);
    });
    let worker = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut poll = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: the initialized pollfd borrows the owned listener.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 10_000) },
                1,
                "owned producer silent"
            );
            let (stream, _) = listener.accept().unwrap();
            crate::api::server::handle_connection(
                stream,
                &tx,
                &crate::api::EventHub::default(),
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                None,
                &std::sync::Mutex::default(),
            )
            .unwrap();
        }
    });
    for _ in 0..3 {
        let message = tokio::time::timeout(Duration::from_secs(10), app.api_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.peer_uid, Some(uid));
        assert_eq!(message.peer_pid, Some(child.0.id()));
        assert!(message.peer_birth.is_some());
        app.handle_api_request_message(message);
    }
    worker.join().unwrap();
    let result = line_rx.recv_timeout(Duration::from_secs(10));
    drop(child);
    output.join().unwrap();
    let replies: serde_json::Value = serde_json::from_str(&result.unwrap().unwrap()).unwrap();
    assert_eq!(replies[0]["error"]["code"], "operator_only", "{replies}");
    assert_eq!(replies[1]["error"]["code"], "msg_not_allowed", "{replies}");
    assert!(replies[2].get("result").is_some(), "{replies}");
    assert_eq!(app.current_api_peer_uid, None);
}

#[tokio::test]
async fn dedicated_and_unknown_callers_cannot_stop_the_server() {
    for uid in [Some(42), Some(99), None] {
        let mut app = fixture(uid);
        let request = Request {
            id: "stop".into(),
            method: Method::ServerStop(EmptyParams {}),
        };
        assert_eq!(
            response(&mut app, request)["error"]["code"],
            "operator_only"
        );
        assert!(!app.state.should_quit);
    }
}

#[tokio::test]
async fn unknown_uid_cannot_use_even_proven_pane_evidence_as_authority() {
    for uid in [Some(99), None] {
        let mut app = fixture(uid);
        pane(&mut app, true);
        assert!(app.caller_pane().is_some());
        assert!(app.attested_sender_agent().is_none());
        assert!(!app.caller_is_local_operator());
        assert_eq!(
            response(&mut app, send())["error"]["code"],
            "msg_not_allowed"
        );
        assert_eq!(
            response(&mut app, owner())["error"]["code"],
            "operator_only"
        );
        assert!(response(&mut app, read()).get("result").is_some());
    }
}
#[tokio::test]
async fn operator_without_a_dedicated_account_keeps_normal_mutations() {
    let mut app = fixture(Some(7));
    app.runtime_config.uid_policy.dedicated = Ok(None).into();
    let result = response(
        &mut app,
        Request {
            id: "stop".into(),
            method: Method::ServerStop(EmptyParams {}),
        },
    );
    assert!(result.get("result").is_some(), "{result}");
    assert!(app.state.should_quit);
}
