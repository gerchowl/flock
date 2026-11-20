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
