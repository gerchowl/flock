//! Commands enter through real authenticated sockets and the real structured pump.
use super::*;
use crate::structured::workers::tests::global::{max_policy, AppQueue};
use crate::structured_wire::fixture::{FixtureCoverage, FixtureLine, FixtureStep};

struct Case {
    server: Server,
    queue: AppQueue,
    next: u64,
}
impl Case {
    async fn new(n: u32) -> Self {
        let queue = AppQueue::new(n, |_| max_policy()).await;
        let mut case = Self {
            server: Server::new().await,
            queue,
            next: 0,
        };
        case.queue
            .until(|r| {
                matches!(
                    r.app.queue_observation(&r.case.id),
                    Some(crate::app::queue_events::Observation::Status(None, None))
                )
            })
            .await;
        case
    }
    async fn open(&self) -> Socket {
        let mut socket = Socket::open(&self.server).await;
        socket.hello(1, &"a".repeat(64)).await;
        socket
    }
    fn service(&mut self) {
        self.queue.app.service_structured_workers(Instant::now());
        let core = Arc::get_mut(&mut self.server.core).unwrap();
        core.service(
            &self.queue.app.state.structured_agents,
            &mut self.queue.app.structured_workers,
            self.server._root.path(),
        );
        core.service_commands(&mut self.queue.app);
    }
    async fn frame(&mut self, socket: &mut Socket) -> Received {
        let owner = self.server.wake.clone();
        let worker = self.queue.notify.clone();
        within(async {
            loop {
                let changed = owner.notified();
                let produced = worker.notified();
                tokio::pin!(changed, produced);
                changed.as_mut().enable();
                produced.as_mut().enable();
                self.service();
                tokio::select! { biased;
                    frame = socket.receive() => return frame,
                    _ = &mut changed => (), _ = &mut produced => (),
                }
            }
        })
        .await
    }
    async fn ack(&mut self, socket: &mut Socket) -> Value {
        match self.frame(socket).await {
            Received::Text(v) => {
                assert_eq!(v["t"], "ack");
                v
            }
            other => panic!("expected command ack, got {other:?}"),
        }
    }
    async fn command(&mut self, socket: &mut Socket, verb: &str, args: Value) -> Value {
        self.next += 1;
        socket.send(json!({"t":"cmd","cmd_id":format!("command-{}",self.next),"verb":verb,"args":args,"resume_only":false})).await;
        self.ack(socket).await
    }
    fn journal(&self) -> Vec<u8> {
        fs::read(
            self.queue
                .case
                .fixture
                .root
                .path()
                .join("sessions")
                .join(&self.queue.case.id)
                .join("queue.jsonl"),
        )
        .unwrap()
    }
    fn events(&self) -> Vec<Value> {
        fs::read_to_string(
            self.queue
                .case
                .fixture
                .root
                .path()
                .join("sessions")
                .join(&self.queue.case.id)
                .join(format!(
                    "{}.jsonl",
                    self.queue
                        .app
                        .state
                        .structured_agents
                        .get(&self.queue.case.id)
                        .unwrap()
                        .0
                        .session_id
                )),
        )
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            serde_json::from_str(v["raw"].as_str().unwrap()).unwrap()
        })
        .collect()
    }
    fn scopes(&self, scopes: Value) {
        let path = self.server._root.path().join("client-tokens/one.token");
        let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        record["scopes"] = scopes;
        fs::write(path, record.to_string()).unwrap();
    }
    async fn stop(mut self) {
        self.queue.stop().await;
        self.server.finish().await;
    }
}
fn script(name: &str) -> Vec<Value> {
    let text = match name {
        "agent-send" => include_str!("../../../../web/client/test/fixtures/wire/agent-send.jsonl"),
        "token-revoked" => {
            include_str!("../../../../web/client/test/fixtures/wire/token-revoked.jsonl")
        }
        _ => panic!("unknown script"),
    };
    let typed: Vec<FixtureLine> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        matches!(&typed[0].step, FixtureStep::Given { covers, .. } if covers.contains(&FixtureCoverage::S7Min))
    );
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn send_script(name: &str, target: &str) -> Value {
    let mut command = script(name)
        .into_iter()
        .find(|v| v["step"] == "expect" && v["frame"]["t"] == "cmd")
        .unwrap()["frame"]
        .clone();
    command["args"]["target"] = json!(target);
    command
}
#[test]
fn agent_send_fixture_ack_matches_one_durable_user_turn() {
    runtime(async {
        let mut case = Case::new(183101).await;
        let mut socket = case.open().await;
        socket
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        let ack = case.ack(&mut socket).await;
        assert_eq!(ack["ok"], true);
        assert_eq!(ack["cmd_id"], "c1");
        let item = ack["result"]["item_id"].as_str().unwrap().to_owned();
        case.queue
            .until(|r| {
                let path = r
                    .case
                    .fixture
                    .root
                    .path()
                    .join("sessions")
                    .join(&r.case.id)
                    .join(format!(
                        "{}.jsonl",
                        r.app
                            .state
                            .structured_agents
                            .get(&r.case.id)
                            .unwrap()
                            .0
                            .session_id
                    ));
                fs::read_to_string(path).unwrap().contains("fixture_input")
            })
            .await;
        socket
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        let refusal = case.ack(&mut socket).await;
        assert_eq!(refusal["error"]["code"], "session_unconfirmed");
        assert_eq!(refusal["error"]["exit"], crate::cli::exit::UNCONFIRMED);
        assert_eq!(refusal["error"]["retry"], "later");
        let inputs: Vec<_> = case
            .events()
            .into_iter()
            .filter(|v| v["type"] == "fixture_input")
            .collect();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["input"]["uuid"], item);
        let text = inputs[0]["input"]["message"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let (header, framed) = text.split_once("\n<<<flk-item ").unwrap();
        assert_eq!(
            header,
            format!("From \"human\" (item {item}): Attachments (blob refs): []")
        );
        let (nonce, body) = framed.split_once('\n').unwrap();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(body, format!("fixture message\nflk-item {nonce}>>>"));
        assert!(String::from_utf8(case.journal()).unwrap().contains(&item));
        case.stop().await;
    });
}
#[test]
fn draft_unknown_remote_verb_and_resume_only_refuse_before_mutation() {
    runtime(async {
        let mut case = Case::new(183102).await;
        let remote = "00000000-0000-4000-8000-000000183110";
        let mut peer = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: "remote.example".into(),
            ..Default::default()
        });
        peer.node_id = Some("a".repeat(64));
        peer.workspaces = vec![serde_json::from_value(json!({
            "workspace":"remote", "is_linked_worktree":false, "status":crate::api::schema::AgentStatus::Idle,
            "agents":[{"agent_id":remote,"pane_id":"w1:p1","status":crate::api::schema::AgentStatus::Idle}]
        }))
        .unwrap()];
        case.queue.app.state.peer_summaries.push(peer);
        let location = case.queue.app.locate_agent(remote).unwrap();
        assert!(!location.local);
        assert_eq!(location.node.as_deref(), Some("a".repeat(64).as_str()));
        let mut socket = case.open().await;
        let before = case.journal();
        for (verb, args, code) in [
            (
                "agent.send",
                json!({"target":case.queue.case.id,"text":"draft","submit":false}),
                "structured_submit_required",
            ),
            (
                "agent.send",
                json!({"target":remote,"text":"remote","submit":true}),
                "unsupported_for_agent",
            ),
            (
                "agent.send",
                json!({"target":"unknown-local","text":"missing","submit":true}),
                "agent_not_found",
            ),
            (
                "agent.interrupt",
                json!({"agent":case.queue.case.id}),
                "server_version_gap",
            ),
        ] {
            assert_eq!(
                case.command(&mut socket, verb, args).await["error"]["code"],
                code
            );
            assert_eq!(case.journal(), before);
        }
        let mut command = send_script("agent-send", &case.queue.case.id);
        command["resume_only"] = json!(true);
        socket.send(command).await;
        let refusal = case.ack(&mut socket).await;
        assert_eq!(refusal["error"]["code"], "session_unconfirmed");
        assert_eq!(refusal["error"]["exit"], crate::cli::exit::UNCONFIRMED);
        assert_eq!(refusal["error"]["retry"], "later");
        assert_eq!(case.journal(), before);
        assert!(!case.events().iter().any(|v| v["type"] == "fixture_input"));
        case.stop().await;
    });
}
#[test]
fn token_revoked_fixture_ack_flushes_before_both_token_sockets_close() {
    runtime(async {
        let mut case = Case::new(183103).await;
        let mut first = case.open().await;
        let mut second = case.open().await;
        let before = case.journal();
        fs::remove_file(case.server._root.path().join("client-tokens/one.token")).unwrap();
        first
            .send(send_script("token-revoked", &case.queue.case.id))
            .await;
        let ack = case.ack(&mut first).await;
        let expected = script("token-revoked")
            .into_iter()
            .find(|v| v["step"] == "send" && v["frame"]["t"] == "ack")
            .unwrap()["frame"]["error"]
            .clone();
        for field in ["code", "exit", "retry"] {
            assert_eq!(ack["error"][field], expected[field]);
        }
        assert!(matches!(
            case.frame(&mut first).await,
            Received::Close(4403)
        ));
        assert!(matches!(
            case.frame(&mut second).await,
            Received::Close(4403)
        ));
        assert_eq!(case.journal(), before);
        case.stop().await;
    });
}
#[test]
fn scope_shrink_refuses_send_and_preserves_allowed_agent_reads() {
    runtime(async {
        let mut case = Case::new(183104).await;
        let mut socket = case.open().await;
        let before = case.journal();
        case.scopes(json!(["read"]));
        socket
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        assert_eq!(case.ack(&mut socket).await["error"]["code"], "scope");
        assert_eq!(case.journal(), before);
        socket.send(json!({"t":"sub","sub_id":"still-readable","stream":format!("agent/{}",case.queue.case.id)})).await;
        loop {
            match case.frame(&mut socket).await {
                Received::Text(v) if v["t"] == "ev" => (),
                Received::Text(v) if v["t"] == "synced" => break,
                other => panic!("read scope lost: {other:?}"),
            }
        }
        socket
            .send(json!({"t":"unsub","sub_id":"still-readable"}))
            .await;
        case.scopes(json!(["read", "send", "answer", "operate"]));
        socket
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        assert_eq!(
            case.ack(&mut socket).await["ok"],
            true,
            "scope refusal must not claim the command ID"
        );
        case.stop().await;
    });
}

async fn enqueue_without_owner(server: &Server, socket: &mut Socket, id: String, target: &str) {
    let admitted = server.wake.notified();
    tokio::pin!(admitted);
    admitted.as_mut().enable();
    socket.send(json!({"t":"cmd","cmd_id":id,"verb":"agent.send","args":{"target":target,"text":"queued","submit":false},"resume_only":false})).await;
    within(async {
        tokio::select! {
            _ = &mut admitted => (),
            frame = socket.receive() => panic!("queue producer ended before admission: {frame:?}"),
        }
    })
    .await;
}
fn refusal(frame: Value, code: &str, exit: i32) {
    assert_eq!(frame["t"], "ack");
    assert_eq!(frame["ok"], false);
    assert_eq!(frame["error"]["code"], code);
    assert_eq!(frame["error"]["exit"], exit);
}
#[test]
fn unknown_verbs_and_oversized_ids_refuse_without_owner_or_read_scope() {
    runtime(async {
        let case = Case::new(183111).await;
        let mut socket = case.open().await;
        case.scopes(json!(["send", "answer", "operate"]));
        for (id, verb, code, exit) in [
            (
                "future".to_owned(),
                "future.command",
                "server_version_gap",
                crate::cli::exit::PROTOCOL_GAP,
            ),
            (
                "x".repeat(129),
                "agent.send",
                "invalid_params",
                crate::cli::exit::FAILURE,
            ),
        ] {
            socket
                .send(json!({"t":"cmd","cmd_id":id,"verb":verb,"args":{},"resume_only":false}))
                .await;
            let frame = socket.text().await;
            assert_eq!(frame["cmd_id"], id);
            refusal(frame, code, exit);
        }
        case.stop().await;
    });
}
#[test]
fn fifth_concurrent_command_is_busy_and_keeps_the_socket_open() {
    runtime(async {
        let case = Case::new(183112).await;
        let mut socket = case.open().await;
        for n in 0..4 {
            enqueue_without_owner(
                &case.server,
                &mut socket,
                format!("pending-{n}"),
                &case.queue.case.id,
            )
            .await;
        }
        for n in 4..6 {
            socket.send(json!({"t":"cmd","cmd_id":format!("excess-{n}"),"verb":"agent.send","args":{},"resume_only":false})).await;
            refusal(
                socket.text().await,
                "session_busy",
                crate::cli::exit::FAILURE,
            );
        }
        case.stop().await;
    });
}
#[test]
fn full_owner_queue_refuses_other_principals_without_closing() {
    runtime(async {
        let mut case = Case::new(183113).await;
        let mut producers = Vec::new();
        // One principal is already limited to four sockets times four commands.
        // Two principals fill all 32 owner slots through real socket producers.
        for token in ["a", "d"] {
            for connection in 0..4 {
                let mut socket = Socket::open(&case.server).await;
                socket.hello(1, &token.repeat(64)).await;
                for n in 0..4 {
                    enqueue_without_owner(
                        &case.server,
                        &mut socket,
                        format!("{connection}-{n}"),
                        &case.queue.case.id,
                    )
                    .await;
                }
                producers.push(socket);
            }
        }
        let mut socket = Socket::open(&case.server).await;
        socket.hello(1, &"b".repeat(64)).await;
        socket.send(json!({"t":"cmd","cmd_id":"overflow","verb":"agent.send","args":{},"resume_only":false})).await;
        refusal(
            socket.text().await,
            "session_busy",
            crate::cli::exit::FAILURE,
        );
        // Queued drafts settle as refusals; the same healthy socket can then send.
        case.service();
        let args = json!({"target":case.queue.case.id,"text":"after-pressure","submit":true});
        assert_eq!(
            case.command(&mut socket, "agent.send", args).await["ok"],
            true
        );
        drop(producers);
        case.stop().await;
    });
}
#[test]
fn same_principal_command_id_is_fenced_across_sockets() {
    runtime(async {
        let mut case = Case::new(183114).await;
        let mut first = case.open().await;
        let mut second = case.open().await;
        first
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        assert_eq!(case.ack(&mut first).await["ok"], true);
        second
            .send(send_script("agent-send", &case.queue.case.id))
            .await;
        refusal(
            case.ack(&mut second).await,
            "session_unconfirmed",
            crate::cli::exit::UNCONFIRMED,
        );
        let journal = String::from_utf8(case.journal()).unwrap();
        let identities: std::collections::HashSet<String> = journal
            .lines()
            .filter_map(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                row["item"]["item_id"].as_str().map(str::to_owned)
            })
            .collect();
        assert_eq!(identities.len(), 1);
        case.stop().await;
    });
}

#[test]
fn oversized_command_id_is_a_typed_refusal_without_owner_admission() {
    runtime(async {
        let case = Case::new(183115).await;
        let mut socket = case.open().await;
        case.scopes(json!(["send", "answer", "operate"]));
        let id = "x".repeat(129);
        socket
            .send(json!({"t":"cmd","cmd_id":id,"verb":"agent.send","args":{},"resume_only":false}))
            .await;
        let frame = socket.text().await;
        assert_eq!(frame["cmd_id"], id);
        refusal(frame, "invalid_params", crate::cli::exit::FAILURE);
        case.stop().await;
    });
}

#[test]
fn structured_name_routes_to_queue_without_a_terminal() {
    runtime(async {
        let mut case = Case::new(183117).await;
        let name = case
            .queue
            .app
            .state
            .structured_agents
            .get(&case.queue.case.id)
            .unwrap()
            .0
            .name
            .clone()
            .unwrap();
        let mut socket = case.open().await;
        let before = case.journal();
        let result = case
            .command(
                &mut socket,
                "agent.send",
                json!({"target":name,"text":"named send","submit":true}),
            )
            .await;
        assert_eq!(result["ok"], true);
        let item = result["result"]["item_id"].as_str().unwrap();
        assert_ne!(case.journal(), before);
        assert!(String::from_utf8(case.journal()).unwrap().contains(item));
        case.stop().await;
    });
}

#[test]
fn cross_runtime_id_and_name_collisions_refuse_before_queue_mutation() {
    runtime(async {
        for exact in [false, true] {
            let mut case = Case::new(if exact { 183118 } else { 183119 }).await;
            let target = if exact {
                case.queue.case.id.clone()
            } else {
                case.queue
                    .app
                    .state
                    .structured_agents
                    .get(&case.queue.case.id)
                    .unwrap()
                    .0
                    .name
                    .clone()
                    .unwrap()
            };
            case.queue.app.state.workspaces =
                vec![crate::workspace::Workspace::test_new("send-wire-collision")];
            case.queue.app.state.ensure_test_terminals();
            let pane = case.queue.app.state.workspaces[0]
                .focused_pane_id()
                .unwrap();
            let terminal = case.queue.app.state.workspaces[0]
                .pane_state(pane)
                .unwrap()
                .attached_terminal_id
                .clone();
            case.queue
                .app
                .state
                .terminals
                .get_mut(&terminal)
                .unwrap()
                .set_agent_name(target.clone())
                .unwrap();
            let before = case.journal();
            let mut socket = case.open().await;
            let result = case
                .command(
                    &mut socket,
                    "agent.send",
                    json!({"target":target,"text":"collision","submit":true}),
                )
                .await;
            assert_eq!(result["error"]["code"], "agent_target_ambiguous");
            assert_eq!(case.journal(), before);
            assert!(!case.events().iter().any(|v| v["type"] == "fixture_input"));
            case.stop().await;
        }
    });
}

#[path = "decisions.rs"]
mod decisions;

#[test]
fn send_and_decide_both_ack_on_the_same_socket() {
    runtime(async {
        let mut case = Case::new(183120).await;
        let mut socket = case.open().await;
        let send = case
            .command(
                &mut socket,
                "agent.send",
                json!({"target":case.queue.case.id,"text":"one socket turn","submit":true}),
            )
            .await;
        assert_eq!(send["ok"], true);
        assert_eq!(send["cmd_id"], "command-1");
        assert!(send["result"]["item_id"].as_str().is_some());
        let mut request = crate::structured::workers::tests::event("control_request");
        request["request_id"] = json!("union-request");
        case.queue.control(json!({"op":"event","event":request}));
        case.queue
            .until(|r| {
                r.app
                    .structured_workers
                    .runtime(&r.case.id, r.case.generation)
                    .unwrap()
                    .waiting_on_human()
                    .unwrap()
                    .iter()
                    .any(|(id, _)| id == "union-request")
            })
            .await;
        let seq = case
            .queue
            .app
            .structured_workers
            .runtime(&case.queue.case.id, case.queue.case.generation)
            .unwrap()
            .waiting_on_human()
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == "union-request")
            .unwrap()
            .1;
        let decision = case.command(&mut socket, "agent.decide", json!({"agent":case.queue.case.id,"request_id":"union-request","request_seq":seq,"decision":"deny"})).await;
        assert_eq!(decision["ok"], true);
        assert_eq!(decision["cmd_id"], "command-2");
        assert_eq!(
            decision["result"],
            json!({"request_id":"union-request","decision":"deny"})
        );
        let session = &case
            .queue
            .app
            .state
            .structured_agents
            .get(&case.queue.case.id)
            .unwrap()
            .0
            .session_id;
        let path = case
            .queue
            .case
            .fixture
            .root
            .path()
            .join("sessions")
            .join(&case.queue.case.id)
            .join(format!("{session}.jsonl"));
        case.queue
            .until(|_| {
                fs::read_to_string(&path)
                    .unwrap()
                    .contains("fixture_control_input")
            })
            .await;
        let events = case.events();
        assert_eq!(
            events
                .iter()
                .filter(|v| v["type"] == "fixture_input")
                .count(),
            1
        );
        let inputs: Vec<_> = events
            .iter()
            .filter(|v| v["type"] == "fixture_control_input")
            .collect();
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            inputs[0]["input"]["response"]["request_id"],
            "union-request"
        );
        case.stop().await;
    });
}

#[test]
fn send_scope_without_operate_is_not_authorized() {
    runtime(async {
        let mut case = Case::new(183121).await;
        let mut socket = case.open().await;
        case.scopes(json!(["read", "send"]));
        let before = case.journal();
        let response = case
            .command(
                &mut socket,
                "agent.send",
                json!({"target":case.queue.case.id,"text":"not an operator","submit":true}),
            )
            .await;
        refusal(
            response,
            "not_authorized",
            crate::api::error_code::ErrorCode::NotAuthorized.exit(),
        );
        assert_eq!(case.journal(), before);
        assert!(!case.events().iter().any(|v| v["type"] == "fixture_input"));
        case.stop().await;
    });
}

#[test]
fn local_pty_only_target_is_unsupported_for_agent() {
    runtime(async {
        let mut case = Case::new(183122).await;
        case.queue.app.state.workspaces =
            vec![crate::workspace::Workspace::test_new("send-pty-only")];
        case.queue.app.state.ensure_test_terminals();
        let pane = case.queue.app.state.workspaces[0]
            .focused_pane_id()
            .unwrap();
        let terminal = case.queue.app.state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        let state = case.queue.app.state.terminals.get_mut(&terminal).unwrap();
        state.launch_argv = Some(vec!["claude".into()]);
        state.set_agent_name("pty-only".into()).unwrap();
        let target = state.agent_id.to_string();
        assert!(case.queue.app.locate_agent(&target).unwrap().local);
        assert!(case
            .queue
            .app
            .state
            .structured_agents
            .get(&target)
            .is_none());
        let before = case.journal();
        let mut socket = case.open().await;
        let response = case
            .command(
                &mut socket,
                "agent.send",
                json!({"target":target,"text":"no queue for a pty","submit":true}),
            )
            .await;
        refusal(
            response,
            "unsupported_for_agent",
            crate::api::error_code::ErrorCode::UnsupportedForAgent.exit(),
        );
        assert_eq!(case.journal(), before);
        assert!(!case.events().iter().any(|v| v["type"] == "fixture_input"));
        case.stop().await;
    });
}
