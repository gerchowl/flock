use super::*;
use tokio::sync::oneshot;

#[test]
fn durable_decision_ack_is_replayed_after_ownership_transfer() {
    runtime(async {
        let mut case = Case::new(183180).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        let first = case
            .command(&mut socket, "agent.decide", args.clone())
            .await;
        assert_eq!(first["ok"], true);
        case.until(|r| r.handles().len() == 1).await;
        case = handoff(case).await;
        let mut socket = case.open().await;
        socket.send(json!({"t":"cmd","cmd_id":"command-1","verb":"agent.decide","args":args,"resume_only":true})).await;
        assert_eq!(case.ack(&mut socket).await, first);
        assert_eq!(case.handles().len(), 1);
        let rows =
            fs::read_to_string(case.server._root.path().join("wire/commands.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 2);
        case.stop().await;
    });
}

#[test]
fn lost_journal_refuses_before_permission_effect() {
    runtime(async {
        for replace in [false, true] {
            let mut case = Case::new(183181).await;
            let args = case.prompt().await;
            let mut socket = case.open().await;
            let path = case.server._root.path().join("wire/commands.jsonl");
            if replace {
                fs::rename(&path, path.with_extension("previous")).unwrap();
                fs::write(&path, b"").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            let frame = case.command(&mut socket, "agent.decide", args).await;
            assert_eq!(frame["error"]["code"], "session_unconfirmed");
            assert_eq!(case.handles().len(), 2);
            assert_eq!(refusal_events(&case)[0]["reason"], "session_unconfirmed");
            case.stop().await;
        }
    });
}

async fn enqueue(case: &Case, socket: &mut Socket, id: &str, args: &Value) {
    let before = case.server.core.command_owner.queued_test();
    socket
        .send(json!({"t":"cmd","cmd_id":id,"verb":"agent.decide","args":args,"resume_only":false}))
        .await;
    within(async {
        loop {
            let admitted = case.server.wake.notified();
            tokio::pin!(admitted);
            admitted.as_mut().enable();
            if case.server.core.command_owner.queued_test() > before { return; }
            tokio::select! {
                _ = &mut admitted => (),
                frame = socket.receive() => panic!("request ended before owner admission: {frame:?}"),
            }
        }
    }).await;
}

async fn hold_next_sync(case: &Case) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, started) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let mut barrier = Some((entered, resume));
    case.server
        .core
        .journal
        .configure_test(
            None,
            Some(Box::new(move || {
                if let Some((entered, resume)) = barrier.take() {
                    let _ = entered.send(());
                    resume
                        .recv_timeout(Duration::from_secs(5))
                        .expect("sync barrier was never released");
                }
            })),
        )
        .await;
    (started, release)
}

#[test]
fn joined_credentials_are_rechecked_before_returning_a_durable_outcome() {
    runtime(async {
        let mut case = Case::new(183182).await;
        let args = case.prompt().await;
        // Give a second credential the same principal, retaining its own token id.
        let path = case.server._root.path().join("client-tokens/two.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["principal"] = json!("0123456789abcdef0123456789abcdef");
        fs::write(&path, token.to_string()).unwrap();
        let mut first = case.open().await;
        let mut second = Socket::open(&case.server).await;
        second.hello(1, &"d".repeat(64)).await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut first, "joined", &args).await;
        case.service();
        within(started).await.unwrap();
        enqueue(&case, &mut second, "joined", &args).await;
        case.service();
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["scopes"] = json!(["read"]);
        fs::write(&path, token.to_string()).unwrap();
        release.send(()).unwrap();
        assert_eq!(case.ack(&mut first).await["ok"], true);
        let joined = case.ack(&mut second).await;
        assert_eq!(joined["error"]["code"], "scope");
        case.until(|r| r.handles().len() == 1).await;
        let events = refusal_events(&case);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["token_id"], "two");
        case.stop().await;
    });
}

#[test]
fn in_flight_duplicate_joins_one_durable_outcome() {
    runtime(async {
        let mut case = Case::new(183183).await;
        let args = case.prompt().await;
        let mut first = case.open().await;
        let mut second = case.open().await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut first, "joined", &args).await;
        case.service();
        within(started).await.unwrap();
        enqueue(&case, &mut second, "joined", &args).await;
        case.service();
        release.send(()).unwrap();
        let ack = case.ack(&mut first).await;
        assert_eq!(ack["ok"], true);
        assert_eq!(case.ack(&mut second).await, ack);
        case.until(|r| r.handles().len() == 1).await;
        assert_eq!(
            fs::read_to_string(case.server._root.path().join("wire/commands.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        case.stop().await;
    });
}

#[test]
fn capacity_refusal_is_typed_audited_and_has_no_effect() {
    runtime(async {
        let mut case = Case::new(183184).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        case.server
            .core
            .journal
            .configure_test(Some((0, 0)), None)
            .await;
        let ack = case.command(&mut socket, "agent.decide", args).await;
        assert_eq!(ack["error"]["code"], "session_busy");
        assert_eq!(case.handles().len(), 2);
        let events = refusal_events(&case);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reason"], "session_busy");
        assert_eq!(events[0]["token_id"], "one");
        case.stop().await;
    });
}

#[test]
fn full_writer_queue_refuses_before_permission_effect() {
    runtime(async {
        let mut case = Case::new(183185).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        let (started, release) = hold_next_sync(&case).await;
        let command = |id: String| wire_commands::Command {
            id,
            verb: "agent.decide".into(),
            args: json!({}),
            resume_only: false,
            read_only: false,
        };
        let mut pending = vec![case
            .server
            .core
            .journal
            .claim_test(command("blocker".into()))];
        within(started).await.unwrap();
        for n in 0..32 {
            pending.push(
                case.server
                    .core
                    .journal
                    .claim_test(command(format!("seed-{n}"))),
            );
        }
        let frame = case.command(&mut socket, "agent.decide", args).await;
        assert_eq!(frame["error"]["code"], "session_busy");
        assert_eq!(case.handles().len(), 2);
        assert_eq!(refusal_events(&case)[0]["reason"], "session_busy");
        release.send(()).unwrap();
        for reply in pending {
            within(reply).await;
        }
        case.stop().await;
    });
}

async fn restart(case: Case) -> Case {
    let Case {
        server,
        app,
        worker,
        notify,
        driver,
        next,
    } = case;
    let mut server = server;
    let _root = std::mem::replace(
        &mut server._root,
        crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap(),
    );
    server.finish().await;
    let store = TokenStore::open(_root.path()).unwrap();
    let server = Server::from_store(_root, store, |clock| Arc::new(clock)).await;
    Case {
        server,
        app,
        worker,
        notify,
        driver,
        next,
    }
}

#[test]
fn restart_before_ack_replays_the_first_durable_outcome_on_importer() {
    runtime(async {
        let mut case = Case::new(183186).await;
        let args = case.prompt().await;
        let mut original = case.open().await;
        enqueue(&case, &mut original, "before-ack", &args).await;
        let path = case.server._root.path().join("wire/commands.jsonl");
        let wake = case.server.wake.clone();
        let first = within(async {
            loop {
                let changed = wake.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                case.service();
                let text = fs::read_to_string(&path).unwrap();
                if let Some(row) = text.lines().nth(1) {
                    let row: Value = serde_json::from_str(row).unwrap();
                    if !row["outcome"].is_null() {
                        break row["outcome"].clone();
                    }
                }
                changed.await;
            }
        })
        .await;
        assert_eq!(first["ok"], true);
        case = restart(case).await;
        let mut importer = case.open().await;
        importer.send(json!({"t":"cmd","cmd_id":"before-ack","verb":"agent.decide","args":args,"resume_only":true})).await;
        assert_eq!(case.ack(&mut importer).await, first);
        case.until(|r| r.handles().len() == 1).await;
        case.stop().await;
    });
}

#[test]
fn revoked_after_claim_refuses_without_dispatch() {
    runtime(async {
        let mut case = Case::new(183187).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut socket, "revoked-claim", &args).await;
        case.service();
        within(started).await.unwrap();
        fs::remove_file(case.server._root.path().join("client-tokens/one.token")).unwrap();
        release.send(()).unwrap();
        assert_eq!(case.ack(&mut socket).await["error"]["code"], "auth_revoked");
        assert_eq!(case.handles().len(), 2);
        assert_eq!(refusal_events(&case)[0]["token_id"], "one");
        case.stop().await;
    });
}

async fn handoff(case: Case) -> Case {
    let Case {
        mut server,
        app,
        worker,
        notify,
        driver,
        next,
    } = case;
    let wake = Arc::new(tokio::sync::Notify::new());
    let core = Core::new_shared(
        server._root.path().to_owned(),
        wake.clone(),
        crate::structured::permissions::auth::tokens::server::Owner::default(),
        crate::api::EventHub::default(),
        true,
    )
    .await
    .unwrap();
    assert!(core.shared.commands.full("agent.decide"));
    server.core.freeze_commands().await.unwrap();
    core.acquire_commands().await.unwrap();
    assert!(!core.shared.commands.full("agent.decide"));
    let root = std::mem::replace(
        &mut server._root,
        crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap(),
    );
    server.finish().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bind = listener.local_addr().unwrap();
    let router = core.router();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let server = Server {
        core: Arc::new(core),
        bind,
        task,
        clock: TestClock::new(),
        _root: root,
        wake,
    };
    Case {
        server,
        app,
        worker,
        notify,
        driver,
        next,
    }
}

#[test]
fn in_flight_digest_conflict_refuses_before_original_dispatch() {
    runtime(async {
        let mut case = Case::new(183188).await;
        let mut args = case.prompt().await;
        args["extra"] = json!(-0.0);
        let mut first = case.open().await;
        let mut second = case.open().await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut first, "digest-conflict", &args).await;
        case.service();
        within(started).await.unwrap();
        args["extra"] = json!(0.0);
        enqueue(&case, &mut second, "digest-conflict", &args).await;
        assert_eq!(
            case.ack(&mut second).await["error"]["code"],
            "invalid_params"
        );
        assert_eq!(case.handles().len(), 2);
        release.send(()).unwrap();
        let _ = case.ack(&mut first).await;
        case.stop().await;
    });
}

#[test]
fn stored_revocation_does_not_close_an_authorized_joined_credential() {
    runtime(async {
        let mut case = Case::new(183189).await;
        let args = case.prompt().await;
        let path = case.server._root.path().join("client-tokens/two.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["principal"] = json!("0123456789abcdef0123456789abcdef");
        fs::write(&path, token.to_string()).unwrap();
        let mut first = case.open().await;
        let mut joined = Socket::open(&case.server).await;
        joined.hello(1, &"d".repeat(64)).await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut first, "revoked-original", &args).await;
        case.service();
        within(started).await.unwrap();
        enqueue(&case, &mut joined, "revoked-original", &args).await;
        case.service();
        fs::remove_file(case.server._root.path().join("client-tokens/one.token")).unwrap();
        release.send(()).unwrap();
        let first_ack = case.ack(&mut first).await;
        assert_eq!(first_ack["error"]["code"], "auth_revoked");
        assert_eq!(case.ack(&mut joined).await, first_ack);
        assert_eq!(case.handles().len(), 2);
        joined.send(json!({"t":"ping","nonce":"authorized"})).await;
        assert_eq!(joined.text().await["t"], "pong");
        case.stop().await;
    });
}

async fn slow_outcome_publication(fleet: bool) {
    let mut case = Case::new(if fleet { 183198 } else { 183190 }).await;
    let args = case.prompt().await;
    let mut socket = case.open().await;
    socket.send(json!({"t":"sub","sub_id":"permission-order","stream":if fleet { "fleet".to_owned() } else { format!("agent/{}",case.worker.id) }})).await;
    loop {
        match case.frame(&mut socket).await {
            Received::Text(frame) if frame["t"] == "synced" => break,
            Received::Text(frame) => assert!(matches!(frame["t"].as_str(), Some("ev" | "gap"))),
            other => panic!("bootstrap ended before synced: {other:?}"),
        }
    }
    let (entered, mut started) = oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let mut barrier = Some((entered, resume));
    let mut writes = 0;
    case.server
        .core
        .journal
        .configure_test(
            None,
            Some(Box::new(move || {
                writes += 1;
                if writes == 2 {
                    let (entered, resume) = barrier.take().unwrap();
                    let _ = entered.send(());
                    resume
                        .recv_timeout(Duration::from_secs(5))
                        .expect("outcome sync was not released");
                }
            })),
        )
        .await;
    enqueue(&case, &mut socket, "permission-order", &args).await;
    let owner = case.server.wake.clone();
    let worker = case.notify.clone();
    within(async {
        loop {
            let changed = owner.notified();
            let produced = worker.notified();
            tokio::pin!(changed, produced);
            changed.as_mut().enable();
            produced.as_mut().enable();
            case.service();
            tokio::select! { biased;
                result = &mut started => { result.unwrap(); break; }
                _ = &mut changed => (), _ = &mut produced => (),
            }
        }
    })
    .await;
    within(async {
        loop {
            if case.events().iter().any(|r| {
                r["type"] == "flk_permission_request"
                    && r["request_id"] == "first-request"
                    && r["state"] == "closed"
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    socket
        .send(json!({"t":"ping","nonce":"outcome-sync-held"}))
        .await;
    assert_eq!(
        socket.text().await["t"],
        "pong",
        "caused permission event preceded durable ACK"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), case.frame(&mut socket))
            .await
            .is_err(),
        "permission publication escaped while Finish was held"
    );
    release.send(()).unwrap();
    assert_eq!(case.ack(&mut socket).await["ok"], true);
    let Received::Text(closed) = case.frame(&mut socket).await else {
        panic!("missing permission close");
    };
    if !fleet {
        assert_eq!(closed["ev"]["state"], "closed");
    } else {
        assert_eq!(closed["sub_id"], "permission-order");
    }
    case.stop().await;
}

#[test]
fn durable_ack_precedes_the_permission_event_it_caused() {
    runtime(slow_outcome_publication(false));
}
#[test]
fn durable_ack_precedes_the_fleet_projection_it_caused() {
    runtime(slow_outcome_publication(true));
}

async fn joined_publication(vanish: bool) {
    let mut case = Case::new(if vanish { 183200 } else { 183199 }).await;
    let args = case.prompt().await;
    let mut origin = case.open().await;
    let first_id = *case
        .server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    let mut observer = case.open().await;
    let (id, session) = case
        .server
        .core
        .shared
        .clients
        .lock()
        .unwrap()
        .iter()
        .find(|(id, _)| **id != first_id)
        .map(|(id, client)| (*id, client.session.clone()))
        .unwrap();
    observer.send(json!({"t":"sub","sub_id":"joined-observer","stream":format!("agent/{}",case.worker.id)})).await;
    loop {
        let Received::Text(frame) = case.frame(&mut observer).await else {
            panic!("observer lost");
        };
        if frame["t"] == "synced" {
            break;
        }
    }
    let (started, release) = hold_next_sync(&case).await;
    enqueue(&case, &mut origin, "joined-order", &args).await;
    case.service();
    within(started).await.unwrap();
    let (data, mut received) = mpsc::channel(1);
    let joined = tokio::spawn(wire_commands::execute(
        case.server.core.shared.clone(),
        session,
        data,
        wire_commands::Command {
            id: "joined-order".into(),
            verb: "agent.decide".into(),
            args,
            resume_only: true,
            read_only: false,
        },
        id,
    ));
    within(async {
        loop {
            let changed = case.server.wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if case.server.core.command_owner.queued_test() != 0 {
                break;
            }
            changed.await;
        }
    })
    .await;
    case.service();
    release.send(()).unwrap();
    assert_eq!(case.ack(&mut origin).await["ok"], true);
    let delivery = within(received.recv()).await.unwrap();
    assert!(matches!(delivery.frame, ServerFrame::Ack { ok: true, .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), case.frame(&mut observer))
            .await
            .is_err(),
        "joined ACK guard did not retain the original command's publication"
    );
    if vanish {
        drop(observer);
        within(async {
            loop {
                if !case
                    .server
                    .core
                    .shared
                    .clients
                    .lock()
                    .unwrap()
                    .contains_key(&id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        // Keep the unsent ACK guard alive: origin teardown must release it.
        case.until(|c| c.handles().len() == 1).await;
    } else {
        delivery.delivered.unwrap().send(true).unwrap();
        drop(delivery.causal);
        assert_eq!(within(joined).await.unwrap(), Ok(()));
        let Received::Text(frame) = case.frame(&mut observer).await else {
            panic!("close missing");
        };
        assert_eq!(frame["ev"]["state"], "closed");
        case.stop().await;
        return;
    }
    drop(delivery);
    assert!(within(joined).await.unwrap().is_err());
    case.stop().await;
}
#[test]
fn joined_resume_only_waiter_holds_publication_until_its_ack_flushes() {
    runtime(joined_publication(false));
}
#[test]
fn joined_origin_loss_releases_an_unsent_ack_still_owned_by_the_command() {
    runtime(joined_publication(true));
}

#[test]
fn near_global_capacity_publication_latency_uses_the_real_wire_path() {
    runtime(async {
        let mut case = Case::new(183201).await;
        within(case.server.core.journal.seed_capacity_test()).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        socket
            .send(
                json!({"t":"sub","sub_id":"capacity","stream":format!("agent/{}",case.worker.id)}),
            )
            .await;
        loop {
            let Received::Text(frame) = case.frame(&mut socket).await else {
                panic!("bootstrap lost");
            };
            if frame["t"] == "synced" {
                break;
            }
        }
        let start = Instant::now();
        let ack = case.command(&mut socket, "agent.decide", args).await;
        assert_eq!(ack["ok"], true);
        let acknowledged = Instant::now();
        let Received::Text(frame) = case.frame(&mut socket).await else {
            panic!("close missing");
        };
        assert_eq!(frame["ev"]["state"], "closed");
        eprintln!("capacity real wire claim+dispatch+finish+ACK {:?}; ACK receive to caused publication receive {:?}", acknowledged.duration_since(start), acknowledged.elapsed());
        case.stop().await;
    });
}

#[test]
fn cancellation_after_claim_preserves_the_effect_and_releases_publication() {
    runtime(async {
        let mut case = Case::new(183202).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        let (id, session) = case
            .server
            .core
            .shared
            .clients
            .lock()
            .unwrap()
            .iter()
            .map(|(id, client)| (*id, client.session.clone()))
            .next()
            .unwrap();
        socket.send(json!({"t":"sub","sub_id":"cancelled-command","stream":format!("agent/{}",case.worker.id)})).await;
        loop {
            let Received::Text(frame) = case.frame(&mut socket).await else {
                panic!("bootstrap lost");
            };
            if frame["t"] == "synced" {
                break;
            }
        }
        let (started, release) = hold_next_sync(&case).await;
        let (data, _unused) = mpsc::channel(1);
        let command = tokio::spawn(wire_commands::execute(
            case.server.core.shared.clone(),
            session,
            data,
            wire_commands::Command {
                id: "cancelled".into(),
                verb: "agent.decide".into(),
                args,
                resume_only: false,
                read_only: false,
            },
            id,
        ));
        within(async {
            loop {
                let changed = case.server.wake.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if case.server.core.command_owner.queued_test() != 0 {
                    break;
                }
                changed.await;
            }
        })
        .await;
        case.service();
        within(started).await.unwrap();
        command.abort();
        assert!(within(command).await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        let Received::Text(closed) = case.frame(&mut socket).await else {
            panic!("publication leaked its receipt");
        };
        assert_eq!(closed["ev"]["state"], "closed");
        let rows =
            fs::read_to_string(case.server._root.path().join("wire/commands.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 2);
        socket
            .send(json!({"t":"ping","nonce":"cancellation-kept-origin"}))
            .await;
        assert_eq!(socket.text().await["nonce"], "cancellation-kept-origin");
        case.stop().await;
    });
}

#[test]
fn review_cached_decision_rechecks_removed_operate_scope() {
    runtime(async {
        let mut case = Case::new(183210).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        assert_eq!(
            case.command(&mut socket, "agent.decide", args.clone())
                .await["ok"],
            true
        );
        case.until(|r| r.handles().len() == 1).await;
        case.scopes(json!(["answer", "read"]));
        socket.send(json!({"t":"cmd","cmd_id":"command-1","verb":"agent.decide","args":args,"resume_only":true})).await;
        let frame = case.ack(&mut socket).await;
        assert_eq!(
            frame["ok"], false,
            "cached decision disclosed after operate removal: {frame}"
        );
        case.stop().await;
    });
}

#[test]
fn review_joined_decision_rechecks_removed_operate_scope() {
    runtime(async {
        let mut case = Case::new(183216).await;
        let args = case.prompt().await;
        // Give a second credential the same principal, retaining its own token id.
        let path = case.server._root.path().join("client-tokens/two.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["principal"] = json!("0123456789abcdef0123456789abcdef");
        fs::write(&path, token.to_string()).unwrap();
        let mut first = case.open().await;
        let mut second = Socket::open(&case.server).await;
        second.hello(1, &"d".repeat(64)).await;
        let (started, release) = hold_next_sync(&case).await;
        enqueue(&case, &mut first, "joined", &args).await;
        case.service();
        within(started).await.unwrap();
        enqueue(&case, &mut second, "joined", &args).await;
        case.service();
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["scopes"] = json!(["read", "answer"]);
        fs::write(&path, token.to_string()).unwrap();
        release.send(()).unwrap();
        assert_eq!(case.ack(&mut first).await["ok"], true);
        let joined = case.ack(&mut second).await;
        assert_eq!(joined["error"]["code"], "not_authorized");
        case.until(|r| r.handles().len() == 1).await;
        let events = refusal_events(&case);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["token_id"], "two");
        case.stop().await;
    });
}

async fn enqueue_verb(case: &Case, socket: &mut Socket, id: &str, verb: &str, args: &Value) {
    let before = case.server.core.command_owner.queued_test();
    socket
        .send(json!({"t":"cmd","cmd_id":id,"verb":verb,"args":args,"resume_only":false}))
        .await;
    within(async {
        loop {
            let changed = case.server.wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if case.server.core.command_owner.queued_test() > before {
                break;
            }
            tokio::select! {
                _ = changed => (),
                frame = socket.receive() => panic!("ended before owner admission: {frame:?}"),
            }
        }
    })
    .await;
}

#[test]
fn every_verb_cached_and_joined_outcomes_require_current_fresh_dispatch_scopes() {
    runtime(async {
        for (index, (verb, removed, expected)) in [
            ("agent.send", "send", "scope"),
            ("agent.decide", "answer", "scope"),
            ("agent.decide", "operate", "not_authorized"),
            ("agent.start", "operate", "not_authorized"),
            ("agent.restart", "operate", "not_authorized"),
            ("agent.stop", "operate", "not_authorized"),
        ]
        .into_iter()
        .enumerate()
        {
            for joined in [false, true] {
                let mut case = Case::new(183230 + index as u32 * 2 + u32::from(joined)).await;
                let args = if verb == "agent.decide" {
                    case.prompt().await
                } else {
                    json!({})
                };
                let path = case.server._root.path().join("client-tokens/two.token");
                let mut record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                record["principal"] = json!("0123456789abcdef0123456789abcdef");
                fs::write(&path, record.to_string()).unwrap();
                let mut primary = case.open().await;
                let mut waiter = Socket::open(&case.server).await;
                waiter.hello(1, &"d".repeat(64)).await;
                let scopes: Vec<_> = ["read", "send", "answer", "operate"]
                    .into_iter()
                    .filter(|scope| *scope != removed)
                    .collect();
                if joined {
                    let (started, release) = hold_next_sync(&case).await;
                    enqueue_verb(&case, &mut primary, "scope-matrix", verb, &args).await;
                    case.service();
                    within(started).await.unwrap();
                    enqueue_verb(&case, &mut waiter, "scope-matrix", verb, &args).await;
                    case.service();
                    record["scopes"] = json!(scopes);
                    fs::write(&path, record.to_string()).unwrap();
                    release.send(()).unwrap();
                    let first = case.ack(&mut primary).await;
                    assert_ne!(
                        first["error"]["code"], expected,
                        "full authority was refused for {verb}"
                    );
                } else {
                    enqueue_verb(&case, &mut primary, "scope-matrix", verb, &args).await;
                    let first = case.ack(&mut primary).await;
                    assert_ne!(
                        first["error"]["code"], expected,
                        "full authority was refused for {verb}"
                    );
                    record["scopes"] = json!(scopes);
                    fs::write(&path, record.to_string()).unwrap();
                    waiter.send(json!({"t":"cmd","cmd_id":"scope-matrix","verb":verb,"args":args,"resume_only":true})).await;
                }
                let returned = case.ack(&mut waiter).await;
                waiter.send(json!({"t":"cmd","cmd_id":"fresh-scope-matrix","verb":verb,"args":args,"resume_only":false})).await;
                let fresh = case.ack(&mut waiter).await;
                assert_eq!(
                    fresh["error"]["code"], expected,
                    "fresh {verb} without {removed}"
                );
                assert_eq!(
                    returned["error"]["code"], fresh["error"]["code"],
                    "{verb} joined={joined} returned an outcome without current {removed}"
                );
                if verb == "agent.decide" {
                    case.until(|c| c.handles().len() == 1).await;
                }
                case.stop().await;
            }
        }
    });
}
