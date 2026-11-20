use super::*;

async fn stalled_open(deferred: bool) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let data = root.path().to_owned();
    let wake = Arc::new(tokio::sync::Notify::new());
    let events = crate::api::EventHub::default();
    let (started, release) =
        crate::web::wire::commands::Ownership::test_stall_next_open(data.clone());
    let mut caller = tokio::spawn({
        let wake = wake.clone();
        let events = events.clone();
        async move {
            let core = Core::new_shared(data, wake, Owner::default(), events, deferred)
                .await
                .unwrap();
            if deferred {
                core.acquire_commands().await.unwrap();
            }
            core
        }
    });
    within(started).await.unwrap();
    let early = tokio::time::timeout(Duration::from_secs(2), &mut caller).await;
    let bounded = early.is_ok();
    if !bounded {
        release.send(()).unwrap();
        let core = within(caller).await.unwrap();
        core.shutdown().await.unwrap();
        panic!("startup/importer waited indefinitely for journal open");
    }
    let core = early.unwrap().unwrap();
    assert!(core.shared.commands.full("agent.decide"));
    let token = root.path().join("client-tokens/probe.token");
    fs::write(
        &token,
        json!({"token":"a".repeat(64),"principal":"c".repeat(32),"scopes":["read","answer"]})
            .to_string(),
    )
    .unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
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
        wake: wake.clone(),
    };
    let mut socket = Socket::open(&server).await;
    socket.hello(1, &"a".repeat(64)).await;
    socket.send(json!({"t":"cmd","cmd_id":"unavailable","verb":"agent.decide","args":{},"resume_only":false})).await;
    let ack = socket.text().await;
    assert_eq!(ack["error"]["code"], "session_busy");
    let audit = events.events_after(0).unwrap();
    assert!(audit
        .iter()
        .any(|(_, event)| serde_json::to_value(event).unwrap()["event"] == "wire_refused"));
    release.send(()).unwrap();
    within(async {
        loop {
            let changed = wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !server.core.shared.commands.full("agent.decide") {
                break;
            }
            changed.await;
        }
    })
    .await;
    server.finish().await;
}
#[test]
fn stalled_startup_runs_with_audited_wire_refusal_then_recovers() {
    runtime(stalled_open(false));
}
#[test]
fn stalled_importer_after_commit_runs_with_audited_wire_refusal_then_recovers() {
    runtime(stalled_open(true));
}
