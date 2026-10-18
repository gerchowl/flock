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
                .join(format!("{}.jsonl", self.queue.case.session)),
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
                    .join(format!("{}.jsonl", r.case.session));
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
                json!({"target":"remote.example","text":"remote","submit":true}),
                "unsupported_for_agent",
            ),
            (
                "agent.interrupt",
                json!({"agent":case.queue.case.id}),
                "unsupported_for_agent",
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
#[test]
fn permission_decisions_use_the_named_request_handle_and_typed_refusals() {
    runtime(async {
        let mut case = Case::new(183105).await;
        let mut socket = case.open().await;
        for id in ["first-request", "other-request"] {
            let mut request = crate::structured::workers::tests::event("control_request");
            request["request_id"] = json!(id);
            case.queue.control(json!({"op":"event","event":request}));
        }
        case.queue
            .until(|r| {
                r.app
                    .structured_workers
                    .runtime(&r.case.id, r.case.generation)
                    .unwrap()
                    .waiting_on_human()
                    .unwrap()
                    .len()
                    == 2
            })
            .await;
        let handles = case
            .queue
            .app
            .structured_workers
            .runtime(&case.queue.case.id, case.queue.case.generation)
            .unwrap()
            .waiting_on_human()
            .unwrap();
        let seq = handles
            .iter()
            .find(|(id, _)| id == "first-request")
            .unwrap()
            .1;
        let args = json!({"agent":case.queue.case.id,"request_id":"first-request","request_seq":seq,"decision":"deny"});
        case.scopes(json!(["answer", "read"]));
        assert_eq!(
            case.command(&mut socket, "agent.decide", args.clone())
                .await["error"]["code"],
            "not_authorized"
        );
        case.scopes(json!(["answer", "read", "operate"]));
        let mut unknown = args.clone();
        unknown["request_id"] = json!("missing-request");
        assert_eq!(
            case.command(&mut socket, "agent.decide", unknown).await["error"]["code"],
            "permission_request_unknown"
        );
        let mut stale = args.clone();
        stale["request_seq"] = json!(seq + 100);
        assert_eq!(
            case.command(&mut socket, "agent.decide", stale).await["error"]["code"],
            "permission_request_unknown"
        );
        let ack = case
            .command(&mut socket, "agent.decide", args.clone())
            .await;
        assert_eq!(
            ack["result"],
            json!({"request_id":"first-request","decision":"deny"})
        );
        case.queue
            .until(|r| {
                r.app
                    .structured_workers
                    .runtime(&r.case.id, r.case.generation)
                    .unwrap()
                    .waiting_on_human()
                    .unwrap()
                    .len()
                    == 1
            })
            .await;
        let remaining = case
            .queue
            .app
            .structured_workers
            .runtime(&case.queue.case.id, case.queue.case.generation)
            .unwrap()
            .waiting_on_human()
            .unwrap();
        assert_eq!(remaining[0].0, "other-request");
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["error"]["code"],
            "permission_request_closed"
        );
        case.queue
            .until(|r| {
                let path = r
                    .case
                    .fixture
                    .root
                    .path()
                    .join("sessions")
                    .join(&r.case.id)
                    .join(format!("{}.jsonl", r.case.session));
                fs::read_to_string(path)
                    .unwrap()
                    .contains("fixture_control_input")
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
