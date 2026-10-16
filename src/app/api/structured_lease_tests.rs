use crate::api::schema::*;
use crate::app::App;
use crate::config::Config;
use crate::structured::{lease::SessionLeases, runtime, transport::StreamLocation};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

struct OwnedRuntime {
    runtime: runtime::Runtime,
    worker: Option<std::thread::JoinHandle<()>>,
    exit: mpsc::Receiver<Result<runtime::Exit, crate::structured::launch::LaunchError>>,
}
impl OwnedRuntime {
    fn finish(&mut self) -> runtime::Exit {
        self.runtime.stop();
        let exit = self
            .exit
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded cleanup")
            .unwrap();
        self.worker.take().unwrap().join().unwrap();
        exit
    }
}
impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.runtime.stop();
            if self.exit.recv_timeout(Duration::from_secs(10)).is_ok() {
                let _ = self.worker.take().unwrap().join();
            }
        }
    }
}

const AGENT: &str = "agent_atlas_lease";
const SESSION: &str = "00000000-0000-4000-8000-000000000001";

#[tokio::test]
async fn fake_claude_socket_calls_have_agent_identity_and_no_operator_exemption() {
    let fixture = runtime::tests::Fixture::new();
    let root = fixture.root.path().to_owned();
    let socket = root.join("api.sock");
    assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("recipient")];
    app.state.ensure_test_terminals();
    app.state.config.fleet.agent_spawn_enabled = true;
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    let target = app.public_pane_id(0, pane).unwrap();
    fixture.file(
        "requests.json",
        &serde_json::to_vec(&[
            Request {
                id: "owner".into(),
                method: Method::OwnerPolicyAdopt(OwnerPolicyParams {
                    repo: root.display().to_string(),
                    policy_version: None,
                }),
            },
            Request {
                id: "delegates".into(),
                method: Method::DelegateList(DelegateListParams {
                    architect: "self".into(),
                    branch: None,
                    repo: None,
                }),
            },
            Request {
                id: "send".into(),
                method: Method::MsgSend(MsgSendParams {
                    from_agent: Some("agent_atlas_spoof".into()),
                    to: MessageTarget::Pane { pane: target },
                    body: "fixture message".into(),
                    correlation_id: Some("lease-call".into()),
                    in_reply_to: None,
                    intent: MsgIntent::Fyi,
                }),
            },
        ])
        .unwrap(),
        0o600,
    );
    let lease = app.session_leases.reserve(AGENT, SESSION, None).unwrap();
    let profile = fixture.profile("lease-calls", "2.1.296");
    let location = StreamLocation {
        data_dir: root.clone(),
        agent: AGENT.into(),
        session: SESSION.into(),
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (exit_tx, exit_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = runtime::run(
            &location.data_dir.clone(),
            profile,
            lease,
            location,
            |runtime| ready_tx.send(runtime).unwrap(),
        );
        let _ = exit_tx.send(result);
    });
    let mut owned = OwnedRuntime {
        runtime: ready_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        worker: Some(worker),
        exit: exit_rx,
    };
    let runtime = &owned.runtime;
    let socket_worker = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut poll = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: a single initialized pollfd borrowing the owned listener.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 10_000) },
                1,
                "fake producer did not connect"
            );
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let peer_pid = crate::api::socket_peer_pid(&stream).expect("owned peer credentials");
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let (response_tx, response_rx) = mpsc::channel();
            tx.send(crate::api::ApiRequestMessage {
                request: serde_json::from_str(&line).unwrap(),
                respond_to: response_tx,
                peer_pid: Some(peer_pid),
                peer_birth: crate::platform::process_start_time(peer_pid),
            })
            .unwrap();
            let response = response_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            writeln!(stream, "{response}").unwrap();
        }
    });
    runtime
        .try_submit(&serde_json::json!({"type":"user"}))
        .unwrap();
    let mut attributed = true;
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(10), app.api_rx.recv())
            .await
            .expect("bounded socket dispatch")
            .expect("request");
        let root_pid: u32 = std::fs::read_to_string(root.join("model.pid"))
            .unwrap()
            .parse()
            .unwrap();
        attributed &= msg.peer_pid != Some(root_pid);
        let caller = {
            app.current_api_peer_pid = msg.peer_pid;
            app.spawn_caller()
        };
        attributed &= app.attested_sender_agent().as_deref() == Some(AGENT);
        attributed &= app
            .admit_spawn(crate::spawn::SpawnVerb::Fork, &caller, None)
            .ok()
            .flatten()
            .is_some_and(|lineage| lineage.parent_agent_id == AGENT && lineage.child_depth == 1);
        app.handle_api_request_message(msg);
    }
    socket_worker.join().unwrap();
    // The result is emitted only after all tool replies have been written.
    loop {
        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match runtime.try_receive() {
                    Ok(event) => break event,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("producer disconnected")
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("bounded result notification");
        if received.record.result_subtype.is_some() {
            break;
        }
    }
    let exit = owned.finish();
    assert_eq!(exit.outcome, runtime::Outcome::Stopped, "{exit:?}");
    assert!(app.session_leases.reserve(AGENT, SESSION, None).is_ok());
    let responses: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("tool-responses.json")).unwrap()).unwrap();
    let sent = app
        .mailboxes
        .queued_message("lease-call")
        .expect("queued message");
    assert_eq!(
        (
            attributed,
            responses[0]["error"]["code"].as_str(),
            responses[1].get("result").is_some(),
            sent.from_agent.as_deref()
        ),
        (true, Some("operator_only"), true, Some(AGENT)),
        "{responses}"
    );
    assert!(responses[2].get("result").is_some(), "{responses}");
    assert!(sent.from_pane.is_none());
}

#[tokio::test]
async fn pending_or_unknown_structured_identity_never_gets_spawn_or_owner_exemption() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let _lease = app.session_leases.reserve(AGENT, SESSION, None).unwrap();
    app.current_api_peer_pid = None;
    assert!(!app.caller_is_local_operator());
    let caller = app.spawn_caller();
    let error = app
        .admit_spawn(crate::spawn::SpawnVerb::Fork, &caller, None)
        .unwrap_err();
    assert_eq!(error.code(), "spawn_caller_unknown");
    assert!(app.attested_sender_agent().is_none());
}

#[test]
fn pending_children_are_counted_for_structured_lineage() {
    let leases = SessionLeases::default();
    let _root = leases.reserve(AGENT, SESSION, None).unwrap();
    let _child = leases.reserve("child", "fork", Some((AGENT, 2))).unwrap();
    assert_eq!(leases.counts(AGENT).unwrap(), (1, 1));
}

#[tokio::test]
async fn queued_socket_requests_with_unknown_or_reused_birth_never_become_operator() {
    let fixture = runtime::tests::Fixture::new();
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let pid = std::process::id();
    let birth = crate::platform::process_start_time(pid).expect("owned test process");
    for peer_birth in [None, Some(birth.wrapping_add(1))] {
        let (respond_to, response) = mpsc::channel();
        app.handle_api_request_message(crate::api::ApiRequestMessage {
            request: Request {
                id: "stale".into(),
                method: Method::OwnerPolicyAdopt(OwnerPolicyParams {
                    repo: fixture.root.path().display().to_string(),
                    policy_version: None,
                }),
            },
            respond_to,
            peer_pid: Some(pid),
            peer_birth,
        });
        let response: serde_json::Value =
            serde_json::from_str(&response.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "operator_only", "{response}");
    }
    assert!(app.current_api_peer_birth.is_none());
}

#[tokio::test]
async fn recovery_parked_requests_preserve_original_peer_birth() {
    let fixture = runtime::tests::Fixture::new();
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let pid = std::process::id();
    let birth = crate::platform::process_start_time(pid).expect("owned test process");
    let (respond_to, response) = mpsc::channel();
    app.park_until_mesh_recovered(
        Request {
            id: "parked".into(),
            method: Method::OwnerPolicyAdopt(OwnerPolicyParams {
                repo: fixture.root.path().display().to_string(),
                policy_version: None,
            }),
        },
        Some(pid),
        Some(Some(birth.wrapping_add(1))),
        respond_to,
    );
    app.replay_recovery_parked();
    let response: serde_json::Value =
        serde_json::from_str(&response.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
    assert_eq!(response["error"]["code"], "operator_only", "{response}");
    assert!(app.current_api_peer_birth.is_none());
    assert!(app.current_api_peer_pid.is_none());
}
