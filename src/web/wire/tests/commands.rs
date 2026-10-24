//! Decide-only port of the approved command tests: real authenticated sockets and harness stdin.
use super::*;
use crate::structured::{workers::tests::Case as Worker, workers::Workers};
use tokio::io::AsyncWriteExt;

fn script(name: &str) -> Vec<Value> {
    use crate::structured_wire::fixture::{FixtureCoverage, FixtureLine, FixtureStep};
    let text = match name {
        "permission-decision" => {
            include_str!("../../../../web/client/test/fixtures/wire/permission-decision.jsonl")
        }
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

struct Case {
    server: Server,
    app: crate::app::App,
    worker: Worker,
    notify: Arc<tokio::sync::Notify>,
    driver: tokio::net::UnixStream,
    next: u64,
}
impl Case {
    async fn new(n: u32) -> Self {
        let notify = Arc::new(tokio::sync::Notify::new());
        let (_, rx) = mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.structured_workers = Workers::new(notify.clone());
        let (mut worker, launch, factory) =
            Worker::prepare(&mut app.state.structured_agents, n, "queue", vec![]);
        let listener =
            tokio::net::UnixListener::bind(worker.fixture.root.path().join("queue.sock")).unwrap();
        worker.generation = app
            .structured_workers
            .start(&mut app.state.structured_agents, launch, factory)
            .unwrap();
        let (driver, _) = within(listener.accept()).await.unwrap();
        worker
            .ready(
                &mut app.state.structured_agents,
                &mut app.structured_workers,
                &notify,
                Instant::now(),
            )
            .await;
        Self {
            server: Server::new().await,
            app,
            worker,
            notify,
            driver,
            next: 0,
        }
    }
    async fn open(&self) -> Socket {
        let mut socket = Socket::open(&self.server).await;
        socket.hello(1, &"a".repeat(64)).await;
        socket
    }
    fn service(&mut self) {
        self.app.service_structured_workers(Instant::now());
        let core = Arc::get_mut(&mut self.server.core).unwrap();
        core.service(
            &self.app.state.structured_agents,
            &mut self.app.structured_workers,
            self.worker.fixture.root.path(),
        );
        core.service_commands(&mut self.app);
    }
    async fn frame(&mut self, socket: &mut Socket) -> Received {
        let owner = self.server.wake.clone();
        let worker = self.notify.clone();
        within(async {
            loop {
                let changed = owner.notified();
                let produced = worker.notified();
                tokio::pin!(changed, produced);
                changed.as_mut().enable();
                produced.as_mut().enable();
                self.service();
                assert!(
                    self.app
                        .structured_workers
                        .report(&self.worker.id, self.worker.generation)
                        .is_none(),
                    "harness exited before frame"
                );
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
    fn events(&self) -> Vec<Value> {
        let record = self
            .app
            .state
            .structured_agents
            .get(&self.worker.id)
            .unwrap()
            .0;
        fs::read_to_string(
            self.worker
                .fixture
                .root
                .path()
                .join("sessions")
                .join(&self.worker.id)
                .join(format!("{}.jsonl", record.session_id)),
        )
        .unwrap()
        .lines()
        .map(|line| {
            let v: Value = serde_json::from_str(line).unwrap();
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
    fn handles(&self) -> Vec<(String, u64)> {
        // Read the owner snapshot populated by real permission records, not the
        // live book whose reader/writer may still be publishing another prompt.
        self.app
            .state
            .structured_agents
            .get(&self.worker.id)
            .unwrap()
            .1
            .update
            .permission_requests
            .clone()
    }
    async fn until(&mut self, mut condition: impl FnMut(&Self) -> bool) {
        let notify = self.notify.clone();
        crate::structured::workers::tests::wait(&notify, || {
            self.service();
            assert!(
                self.app
                    .structured_workers
                    .report(&self.worker.id, self.worker.generation)
                    .is_none(),
                "harness exited before condition"
            );
            condition(self)
        })
        .await;
    }
    async fn prompt(&mut self) -> Value {
        for id in ["first-request", "other-request"] {
            let mut request = crate::structured::workers::tests::event("control_request");
            request["request_id"] = json!(id);
            within(
                self.driver
                    .write_all(format!("{}\n", json!({"op":"event","event":request})).as_bytes()),
            )
            .await
            .unwrap();
            // The Python driver selects its fd before buffered readline. Send
            // the next stage only after this record reached the owner, so a
            // second line cannot hide in its userspace buffer behind select.
            self.until(|r| r.handles().iter().any(|(request, _)| request == id))
                .await;
        }
        self.until(|r| r.handles().len() == 2).await;
        let seq = self
            .handles()
            .into_iter()
            .find(|(id, _)| id == "first-request")
            .unwrap()
            .1;
        let mut args = script("permission-decision")
            .into_iter()
            .find(|v| v["step"] == "expect" && v["frame"]["t"] == "cmd")
            .unwrap()["frame"]["args"]
            .clone();
        args["agent"] = json!(self.worker.id);
        args["request_id"] = json!("first-request");
        args["request_seq"] = json!(seq);
        args
    }
    async fn stop(mut self) {
        self.worker
            .finish(
                &mut self.app.state.structured_agents,
                &mut self.app.structured_workers,
                &self.notify,
                Instant::now(),
                None,
            )
            .await;
        self.server.finish().await;
    }
}
fn refusal(frame: Value, code: &str, exit: i32) {
    assert_eq!(frame["t"], "ack");
    assert_eq!(frame["ok"], false);
    assert_eq!(frame["error"]["code"], code);
    assert_eq!(frame["error"]["exit"], exit);
}
#[test]
fn permission_decisions_use_the_named_request_handle_and_typed_refusals() {
    runtime(async {
        let mut case = Case::new(183105).await;
        let mut socket = case.open().await;
        let args = case.prompt().await;
        case.scopes(json!(["answer", "read"]));
        assert_eq!(
            case.command(&mut socket, "agent.decide", args.clone())
                .await["error"]["code"],
            "not_authorized"
        );
        case.scopes(json!(["answer", "read", "operate"]));
        for (field, value) in [
            ("request_id", json!("missing-request")),
            (
                "request_seq",
                json!(args["request_seq"].as_u64().unwrap() + 100),
            ),
        ] {
            let mut unknown = args.clone();
            unknown[field] = value;
            assert_eq!(
                case.command(&mut socket, "agent.decide", unknown).await["error"]["code"],
                "permission_request_unknown"
            );
        }
        let ack = case
            .command(&mut socket, "agent.decide", args.clone())
            .await;
        assert_eq!(
            ack["result"],
            json!({"request_id":"first-request","decision":"deny"})
        );
        case.until(|r| r.handles().len() == 1).await;
        assert_eq!(case.handles()[0].0, "other-request");
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["error"]["code"],
            "permission_request_closed"
        );
        case.until(|r| {
            r.events()
                .iter()
                .any(|v| v["type"] == "fixture_control_input")
        })
        .await;
        let inputs: Vec<_> = case
            .events()
            .into_iter()
            .filter(|v| v["type"] == "fixture_control_input")
            .collect();
        assert_eq!(inputs.len(), 1);
        assert_eq!(
            inputs[0]["input"]["response"]["request_id"],
            "first-request"
        );
        case.stop().await;
    });
}
#[test]
fn token_revoked_ack_flushes_before_both_token_sockets_close() {
    runtime(async {
        let mut case = Case::new(183103).await;
        let args = case.prompt().await;
        let mut first = case.open().await;
        let mut second = case.open().await;
        fs::remove_file(case.server._root.path().join("client-tokens/one.token")).unwrap();
        let ack = case.command(&mut first, "agent.decide", args).await;
        let expected = script("token-revoked")
            .into_iter()
            .find(|v| v["step"] == "send" && v["frame"]["t"] == "ack")
            .unwrap()["frame"]["error"]
            .clone();
        for field in ["code", "exit", "retry"] {
            assert_eq!(ack["error"][field], expected[field]);
        }
        refusal(
            ack,
            "auth_revoked",
            crate::api::error_code::ErrorCode::AuthRevoked.exit(),
        );
        assert!(matches!(
            case.frame(&mut first).await,
            Received::Close(4403)
        ));
        assert!(matches!(
            case.frame(&mut second).await,
            Received::Close(4403)
        ));
        assert_eq!(case.handles().len(), 2);
        case.stop().await;
    });
}
#[test]
fn scope_shrink_refuses_decide_and_preserves_allowed_agent_reads() {
    runtime(async {
        let mut case = Case::new(183104).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        case.scopes(json!(["read"]));
        assert_eq!(
            case.command(&mut socket, "agent.decide", args.clone())
                .await["error"]["code"],
            "scope"
        );
        assert_eq!(case.handles().len(), 2);
        socket.send(json!({"t":"sub","sub_id":"still-readable","stream":format!("agent/{}",case.worker.id)})).await;
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
        case.scopes(json!(["read", "answer", "operate"]));
        socket.send(json!({"t":"cmd","cmd_id":"command-1","verb":"agent.decide","args":args,"resume_only":false})).await;
        assert_eq!(
            case.ack(&mut socket).await["ok"],
            true,
            "scope refusal must not claim ID"
        );
        case.stop().await;
    });
}
#[test]
fn same_principal_command_id_is_fenced_across_sockets() {
    runtime(async {
        let mut case = Case::new(183114).await;
        let args = case.prompt().await;
        let mut first = case.open().await;
        let mut second = case.open().await;
        first.send(json!({"t":"cmd","cmd_id":"same-id","verb":"agent.decide","args":args,"resume_only":false})).await;
        assert_eq!(case.ack(&mut first).await["ok"], true);
        let mut other = args.clone();
        other["request_id"] = json!("other-request");
        other["request_seq"] = json!(
            case.handles()
                .iter()
                .find(|(id, _)| id == "other-request")
                .unwrap()
                .1
        );
        second.send(json!({"t":"cmd","cmd_id":"same-id","verb":"agent.decide","args":other,"resume_only":false})).await;
        refusal(
            case.ack(&mut second).await,
            "session_unconfirmed",
            crate::cli::exit::UNCONFIRMED,
        );
        assert_eq!(
            case.handles()
                .iter()
                .filter(|(id, _)| id == "other-request")
                .count(),
            1
        );
        case.stop().await;
    });
}
#[test]
fn decide_unknown_remote_and_resume_only_refuse_before_mutation() {
    runtime(async {
        let mut case = Case::new(183102).await;
        let args = case.prompt().await;
        let remote = "00000000-0000-4000-8000-000000183110";
        let mut peer = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: "remote.example".into(),
            ..Default::default()
        });
        peer.node_id = Some("a".repeat(64));
        peer.workspaces=vec![serde_json::from_value(json!({"workspace":"remote","is_linked_worktree":false,"status":"idle","agents":[{"agent_id":remote,"pane_id":"w1:p1","status":"idle"}]})).unwrap()];
        case.app.state.peer_summaries.push(peer);
        assert!(!case.app.locate_agent(remote).unwrap().local);
        let mut socket = case.open().await;
        for (target, code) in [
            ("unknown-local", "agent_not_found"),
            (remote, "unsupported_for_agent"),
        ] {
            let mut request = args.clone();
            request["agent"] = json!(target);
            assert_eq!(
                case.command(&mut socket, "agent.decide", request).await["error"]["code"],
                code
            );
        }
        socket.send(json!({"t":"cmd","cmd_id":"resume","verb":"agent.decide","args":args,"resume_only":true})).await;
        refusal(
            case.ack(&mut socket).await,
            "session_unconfirmed",
            crate::cli::exit::UNCONFIRMED,
        );
        assert_eq!(case.handles().len(), 2);
        assert!(!case
            .events()
            .iter()
            .any(|v| v["type"] == "fixture_control_input"));
        case.stop().await;
    });
}
#[test]
fn unknown_verbs_and_oversized_ids_refuse_without_owner_or_read_scope() {
    runtime(async {
        let case = Case::new(183111).await;
        let mut socket = case.open().await;
        case.scopes(json!(["answer", "operate"]));
        for (id, verb, code, exit) in [
            (
                "future".to_owned(),
                "future.command",
                "server_version_gap",
                crate::cli::exit::PROTOCOL_GAP,
            ),
            (
                "x".repeat(129),
                "agent.decide",
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
async fn enqueue_without_owner(server: &Server, socket: &mut Socket, id: String, target: &str) {
    let admitted = server.wake.notified();
    tokio::pin!(admitted);
    admitted.as_mut().enable();
    socket.send(json!({"t":"cmd","cmd_id":id,"verb":"agent.decide","args":{"agent":target,"request_id":"missing","request_seq":1,"decision":"deny"},"resume_only":false})).await;
    within(async {tokio::select!{_=&mut admitted=>(),frame=socket.receive()=>panic!("queue producer ended before admission: {frame:?}")}}).await;
}
#[test]
fn fifth_concurrent_decision_is_busy_and_keeps_the_socket_open() {
    runtime(async {
        let case = Case::new(183112).await;
        let mut socket = case.open().await;
        for n in 0..4 {
            enqueue_without_owner(
                &case.server,
                &mut socket,
                format!("pending-{n}"),
                &case.worker.id,
            )
            .await;
        }
        for n in 4..6 {
            socket.send(json!({"t":"cmd","cmd_id":format!("excess-{n}"),"verb":"agent.decide","args":{},"resume_only":false})).await;
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
fn full_decision_queue_refuses_other_principals_without_closing() {
    runtime(async {
        let mut case = Case::new(183113).await;
        let mut producers = Vec::new();
        for token in ["a", "d"] {
            for connection in 0..4 {
                let mut socket = Socket::open(&case.server).await;
                socket.hello(1, &token.repeat(64)).await;
                for n in 0..4 {
                    enqueue_without_owner(
                        &case.server,
                        &mut socket,
                        format!("{connection}-{n}"),
                        &case.worker.id,
                    )
                    .await;
                }
                producers.push(socket);
            }
        }
        let mut socket = Socket::open(&case.server).await;
        socket.hello(1, &"b".repeat(64)).await;
        socket.send(json!({"t":"cmd","cmd_id":"overflow","verb":"agent.decide","args":{},"resume_only":false})).await;
        refusal(
            socket.text().await,
            "session_busy",
            crate::cli::exit::FAILURE,
        );
        // Service frees capacity; the same healthy socket must still accept a command.
        case.service();
        let args = json!({"agent":case.worker.id,"request_id":"missing","request_seq":1,"decision":"deny"});
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["error"]["code"],
            "permission_request_unknown"
        );
        drop(producers);
        case.stop().await;
    });
}
#[test]
fn newer_wire_client_cannot_decide() {
    runtime(async {
        let case = Case::new(183116).await;
        let mut socket = Socket::open(&case.server).await;
        socket.hello(WIRE_JSON_VERSION + 1, &"a".repeat(64)).await;
        socket.send(json!({"t":"cmd","cmd_id":"read-only","verb":"agent.decide","args":{},"resume_only":false})).await;
        refusal(
            socket.text().await,
            "server_version_gap",
            crate::cli::exit::PROTOCOL_GAP,
        );
        case.stop().await;
    });
}
