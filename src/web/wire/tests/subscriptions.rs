//! Persisted JSONL through the actual authenticated WebSocket endpoint.
use super::*;
use crate::structured::{
    history::tests::Fixture as Stored,
    registry::{Record, Registry, StructuredRuntime},
    transport::{RecordOrigin, StreamRecord},
    workers::Workers,
};
fn record(seq: u64) -> StreamRecord {
    StreamRecord {
        seq,
        known: Some(true),
        shape: None,
        received_at_ms: 42,
        origin: RecordOrigin::Claude,
        result_subtype: None,
        permission_grant_request_id: None,
        permission_request_seq: None,
        raw: json!({"type":"assistant","message":{"content":[]}}).to_string(),
    }
}
struct Case {
    server: Server,
    registry: Registry,
    workers: Workers,
    stored: Stored,
}
impl Case {
    async fn new(records: Vec<StreamRecord>) -> Self {
        let server = Server::new().await;
        let stored = Stored::new(records);
        let mut registry = Registry::default();
        let generation = registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: "fixture-agent".into(),
                session_id: "history".into(),
                name: None,
                harness: "claude".into(),
                cwd: server._root.path().to_owned(),
                model: None,
            })
            .unwrap();
        let mut workers = Workers::new(server.wake.clone());
        workers
            .install_retained(
                &registry,
                "fixture-agent",
                generation,
                stored.source.clone(),
            )
            .unwrap();
        Self {
            server,
            registry,
            workers,
            stored,
        }
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
                    _ = wake.notified() => Arc::get_mut(&mut self.server.core).unwrap().service(&self.registry, &mut self.workers, self.server._root.path()),
                }
            }
        }).await
    }
    async fn sub(&mut self, socket: &mut Socket, id: &str, after: Option<u64>) {
        socket
            .send(json!({"t":"sub","sub_id":id,"stream":"agent/fixture-agent","after":after}))
            .await;
    }
}
#[test]
fn jsonl_bootstrap_resume_barrier_live_and_independent_cuts() {
    runtime(async {
        let mut case = Case::new(vec![record(1), record(2)]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "first", None).await;
        for seq in 1..=2 {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["seq"], seq);
            assert_eq!(frame["t"], "ev");
            assert_eq!(frame["ts_ms"], 42);
        }
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"first","seq":2})
        );
        case.sub(&mut socket, "second", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["seq"], 2);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.stored.append(record(3));
        let mut ids = Vec::new();
        for _ in 0..2 {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["t"], "ev");
            assert_eq!(frame["seq"], 3);
            ids.push(frame["sub_id"].as_str().unwrap().to_owned());
        }
        ids.sort();
        assert_eq!(ids, ["first", "second"]);
        case.server.finish().await;
    });
}
#[test]
fn typed_refusals_retire_ids_and_unsubscribe_fences_queued_output() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"bad","stream":"agent/absent"}))
            .await;
        assert_eq!(case.frame(&mut socket).await["code"], "unknown_stream");
        case.sub(&mut socket, "bad", None).await;
        assert_eq!(case.frame(&mut socket).await["code"], "sub_id_reused");
        case.sub(&mut socket, "ahead", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["code"], "cursor_ahead");
        case.sub(&mut socket, "live", None).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.sub(&mut socket, "live", None).await;
        assert_eq!(case.frame(&mut socket).await["code"], "sub_id_reused");
        case.sub(&mut socket, "unsub", None).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        socket.send(json!({"t":"unsub","sub_id":"unsub"})).await;
        // A same-socket refusal is an ordered receive-side fence after unsubscribe.
        case.sub(&mut socket, "unsub", None).await;
        assert_eq!(case.frame(&mut socket).await["code"], "sub_id_reused");
        case.stored.append(record(1));
        case.sub(&mut socket, "fresh", Some(0)).await;
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["sub_id"], "fresh");
        assert_eq!(frame["seq"], 1);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.server.clock.deadline(15_000).await;
        case.server.clock.advance(15_000);
        assert_eq!(case.frame(&mut socket).await["t"], "ping");
        case.server.finish().await;
    });
}
#[test]
fn read_scope_is_checked_without_even_servicing_the_owner() {
    runtime(async {
        let server = Server::new().await;
        let path = server._root.path().join("client-tokens/one.token");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["scopes"] = json!(["send"]);
        fs::write(&path, value.to_string()).unwrap();
        let mut socket = Socket::open(&server).await;
        socket.hello(1, &"a".repeat(64)).await;
        socket
            .send(json!({"t":"sub","sub_id":"s1","stream":"agent/absent"}))
            .await;
        let Received::Text(frame) = socket.receive().await else {
            panic!("scope refusal")
        };
        assert_eq!(frame["code"], "scope");
        assert!(
            server.wake.notified().now_or_never().is_none(),
            "scope refusal must not touch the owner mailbox"
        );
        server.finish().await;
    });
}
#[test]
fn agent_gap_and_persisted_unknown_provenance_blobs_and_permission_audits() {
    runtime(async {
        let mut unknown = record(5);
        unknown.known = Some(false);
        unknown.shape = Some(crate::structured::claude::ShapeKey(
            "original-version/novel".into(),
        ));
        unknown.raw = "diagnostic raw".into();
        let blob_root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let payload = json!({"type":"thinking","thinking":"fixture text".repeat(256)}).to_string();
        let reference = crate::structured::blobs::BlobStore::open(blob_root.path())
            .unwrap()
            .put(payload.as_bytes(), 8192)
            .unwrap();
        let mut blob = record(6);
        blob.raw =
            json!({"type":"assistant","message":{"content":[reference.clone()]}}).to_string();
        let mut audit = record(7);
        audit.known = Some(false);
        audit.origin = RecordOrigin::Flk;
        audit.shape = Some(crate::structured::claude::ShapeKey("flk/permission".into()));
        audit.raw =
            json!({"type":"flk_permission_request","state":"open","request_id":"req"}).to_string();
        let mut closed = audit.clone();
        closed.seq = 8;
        closed.raw = json!({"type":"flk_permission_request","state":"closed","request_id":"req"})
            .to_string();
        let mut case = Case::new(vec![
            unknown.clone(),
            blob.clone(),
            audit.clone(),
            closed.clone(),
        ])
        .await;
        let store = crate::structured::blobs::BlobStore::open(case.server._root.path()).unwrap();
        assert_eq!(store.put(payload.as_bytes(), 8192).unwrap(), reference);
        assert_eq!(
            store.get(&reference.blob, reference.size).unwrap(),
            payload.as_bytes()
        );
        let mut socket = case.open().await;
        case.sub(&mut socket, "s1", Some(2)).await;
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"gap","sub_id":"s1","from":3,"to":4})
        );
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["seq"], 5);
        assert_eq!(
            frame["ev"],
            json!({"type":"unknown","raw":unknown.raw,"shape":"original-version/novel"})
        );
        assert_eq!(
            case.frame(&mut socket).await["ev"],
            serde_json::from_str::<Value>(&blob.raw).unwrap()
        );
        for record in [audit, closed] {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["seq"], record.seq);
            assert_eq!(frame["ev"]["raw"], record.raw);
        }
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"s1","seq":8})
        );
        case.server.finish().await;
    });
}
#[test]
fn oversized_record_lags_only_its_cut_and_fresh_id_resumes_applied_cursor() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "s1", None).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        let mut huge = record(1);
        huge.raw =
            json!({"type":"assistant","message":{"padding":"x".repeat(MAX_MESSAGE)}}).to_string();
        case.stored.append(huge);
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"lagged","sub_id":"s1","skipped_to":1})
        );
        case.sub(&mut socket, "s1", Some(0)).await;
        assert_eq!(case.frame(&mut socket).await["code"], "sub_id_reused");
        case.sub(&mut socket, "s2", Some(0)).await;
        assert_eq!(case.frame(&mut socket).await["t"], "lagged");
        case.sub(&mut socket, "s3", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.stored.append(record(2));
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["sub_id"], "s3");
        assert_eq!(frame["seq"], 2);
        case.server.finish().await;
    });
}
#[test]
fn fault_proxy_exercises_real_client_never_opened_and_late_id_frames() {
    runtime(async {
        for mode in ["never", "late"] {
            let mut case = Case::new(vec![record(1)]).await;
            let mut child = tokio::process::Command::new("bun");
            child
                .arg("web/client/tests/wire/agent-proxy.ts")
                .arg(format!("ws://{}/v1/ws", case.server.bind))
                .arg(mode)
                .env_remove("FLOCK_SOCKET_PATH")
                .env_remove("FLOCK_PANE_ID")
                .env_remove("FLOCK_AGENT_ID")
                .kill_on_drop(true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let child = child.spawn().expect("dev shell provides bun");
            let wake = case.server.wake.clone();
            let output=within(async {
                let done=child.wait_with_output();tokio::pin!(done);
                loop {tokio::select! {biased;
                    output=&mut done=>return output.unwrap(),
                    _=wake.notified()=>Arc::get_mut(&mut case.server.core).unwrap().service(&case.registry,&mut case.workers,case.server._root.path()),
                }}
            }).await;
            case.server.finish().await;
            assert!(
                output.status.success(),
                "{mode}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    });
}

#[test]
fn live_overflow_closes_both_cuts_and_a_fresh_id_replays_from_applied_cursor() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let mut socket = case.open().await;
        for id in ["first", "second"] {
            case.sub(&mut socket, id, None).await;
            assert_eq!(case.frame(&mut socket).await["t"], "synced");
        }
        // Same executor, no await: publication necessarily precedes the next page.
        for seq in 1..=crate::structured::history::LIVE_RECORDS as u64 + 1 {
            case.stored.append(record(seq));
        }
        let mut ids = Vec::new();
        for _ in 0..2 {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["t"], "lagged");
            ids.push(frame["sub_id"].as_str().unwrap().to_owned());
        }
        ids.sort();
        assert_eq!(ids, ["first", "second"]);
        case.sub(&mut socket, "fresh", Some(0)).await;
        for seq in 1..=crate::structured::history::LIVE_RECORDS as u64 + 1 {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["t"], "ev");
            assert_eq!(frame["seq"], seq);
        }
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.server.finish().await;
    });
}

#[test]
fn active_cut_budget_closes_the_socket_without_an_unbounded_owner_queue() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let mut socket = case.open().await;
        for id in 0..8 {
            case.sub(&mut socket, &format!("s{id}"), None).await;
            assert_eq!(case.frame(&mut socket).await["t"], "synced");
        }
        case.sub(&mut socket, "over-budget", None).await;
        assert!(matches!(socket.receive().await, Received::Close(1001)));
        case.server.finish().await;
    });
}

#[test]
fn shrinking_read_scope_blocks_new_registration_before_owner_lookup() {
    runtime(async {
        let server = Server::new().await;
        let mut socket = Socket::open(&server).await;
        socket.hello(1, &"a".repeat(64)).await;
        let path = server._root.path().join("client-tokens/one.token");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["scopes"] = json!(["send"]);
        fs::write(path, value.to_string()).unwrap();
        socket
            .send(json!({"t":"sub","sub_id":"s1","stream":"agent/fixture-agent"}))
            .await;
        let Received::Text(frame) = socket.receive().await else {
            panic!("scope refusal")
        };
        assert_eq!(frame["code"], "scope");
        assert!(
            server.wake.notified().now_or_never().is_none(),
            "scope refusal must not touch the owner mailbox"
        );
        server.finish().await;
    });
}

#[test]
fn selected_agent_gap_fixture_runs_against_cold_persisted_jsonl_and_owner_install() {
    runtime(async {
        use std::io::Write;
        let mut case = Case::new(vec![]).await;
        case.registry = Registry::default();
        case.workers = Workers::new(case.server.wake.clone());
        case.registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: "fixture".into(),
                session_id: "history".into(),
                name: None,
                harness: "claude".into(),
                cwd: case.server._root.path().to_owned(),
                model: None,
            })
            .unwrap();
        let sessions = case.server._root.path().join("sessions");
        crate::platform::structured::ensure_private_dir(&sessions).unwrap();
        let directory = sessions.join("fixture");
        crate::platform::structured::ensure_private_dir(&directory).unwrap();
        let mut file =
            crate::platform::structured::create_private_file(&directory.join("history.jsonl"))
                .unwrap();
        let mut stored = record(5);
        stored.received_at_ms = 1;
        stored.raw = json!({"type":"assistant","message":{}}).to_string();
        writeln!(file, "{}", serde_json::to_string(&stored).unwrap()).unwrap();
        file.sync_all().unwrap();
        let script = include_str!("../../../../web/client/test/fixtures/wire/agent-gap.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(script[0]["covers"], json!(["s7-min"]));
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"s1","stream":"agent/fixture","after":2}))
            .await;
        for frame in script
            .iter()
            .filter(|line| line["step"] == "send")
            .map(|line| &line["frame"])
            .filter(|frame| frame["t"] != "welcome")
        {
            assert_eq!(&case.frame(&mut socket).await, frame);
        }
        assert!(case.workers.history(&case.registry, "fixture").is_some());
        case.server.finish().await;
    });
}

#[test]
fn legacy_provenance_gap_continues_to_later_records_and_the_barrier() {
    runtime(async {
        let mut legacy = record(1);
        legacy.known = None;
        let mut case = Case::new(vec![legacy, record(2)]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "s1", None).await;
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"gap","sub_id":"s1","from":1,"to":1})
        );
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["t"], "ev");
        assert_eq!(frame["seq"], 2);
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"s1","seq":2})
        );
        case.server.finish().await;
    });
}

#[test]
fn lifetime_identity_budget_and_name_caps_end_with_a_resource_close() {
    runtime(async {
        for (id, stream) in [
            ("x".repeat(129), "agent/fixture".into()),
            ("s1".into(), "x".repeat(257)),
        ] {
            let server = Server::new().await;
            let mut socket = Socket::open(&server).await;
            socket.hello(1, &"a".repeat(64)).await;
            socket
                .send(json!({"t":"sub","sub_id":id,"stream":stream}))
                .await;
            assert!(matches!(socket.receive().await, Received::Close(1001)));
            server.finish().await;
        }
        let server = Server::new().await;
        let mut socket = Socket::open(&server).await;
        socket.hello(1, &"a".repeat(64)).await;
        for id in 0..1024 {
            socket
                .send(json!({"t":"sub","sub_id":format!("s{id}"),"stream":"unknown"}))
                .await;
            let Received::Text(frame) = socket.receive().await else {
                panic!("typed refusal within budget")
            };
            assert_eq!(frame["code"], "unknown_stream");
        }
        socket
            .send(json!({"t":"sub","sub_id":"new","stream":"unknown"}))
            .await;
        assert!(matches!(socket.receive().await, Received::Close(1001)));
        server.finish().await;
    });
}

#[test]
fn unsubscribe_api_fences_an_already_queued_jsonl_event_before_the_writer_starts() {
    runtime(async {
        let mut case = Case::new(vec![record(1)]).await;
        let session = case
            .server
            .core
            .shared
            .store
            .authenticate(&Bearer("a".repeat(64)))
            .unwrap();
        let (data, mut queued) = mpsc::channel(8);
        let (priority, priority_rx) = mpsc::channel(4);
        let mut subscriptions = super::super::subscriptions::Subscriptions::new(
            case.server.core.shared.clone(),
            data.clone(),
            session,
            priority.clone(),
        );
        assert!(subscriptions.subscribe("closed".into(), "agent/fixture-agent".into(), None));
        let wake = case.server.wake.clone();
        let delivery=within(async {
            loop { tokio::select! {biased;
                delivery=queued.recv()=>return delivery.unwrap(),
                _=wake.notified()=>Arc::get_mut(&mut case.server.core).unwrap().service(&case.registry,&mut case.workers,case.server._root.path()),
            }}
        }).await;
        assert!(
            matches!(&delivery.frame,ServerFrame::Ev{sub_id,seq:Some(1),..} if sub_id=="closed")
        );
        subscriptions.unsubscribe("closed");
        assert!(!delivery.active.as_ref().unwrap().load(Ordering::Acquire));
        data.try_send(delivery).unwrap();
        let (done, ack) = tokio::sync::oneshot::channel();
        data.try_send(output::Delivery {
            frame: data_frame("sibling"),
            active: None,
            delivered: Some(done),
        })
        .unwrap();
        priority
            .try_send(
                output::encode(ServerFrame::Ping {
                    nonce: "priority".into(),
                })
                .unwrap(),
            )
            .unwrap();
        let (sink, release, _started, mut flushed, _dropped) = gated_sink();
        release.send(()).unwrap();
        let writer = tokio::spawn(output::writer(
            sink,
            case.server.core.shared.clock.clone(),
            priority_rx,
            queued,
        ));
        assert!(within(ack).await.unwrap());
        let first = within(flushed.recv()).await.unwrap();
        let second = within(flushed.recv()).await.unwrap();
        assert_eq!(
            first,
            output::encode(ServerFrame::Ping {
                nonce: "priority".into()
            })
            .unwrap()
        );
        assert_eq!(second, output::encode(data_frame("sibling")).unwrap());
        assert!(flushed.try_recv().is_err());
        drop(priority);
        drop(subscriptions);
        within(writer).await.unwrap();
        case.server.finish().await;
    });
}

#[test]
fn new_generation_selects_its_source_before_using_an_old_resume_hint() {
    runtime(async {
        let mut case = Case::new(vec![record(1)]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "old", None).await;
        assert_eq!(case.frame(&mut socket).await["seq"], 1);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        socket.send(json!({"t":"unsub","sub_id":"old"})).await;
        let old = case.registry.get("fixture-agent").unwrap().2;
        let new = case.registry.begin_launch("fixture-agent", old).unwrap();
        let mut first = record(1);
        first.raw =
            json!({"type":"assistant","message":{"text":"a different first record"}}).to_string();
        case.stored = Stored::new(vec![first, record(2)]);
        case.workers
            .install_retained(
                &case.registry,
                "fixture-agent",
                new,
                case.stored.source.clone(),
            )
            .unwrap();
        case.sub(&mut socket, "new", Some(1)).await;
        let frame = case.frame(&mut socket).await;
        assert_eq!(frame["t"], "ev");
        assert_eq!(frame["sub_id"], "new");
        assert_eq!(frame["seq"], 2);
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"synced","sub_id":"new","seq":2})
        );
        case.server.finish().await;
    });
}

#[test]
fn invalid_stored_subject_is_distinct_from_unknown_stream() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        case.registry
            .register(Record {
                runtime: StructuredRuntime::Structured,
                agent_id: "reserved".into(),
                session_id: "queue".into(),
                name: None,
                harness: "claude".into(),
                cwd: case.server._root.path().to_owned(),
                model: None,
            })
            .unwrap();
        let mut socket = case.open().await;
        for stream in ["agent/../escaped", "agent/reserved"] {
            socket
                .send(json!({"t":"sub","sub_id":stream,"stream":stream}))
                .await;
            assert_eq!(case.frame(&mut socket).await["code"], "invalid_agent");
        }
        case.sub(&mut socket, "sibling", None).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.server.finish().await;
    });
}
#[test]
fn finished_worker_without_a_report_is_cold_recovered_by_sub() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let worker = crate::structured::workers::tests::Case::uncertain(
            &mut case.registry,
            &mut case.workers,
            433,
            &case.server.wake,
        )
        .await;
        worker.forget_report(&mut case.workers);
        let record = case.registry.get(&worker.id).unwrap().0;
        let sessions = case.server._root.path().join("sessions");
        crate::platform::structured::ensure_private_dir(&sessions).unwrap();
        let dir = sessions.join(&record.agent_id);
        crate::platform::structured::ensure_private_dir(&dir).unwrap();
        let path = dir.join(format!("{}.jsonl", record.session_id));
        use std::io::Write;
        let mut file = crate::platform::structured::create_private_file(&path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&self::record(1)).unwrap()).unwrap();
        file.sync_all().unwrap();
        let mut socket = case.open().await;
        socket
            .send(json!({"t":"sub","sub_id":"recovered","stream":format!("agent/{}",worker.id)}))
            .await;
        assert_eq!(case.frame(&mut socket).await["seq"], 1);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        assert!(case.workers.history(&case.registry, &worker.id).is_some());
        case.server.finish().await;
    });
}
#[test]
fn invalid_known_payload_is_an_inline_gap_without_losing_siblings() {
    runtime(async {
        let mut invalid = record(1);
        invalid.raw = "no longer a session event".into();
        let mut case = Case::new(vec![invalid, record(2)]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "sibling", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["seq"], 2);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.sub(&mut socket, "bad", None).await;
        assert_eq!(
            case.frame(&mut socket).await,
            json!({"t":"gap","sub_id":"bad","from":1,"to":1})
        );
        assert_eq!(case.frame(&mut socket).await["seq"], 2);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        let mut invalid_live = record(3);
        invalid_live.raw = "invalid live payload".into();
        case.stored.append(invalid_live);
        for _ in 0..2 {
            let gap = case.frame(&mut socket).await;
            assert_eq!(gap["t"], "gap");
            assert_eq!(gap["from"], 3);
            assert_eq!(gap["to"], 3);
        }
        case.stored.append(record(4));
        let mut ids = Vec::new();
        for _ in 0..2 {
            let frame = case.frame(&mut socket).await;
            assert_eq!(frame["seq"], 4);
            ids.push(frame["sub_id"].as_str().unwrap().to_owned());
        }
        ids.sort();
        assert_eq!(ids, ["bad", "sibling"]);
        case.server.finish().await;
    });
}
#[test]
fn page_capacity_wait_is_cancellable_and_does_not_close_siblings() {
    runtime(async {
        use super::super::subscriptions::{replay, IO};
        let mut case = Case::new(vec![record(1)]).await;
        let mut socket = case.open().await;
        case.sub(&mut socket, "sibling", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        let slots = within(IO.acquire_many(32)).await.unwrap();
        let (data, mut queued) = mpsc::channel(1);
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hints = Arc::default();
        let owner = case.server.core.shared.histories.clone();
        let mut pending = Box::pin(replay(
            &owner,
            &data,
            &active,
            &hints,
            "waiting",
            "agent/fixture-agent",
            None,
        ));
        assert!(pending.as_mut().now_or_never().is_none());
        Arc::get_mut(&mut case.server.core).unwrap().service(
            &case.registry,
            &mut case.workers,
            case.server._root.path(),
        );
        assert!(
            pending.as_mut().now_or_never().is_none(),
            "capacity exhaustion must wait, not close the socket"
        );
        drop(pending);
        assert!(queued.try_recv().is_err());
        drop(slots);
        case.stored.append(record(2));
        assert_eq!(case.frame(&mut socket).await["seq"], 2);
        case.sub(&mut socket, "after-capacity", Some(1)).await;
        assert_eq!(case.frame(&mut socket).await["seq"], 2);
        assert_eq!(case.frame(&mut socket).await["t"], "synced");
        case.server.finish().await;
    });
}

#[test]
fn explicit_revocation_ends_all_live_cuts_for_the_principal() {
    runtime(async {
        let mut case = Case::new(vec![]).await;
        let mut socket = case.open().await;
        for id in ["one", "two"] {
            case.sub(&mut socket, id, None).await;
            assert_eq!(case.frame(&mut socket).await["t"], "synced");
        }
        let session = case
            .server
            .core
            .shared
            .clients
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .session
            .clone();
        case.server.core.revoke(&session);
        assert!(matches!(
            socket.receive().await,
            Received::Close(AUTH_REVOKED_CLOSE)
        ));
        case.stored.append(record(1));
        case.server.finish().await;
    });
}
