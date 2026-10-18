use crate::api::schema::*;
use crate::app::App;
use crate::config::Config;
use crate::structured::{lease::SessionLeases, runtime, transport::StreamLocation};
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
    fake_claude_calls(false).await;
}

#[tokio::test]
async fn session_1441_real_mcp_child_is_attested_and_operator_tools_are_refused() {
    fake_claude_calls(true).await;
}

async fn fake_claude_calls(mcp: bool) {
    let fixture = runtime::tests::Fixture::for_socket();
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
    // An explicitly disabled account still cannot grant a leased same-UID agent
    // operator authority. Keep a directory service out of this identity proof.
    app.runtime_config.uid_policy.dedicated = Ok(None).into();
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
                    to: MessageTarget::Pane {
                        pane: target.clone(),
                    },
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
    if mcp {
        fixture.file(
            "mcp-requests.json",
            &serde_json::to_vec(&[
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
                serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"flock_owner_policy_adopt","arguments":{"repo":root}}}),
                serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"flock_delegate_list","arguments":{"architect":"self"}}}),
                serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"flock_msg_send","arguments":{"from_agent":"agent_atlas_spoof",
                "to":{"type":"pane","pane":target},"body":"fixture message",
                "correlation_id":"lease-call","intent":"fyi"}}}),
            ])
            .unwrap(),
            0o600,
        );
    }
    let lease = app.session_leases.reserve(AGENT, SESSION, None).unwrap();
    let profile = fixture.profile(
        if mcp {
            "mcp-lease-calls"
        } else {
            "lease-calls"
        },
        "2.1.296",
    );
    let location = StreamLocation {
        data_dir: root.clone(),
        storage: Default::default(),
        agent: AGENT.into(),
        session: SESSION.into(),
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (exit_tx, exit_rx) = mpsc::channel();
    let request_count = if mcp { 2 } else { 3 };
    let endpoint = if mcp {
        // The ordinary cargo/nextest all-target build provides the sibling CLI.
        // Derive it from this test artifact, never PATH or an installed binary.
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("flk");
        crate::structured::launch::SessionMcp::new(&binary, &socket).unwrap()
    } else {
        fixture.mcp()
    };
    let worker = std::thread::spawn(move || {
        let result = runtime::run(
            &location.data_dir.clone(),
            profile,
            lease,
            location,
            endpoint,
            |runtime| ready_tx.send(runtime).unwrap(),
        );
        let _ = exit_tx.send(result);
    });
    let mut owned = OwnedRuntime {
        runtime: ready_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        worker: Some(worker),
        exit: exit_rx,
    };
    // A separate operator ancestry must remain usable while the model lives.
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(crate::platform::process_start_time(std::process::id()));
    assert!(app.caller_is_local_operator());
    app.current_api_peer_pid = None;
    app.current_api_peer_birth = None;
    let runtime = &owned.runtime;
    let socket_worker = std::thread::spawn(move || {
        for _ in 0..request_count {
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
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            crate::api::server::handle_connection(
                stream,
                &tx,
                &crate::api::EventHub::default(),
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                None,
                &std::sync::Mutex::default(),
                &Default::default(),
            )
            .unwrap();
        }
    });
    runtime
        .try_submit(&serde_json::json!({"type":"user"}))
        .unwrap();
    let mut attributed = true;
    for _ in 0..request_count {
        let msg = tokio::time::timeout(Duration::from_secs(10), app.api_rx.recv())
            .await
            .expect("bounded socket dispatch")
            .expect("request");
        let root_pid: u32 = std::fs::read_to_string(root.join("model.pid"))
            .unwrap()
            .parse()
            .unwrap();
        attributed &= msg.peer_pid != Some(root_pid);
        assert!(
            msg.peer_birth.is_some(),
            "production socket capture must supply birth"
        );
        assert_eq!(
            msg.peer_birth,
            msg.peer_pid.and_then(crate::platform::process_start_time)
        );
        let caller = {
            app.current_api_peer_pid = msg.peer_pid;
            app.current_api_peer_birth = Some(msg.peer_birth);
            assert_eq!(msg.peer_uid, Some(crate::platform::peer::effective_uid()));
            app.current_api_peer_uid = Some(msg.peer_uid);
            assert!(!app.caller_is_local_operator());
            app.spawn_caller()
        };
        attributed &= app.attested_sender_agent().as_deref() == Some(AGENT);
        attributed &= !app.caller_is_local_operator();
        attributed &= app
            .admit_spawn(crate::spawn::SpawnVerb::Fork, &caller, None)
            .ok()
            .flatten()
            .is_some_and(|lineage| lineage.parent_agent_id == AGENT && lineage.child_depth == 1);
        app.handle_api_request_message(msg);
    }
    socket_worker.join().unwrap();
    // The result is emitted only after all tool replies have been written.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .expect("bounded result even if the producer floods unrelated events");
        let received = runtime
            .receive_timeout(remaining)
            .expect("bounded event-driven result notification");
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
    assert!(
        attributed,
        "kernel peer must attest the descendant as the leased agent"
    );
    assert_eq!(sent.from_agent.as_deref(), Some(AGENT));
    if mcp {
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], "flock");
        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "flock_msg_send"));
        assert!(!tools
            .iter()
            .any(|tool| tool["name"] == "flock_owner_policy_adopt"));
        assert_eq!(
            responses[2]["error"]["data"]["refusal"], "not_exposed_via_mcp",
            "{responses}"
        );
        assert_eq!(
            responses[2]["error"]["data"]["exit"],
            crate::cli::exit::USAGE
        );
        assert!(responses[3].get("result").is_some(), "{responses}");
        assert!(responses[4].get("result").is_some(), "{responses}");
        assert!(
            responses[3].get("error").is_none() && responses[4].get("error").is_none(),
            "{responses}"
        );
    } else {
        assert_eq!(
            responses[0]["error"]["code"], "operator_only",
            "{responses}"
        );
        assert!(responses[1].get("result").is_some(), "{responses}");
        assert!(responses[2].get("result").is_some(), "{responses}");
    }
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
async fn queued_socket_requests_with_reused_birth_never_become_operator() {
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
    {
        let peer_birth = Some(birth.wrapping_add(1));
        let (respond_to, response) = mpsc::channel();
        app.handle_api_request_message(crate::api::ApiRequestMessage {
            scope: None,
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
            peer_uid: Some(crate::platform::peer::effective_uid()),
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
        None,
        None,
        respond_to,
    );
    app.replay_recovery_parked();
    let response: serde_json::Value =
        serde_json::from_str(&response.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
    assert_eq!(response["error"]["code"], "operator_only", "{response}");
    assert!(app.current_api_peer_birth.is_none());
    assert!(app.current_api_peer_pid.is_none());
}

#[tokio::test]
async fn unreadable_peer_birth_with_no_leases_preserves_operator_behavior() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(None);
    assert!(app.caller_is_local_operator());
}

#[tokio::test]
async fn real_pane_ancestry_survives_an_unrelated_pending_lease() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("caller")];
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].focused_pane_id().unwrap();
    let terminal = app.state.workspaces[0]
        .pane_state(pane)
        .unwrap()
        .attached_terminal_id
        .clone();
    app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let runtime = crate::terminal::TerminalRuntime::spawn_shell_command(
        pane,
        24,
        80,
        root.path().to_owned(),
        "read line",
        &[],
        4096,
        Default::default(),
        app.event_tx.clone(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .unwrap();
    let pid = runtime.child_pid().unwrap();
    app.current_api_peer_pid = Some(pid);
    app.current_api_peer_birth = Some(crate::platform::process_start_time(pid));
    app.terminal_runtimes.insert(terminal.clone(), runtime);
    let _pending = app.session_leases.reserve(AGENT, SESSION, None).unwrap();
    let caller = app.lease_caller();
    let agent = matches!(app.spawn_caller(), super::spawn::SpawnCaller::Agent { .. });
    let operator = app.caller_is_local_operator();
    let runtime = app.terminal_runtimes.remove(&terminal).unwrap();
    assert!(tokio::time::timeout(
        Duration::from_secs(10),
        runtime.shutdown_for_restart(vec![pid], Duration::ZERO)
    )
    .await
    .unwrap()
    .unwrap());
    assert_eq!(caller, crate::structured::lease::Caller::Unrelated);
    assert!(agent);
    assert!(!operator);
    assert!(app.test_pane_child_pids.is_empty());
}

#[tokio::test]
async fn unreadable_peer_birth_with_active_leases_remains_uncertain() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    let _lease = app.session_leases.reserve(AGENT, SESSION, None).unwrap();
    app.current_api_peer_pid = Some(std::process::id());
    app.current_api_peer_birth = Some(None);
    assert_eq!(
        app.lease_caller(),
        crate::structured::lease::Caller::Uncertain
    );
    assert!(!app.caller_is_local_operator());
}

#[tokio::test]
async fn self_submit_guard_protects_unproven_handoff_panes() {
    for mismatch in [false, true] {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("self-submit")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        crate::test_support::adopt_unproven_pane_caller(&mut app, pane, mismatch);
        assert_eq!(app.caller_pane(), None);
        assert!(!app.caller_is_local_operator());
        let target = app.public_pane_id(0, pane).unwrap();
        let response = app.handle_pane_submit(
            "self".into(),
            PaneSubmitParams {
                pane_id: target,
                text: "owned fixture input".into(),
                if_session: None,
                min_age_secs: 0,
                self_submit_confirmed: Some(false),
            },
        );
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response["error"]["code"], "self_submit_unconfirmed",
            "{response}"
        );
        assert!(app.pending_agent_submit.is_none());
    }
}

#[tokio::test]
async fn self_exclusion_reads_and_self_message_refusals_keep_unproven_candidates() {
    for mismatch in [false, true] {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("self-exclusion")];
        app.state.ensure_test_terminals();
        let pane = app.state.workspaces[0].focused_pane_id().unwrap();
        crate::test_support::adopt_unproven_pane_caller(&mut app, pane, mismatch);
        let terminal = app.state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state.terminals.get_mut(&terminal).unwrap().launch_argv = Some(vec!["claude".into()]);
        assert_eq!(app.caller_pane(), None);
        assert!(app.attested_sender_agent().is_none());
        let response: serde_json::Value = serde_json::from_str(&app.handle_agent_get(
            "self".into(),
            AgentTarget {
                target: "self".into(),
            },
        ))
        .unwrap();
        assert!(
            response.get("result").is_some(),
            "bulk self-exclusion keeps its candidate: {response}"
        );
        let target = app.public_pane_id(0, pane).unwrap();
        let response: serde_json::Value = serde_json::from_str(&app.send_message(
            "send".into(),
            MsgSendParams {
                to: MessageTarget::Pane { pane: target },
                body: "fixture message".into(),
                from_agent: None,
                correlation_id: None,
                in_reply_to: None,
                intent: MsgIntent::Fyi,
            },
        ))
        .unwrap();
        assert_eq!(
            response["error"]["code"], "self_message_forbidden",
            "{response}"
        );
    }
}

#[tokio::test]
async fn scoped_queued_and_parked_requests_refuse_after_release() {
    use crate::api::scoped::tests::live_scope;
    for parked in [false, true] {
        let (leases, lease, scope, peer) = live_scope();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.session_leases = leases;
        app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
            operator: peer.uid.unwrap(),
            dedicated: Ok(None).into(),
        };
        let request = crate::api::schema::Request {
            id: "queued-scope".into(),
            method: crate::api::schema::Method::ServerStop(
                crate::api::schema::EmptyParams::default(),
            ),
        };
        let (respond_to, response) = std::sync::mpsc::channel();
        if parked {
            app.park_until_mesh_recovered(
                request,
                peer.pid,
                Some(peer.birth),
                Some(peer.uid),
                Some(scope),
                respond_to,
            );
            drop(lease);
            app.replay_recovery_parked();
        } else {
            drop(lease);
            app.handle_api_request_message(crate::api::ApiRequestMessage {
                request,
                respond_to,
                peer_pid: peer.pid,
                peer_birth: peer.birth,
                peer_uid: peer.uid,
                scope: Some(scope),
            });
        }
        let result: serde_json::Value = serde_json::from_str(
            &response
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["error"]["code"], "not_authorized");
        assert!(!app.state.should_quit);
        assert!(app.current_api_scope.is_none());
    }
}

#[tokio::test]
async fn scoped_live_agent_does_not_become_operator_and_legacy_uid_policy_is_unchanged() {
    use crate::api::scoped::tests::live_scope;
    for (scoped, dedicated) in [(true, false), (false, false), (false, true)] {
        let (leases, _lease, scope, peer) = live_scope();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        if scoped {
            app.session_leases = leases;
        }
        app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
            operator: if dedicated {
                peer.uid.unwrap().wrapping_add(1)
            } else {
                peer.uid.unwrap()
            },
            dedicated: Ok(if dedicated { peer.uid } else { None }).into(),
        };
        let (respond_to, response) = std::sync::mpsc::channel();
        app.handle_api_request_message(crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "boundary".into(),
                method: crate::api::schema::Method::ServerStop(
                    crate::api::schema::EmptyParams::default(),
                ),
            },
            respond_to,
            peer_pid: peer.pid,
            peer_birth: peer.birth,
            peer_uid: peer.uid,
            scope: if scoped { Some(scope) } else { None },
        });
        let reply: serde_json::Value = serde_json::from_str(
            &response
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(app.state.should_quit, !scoped && !dedicated, "{reply}");
        if scoped || dedicated {
            assert_eq!(reply["error"]["code"], "operator_only");
        }
    }
}
