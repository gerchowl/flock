//! Server-produced fleet frames, not client-side injected fleet payloads.
use super::*;
use crate::structured::{
    registry::{Record, Registry, StructuredRuntime},
    workers::Workers,
};
struct Case {
    server: Server,
    registry: Registry,
    workers: Workers,
}
impl Case {
    async fn new() -> Self {
        let server = Server::new().await;
        let workers = Workers::new(server.wake.clone());
        Self {
            server,
            registry: Registry::default(),
            workers,
        }
    }
    fn service(&mut self) {
        Arc::get_mut(&mut self.server.core).unwrap().service(
            &self.registry,
            &mut self.workers,
            self.server._root.path(),
        );
    }
    fn add(&mut self, id: &str) {
        self.registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: id.into(),
                name: Some(id.into()),
                harness: "claude".into(),
                session_id: format!("session-{id}"),
                cwd: self.server._root.path().to_owned(),
                model: None,
            })
            .unwrap();
    }
    async fn open(&self) -> Socket {
        let mut socket = Socket::open(&self.server).await;
        socket.hello(1, &"a".repeat(64)).await;
        socket
    }
    async fn frame(&mut self, socket: &mut Socket) -> Value {
        let wake = self.server.wake.clone();
        within(async {
            loop {
                tokio::select! { biased;
                    frame = socket.receive() => return match frame { Received::Text(frame) => frame, other => panic!("unexpected socket end: {other:?}") },
                    _ = wake.notified() => self.service(),
                }
            }
        }).await
    }
}
#[test]
fn fleet_endpoint_empty_and_nonempty_bootstrap() {
    runtime(async {
        let mut case = Case::new().await;
        case.service();
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"empty","stream":"fleet"}))
            .await;
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"gap","sub_id":"empty","from":1,"to":0,"snapshot_to":0})
        );
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"empty","seq":0})
        );
        case.add("fixture-agent");
        case.service();
        let row = case.frame(&mut socket).await;
        assert_eq!(row["ev"]["type"], "fleet.upsert");
        let row: FleetAgentRow = serde_json::from_value(row["ev"]["row"].clone()).unwrap();
        assert_eq!(row.id, "fixture-agent");
        assert!(row.activity.is_none());
        assert!(row.waiting_on_human.is_empty());
        socket
            .send(json!({"t":"sub","sub_id":"snapshot","stream":"fleet"}))
            .await;
        let checkpoint = case.frame(&mut socket).await;
        assert_eq!(checkpoint["t"], "gap");
        assert!(checkpoint["snapshot_to"].as_u64().unwrap() > checkpoint["to"].as_u64().unwrap());
        case.server.finish().await;
    });
}
struct Controlled {
    subscriptions: super::super::subscriptions::Subscriptions,
    frames: mpsc::Receiver<super::super::output::Delivery>,
    _priority: mpsc::Receiver<Message>,
}
impl Controlled {
    fn new(case: &Case) -> Self {
        let (data, frames) = mpsc::channel(8);
        let (priority, control) = mpsc::channel(4);
        let shared = case.server.core.shared.clone();
        let session = shared.store.authenticate(&Bearer("a".repeat(64))).unwrap();
        Self {
            subscriptions: super::super::subscriptions::Subscriptions::new(
                shared, data, session, priority,
            ),
            frames,
            _priority: control,
        }
    }
    async fn delivery(&mut self) -> super::super::output::Delivery {
        within(self.frames.recv()).await.unwrap()
    }
    async fn frame(&mut self) -> Value {
        let delivery = self.delivery().await;
        let frame = serde_json::to_value(delivery.frame).unwrap();
        delivery.delivered.unwrap().send(true).unwrap();
        frame
    }
    fn sub(&mut self, id: &str, after: Option<u64>) {
        assert!(self
            .subscriptions
            .subscribe(id.into(), "fleet".into(), after));
    }
}
impl Case {
    fn remove(&mut self, id: &str) {
        let generation = self.registry.get(id).unwrap().2;
        self.registry.remove(id, generation).unwrap();
    }
}
#[test]
fn fleet_replay_cut_is_immutable_while_owner_mutates_before_delivery() {
    runtime(async {
        let mut case = Case::new().await;
        case.add("fixture-agent");
        case.add("second-fixture-agent");
        case.service();
        let mut wire = Controlled::new(&case);
        wire.sub("cut", None);
        let gap = wire.delivery().await;
        let snapshot_end = serde_json::to_value(&gap.frame).unwrap()["snapshot_to"]
            .as_u64()
            .unwrap();
        // Holding the writer acknowledgement pins replay at its first frame.
        case.remove("second-fixture-agent");
        case.service();
        let fence = within(case.server.core.shared.fleet.subscribe(Some(snapshot_end)))
            .await
            .unwrap();
        drop(fence);
        gap.delivered.unwrap().send(true).unwrap();
        let a = wire.frame().await;
        let b = wire.frame().await;
        assert_eq!(a["ev"]["row"]["id"], "fixture-agent");
        assert_eq!(b["ev"]["row"]["id"], "second-fixture-agent");
        assert_eq!(
            wire.frame().await,
            json!({"t":"synced","sub_id":"cut","seq":snapshot_end})
        );
        let removal = wire.frame().await;
        assert_eq!(
            removal["ev"],
            json!({"type":"fleet.remove","id":"second-fixture-agent"})
        );
        assert_eq!(removal["seq"], snapshot_end + 1);
        drop(wire);
        case.server.finish().await;
    });
}
#[test]
fn fleet_lagged_id_retires_and_fresh_id_resnapshots_without_unsubscribe() {
    runtime(async {
        let mut case = Case::new().await;
        case.add("fixture-agent");
        case.service();
        let mut wire = Controlled::new(&case);
        wire.sub("old", None);
        let paused = wire.delivery().await;
        for n in 0..20 {
            case.add(&format!("fixture-{n:02}"));
        }
        case.service();
        let fence = within(case.server.core.shared.fleet.subscribe(None))
            .await
            .unwrap();
        drop(fence);
        paused.delivered.unwrap().send(true).unwrap();
        let lagged = wire.frame().await;
        assert_eq!(lagged["t"], "lagged");
        assert_eq!(lagged["sub_id"], "old");
        wire.sub("old", Some(0));
        assert_eq!(wire.frame().await["code"], "sub_id_reused");
        wire.sub("fresh", Some(0));
        let gap = wire.frame().await;
        assert_eq!(gap["t"], "gap");
        assert_eq!(gap["sub_id"], "fresh");
        let end = gap["snapshot_to"].as_u64().unwrap();
        let mut rows = std::collections::BTreeSet::new();
        for _ in 0..21 {
            let frame = wire.frame().await;
            assert_eq!(frame["sub_id"], "fresh");
            let delta: FleetDelta = serde_json::from_value(frame["ev"].clone()).unwrap();
            let FleetDelta::Upsert { row } = delta else {
                panic!("full snapshot")
            };
            assert!(rows.insert(row.id));
        }
        assert!(rows.contains("fixture-agent"));
        assert_eq!(
            wire.frame().await,
            json!({"t":"synced","sub_id":"fresh","seq":end})
        );
        drop(wire);
        case.server.finish().await;
    });
}
#[test]
fn fleet_private_checkpoints_do_not_lag_other_readers() {
    runtime(async {
        use std::collections::BTreeSet;
        async fn snapshot(wire: &mut Controlled, id: &str, count: usize) -> BTreeSet<String> {
            let gap = wire.frame().await;
            assert_eq!(gap["t"], "gap");
            assert_eq!(gap["sub_id"], id);
            let mut rows = BTreeSet::new();
            for _ in 0..count {
                let row = wire.frame().await;
                assert_eq!(row["t"], "ev", "{row}");
                assert_eq!(row["sub_id"], id);
                assert!(rows.insert(row["ev"]["row"]["id"].as_str().unwrap().to_owned()));
            }
            assert_eq!(
                wire.frame().await,
                json!({"t":"synced","sub_id":id,"seq":gap["snapshot_to"]})
            );
            rows
        }
        async fn update(wire: &mut Controlled, id: &str) -> u64 {
            let row = wire.frame().await;
            assert_eq!(
                row["t"], "ev",
                "a different reader's checkpoint leaked: {row}"
            );
            assert_eq!(row["ev"]["type"], "fleet.upsert");
            assert_eq!(row["ev"]["row"]["id"], id);
            row["seq"].as_u64().unwrap()
        }
        async fn flush(case: &Case) {
            // The owner replies only after publishing pending live deltas and registering this cut.
            drop(
                within(case.server.core.shared.fleet.subscribe(None))
                    .await
                    .unwrap(),
            );
        }
        let mut case = Case::new().await;
        for n in 0..17 {
            case.add(&format!("fixture-{n:02}"));
        }
        case.service();
        let mut a = Controlled::new(&case);
        let mut b = Controlled::new(&case);
        let mut c = Controlled::new(&case);
        a.sub("a", None);
        let mut expected = snapshot(&mut a, "a", 17).await;
        b.sub("b", None);
        assert_eq!(snapshot(&mut b, "b", 17).await, expected);
        c.sub("c", None);
        assert_eq!(snapshot(&mut c, "c", 17).await, expected);
        case.add("live-fixture");
        case.service();
        flush(&case).await;
        let seq = update(&mut a, "live-fixture").await;
        assert_eq!(update(&mut b, "live-fixture").await, seq);
        assert_eq!(update(&mut c, "live-fixture").await, seq);
        expected.insert("live-fixture".into());

        case.add("paused-fixture");
        case.service();
        flush(&case).await;
        let paused = a.delivery().await;
        assert_eq!(
            serde_json::to_value(&paused.frame).unwrap()["ev"]["row"]["id"],
            "paused-fixture"
        );
        let seq = update(&mut b, "paused-fixture").await;
        assert_eq!(update(&mut c, "paused-fixture").await, seq);
        expected.insert("paused-fixture".into());
        // Only a is slow: hold its real writer acknowledgement across twenty live changes.
        for n in 0..20 {
            let id = format!("changed-fixture-{n:02}");
            case.add(&id);
            case.service();
            flush(&case).await;
            let seq = update(&mut b, &id).await;
            assert_eq!(update(&mut c, &id).await, seq);
            expected.insert(id);
        }
        paused.delivered.unwrap().send(true).unwrap();
        assert_eq!(a.frame().await["t"], "lagged");
        a.sub("fresh", Some(0));
        assert_eq!(snapshot(&mut a, "fresh", expected.len()).await, expected);
        case.add("after-resnapshot");
        case.service();
        flush(&case).await;
        let seq = update(&mut a, "after-resnapshot").await;
        assert_eq!(update(&mut b, "after-resnapshot").await, seq);
        assert_eq!(update(&mut c, "after-resnapshot").await, seq);
        drop((a, b, c));
        case.server.finish().await;
    });
}
#[test]
fn fleet_real_permission_book_supplies_harness_ids_and_clears_them() {
    runtime(async {
        use crate::structured::{
            permissions::{self, CloseReason, Principal},
            workers::tests::{event, wait, Case as Worker},
        };
        let mut case = Case::new().await;
        let wake = case.server.wake.clone();
        let now = Instant::now();
        let request = event("control_request");
        let request_id = request["request_id"].as_str().unwrap().to_owned();
        let worker = Worker::start(
            &mut case.registry,
            &mut case.workers,
            2225,
            "state-reducer",
            vec![event("command_lifecycle"), request],
        );
        worker
            .human(&mut case.registry, &mut case.workers, &wake, now)
            .await;
        case.service();
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"human","stream":"fleet"}))
            .await;
        assert_eq!(case.frame(&mut socket).await["t"], "gap");
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["ev"]["row"]["waiting_on_human"], json!([request_id]));
        assert!(frame["ev"]["row"].get("activity").is_none());
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        let principal = Principal::Token(permissions::auth::fixture("fixture-operator", true));
        case.workers
            .runtime(&worker.id, worker.generation)
            .unwrap()
            .close_permissions(&principal, Some(&request_id), CloseReason::Interrupted)
            .unwrap();
        wait(&wake, || {
            case.workers.poll(&mut case.registry, now, 1000);
            case.registry
                .get(&worker.id)
                .unwrap()
                .1
                .update
                .needs_human
                .is_empty()
        })
        .await;
        case.service();
        assert_eq!(
            case.frame(&mut socket).await["ev"]["row"]["waiting_on_human"],
            json!([])
        );
        worker
            .finish(&mut case.registry, &mut case.workers, &wake, now, None)
            .await;
        case.server.finish().await;
    });
}
#[test]
fn fleet_nonzero_empty_checkpoint_survives_restart_and_ahead_is_typed() {
    runtime(async {
        let mut case = Case::new().await;
        case.add("fixture-agent");
        case.service();
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"before","stream":"fleet"}))
            .await;
        for _ in 0..3 {
            case.frame(&mut socket).await;
        }
        case.remove("fixture-agent");
        case.service();
        let removed = case.frame(&mut socket).await;
        assert_eq!(removed["ev"]["type"], "fleet.remove");
        let high = removed["seq"].as_u64().unwrap();
        case.server.core.shutdown().await.unwrap();
        case.server.task.abort();
        let _ = (&mut case.server.task).await;
        drop(socket);
        let core = Core::with_clock(
            TokenStore::open(case.server._root.path()).unwrap(),
            Arc::new(case.server.clock.clone()),
            case.server.wake.clone(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        case.server.bind = listener.local_addr().unwrap();
        let router = core.router();
        case.server.task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        case.server.core = Arc::new(core);
        case.service();
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"restart","stream":"fleet","after":high}))
            .await;
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"gap","sub_id":"restart","from":high+1,"to":high,"snapshot_to":high})
        );
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"restart","seq":high})
        );
        socket
            .send(json!({"t":"sub","sub_id":"ahead","stream":"fleet","after":high+100}))
            .await;
        assert_eq!(case.frame(&mut socket).await["code"], "cursor_ahead");
        case.add("second-fixture-agent");
        case.service();
        assert_eq!(case.frame(&mut socket).await["seq"], high + 1);
        case.server.finish().await;
    });
}
#[test]
fn fleet_server_driven_script_removes_absent_rows_only_at_snapshot_completion() {
    runtime(async {
        fn selected(actual: &Value, expected: &Value) {
            if let Some(fields) = expected.as_object() {
                for (key, value) in fields {
                    selected(&actual[key], value);
                }
            } else {
                assert_eq!(actual, expected);
            }
        }
        let mut case = Case::new().await;
        case.service();
        let mut socket = case.open().await;
        let mut rows = std::collections::BTreeMap::<String, FleetAgentRow>::new();
        let mut snapshot = None;
        let mut fresh = std::collections::BTreeMap::new();
        for line in
            include_str!("../../../../tests/fixtures/structured-wire/fleet-server.jsonl").lines()
        {
            let step: Value = serde_json::from_str(line).unwrap();
            if let Some(id) = step["add"].as_str() {
                case.add(id);
            } else if let Some(id) = step["remove"].as_str() {
                case.remove(id);
            } else if let Some(id) = step["unsub"].as_str() {
                socket.send(json!({"t":"unsub","sub_id":id})).await;
            } else if let Some(sub) = step["sub"].as_object() {
                let mut frame = sub.clone();
                frame.insert("t".into(), json!("sub"));
                socket.send(Value::Object(frame)).await;
            } else {
                let frame = case.frame(&mut socket).await;
                selected(&frame, &step["expect"]);
                match frame["t"].as_str().unwrap() {
                    "gap" => {
                        snapshot = frame["snapshot_to"].as_u64();
                        fresh.clear();
                        if frame["snapshot_to"] == frame["to"] {
                            rows.clear();
                            snapshot = None;
                        }
                    }
                    "ev" => {
                        let target = if snapshot.is_some() {
                            &mut fresh
                        } else {
                            &mut rows
                        };
                        match serde_json::from_value::<FleetDelta>(frame["ev"].clone()).unwrap() {
                            FleetDelta::Upsert { row } => {
                                assert!(row.activity.is_none());
                                target.insert(row.id.clone(), row);
                            }
                            FleetDelta::Remove { id } => {
                                target.remove(&id);
                            }
                        }
                        if snapshot == frame["seq"].as_u64() {
                            rows = std::mem::take(&mut fresh);
                            snapshot = None;
                        }
                    }
                    "synced" => assert!(snapshot.is_none(), "barrier before completion"),
                    _ => panic!("unexpected server frame: {frame}"),
                }
                assert_eq!(
                    serde_json::to_value(rows.keys().collect::<Vec<_>>()).unwrap(),
                    step["rows"]
                );
            }
            case.service();
        }
        case.server.finish().await;
    });
}
#[test]
fn fleet_server_drives_the_real_client_through_nonempty_and_empty_replacement() {
    runtime(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut case = Case::new().await;
        case.add("fixture-agent");
        case.service();
        let mut child = tokio::process::Command::new("bun");
        child
            .arg("web/client/tests/wire/fleet-server.ts")
            .arg(format!("ws://{}/v1/ws", case.server.bind))
            .env_remove("FLOCK_SOCKET_PATH")
            .env_remove("FLOCK_PANE_ID")
            .env_remove("FLOCK_AGENT_ID")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("SKIP fleet_server_drives_the_real_client: Bun is absent from PATH; nextest CI provisioning is tracked in #2260");
                case.server.finish().await;
                return;
            }
            Err(error) => panic!("Bun client launch failed: {error}"),
        };
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut input = child.stdin.take().unwrap();
        let wake = case.server.wake.clone();
        for expected in ["replace", "empty", "complete"] {
            let stage = within(async {
                loop { tokio::select! { biased;
                    line = lines.next_line() => return line.unwrap().expect("client exited before stage"),
                    _ = wake.notified() => case.service(),
                }}
            }).await;
            assert_eq!(stage, expected, "unexpected client stage");
            match expected {
                "replace" => {
                    case.remove("fixture-agent");
                    case.add("second-fixture-agent");
                }
                "empty" => case.remove("second-fixture-agent"),
                _ => continue,
            }
            case.service();
            input.write_all(b"changed\n").await.unwrap();
            input.flush().await.unwrap();
        }
        drop(input);
        assert!(within(child.wait()).await.unwrap().success());
        case.server.finish().await;
    });
}
#[test]
fn fleet_read_scope_refusal_retires_the_id_before_cut_registration() {
    runtime(async {
        let mut case = Case::new().await;
        case.service();
        let path = case.server._root.path().join("client-tokens/one.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["scopes"] = json!(["send"]);
        fs::write(path, serde_json::to_vec(&token).unwrap()).unwrap();
        let mut socket = case.open().await;
        for expected in ["scope", "sub_id_reused"] {
            socket
                .send(json!({"t":"sub","sub_id":"denied","stream":"fleet"}))
                .await;
            assert_eq!(case.frame(&mut socket).await["code"], expected);
        }
        case.server.finish().await;
    });
}
