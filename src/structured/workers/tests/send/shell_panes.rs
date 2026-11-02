//! Real PTY children enter through kernel-authenticated socket dispatch.
use super::*;
use std::sync::atomic::AtomicBool;

async fn shell_request(
    r: &mut AppQueue,
    nested: bool,
    custom_runtime: bool,
) -> (
    crate::layout::PaneId,
    crate::api::ApiRequestMessage,
    std::thread::JoinHandle<()>,
) {
    r.app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    r.app.state.workspaces = vec![crate::workspace::Workspace::test_new("shell-send")];
    r.app.state.ensure_test_terminals();
    let pane = r.app.state.workspaces[0].focused_pane_id().unwrap();
    let state = r.app.state.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane)
        .unwrap();
    state.created_as_root_shell = true;
    let terminal = state.attached_terminal_id.clone();
    let runtime = if custom_runtime {
        crate::terminal::TerminalRuntime::spawn_shell_command(
            pane,
            24,
            100,
            r.case.fixture.root.path().to_path_buf(),
            "while IFS= read -r command; do eval \"$command\"; done",
            &[],
            4096,
            Default::default(),
            r.app.event_tx.clone(),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
    } else {
        crate::terminal::TerminalRuntime::spawn(
            pane,
            24,
            100,
            r.case.fixture.root.path().to_path_buf(),
            4096,
            Default::default(),
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            r.app.event_tx.clone(),
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap()
    };
    let socket = r.case.fixture.root.path().join("shell.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let request = serde_json::to_string(&Request {
        id: "shell-send".into(),
        method: Method::AgentSend(params(&r.case.id, 2494, "pane turn")),
    })
    .unwrap();
    let script = r.case.fixture.root.path().join("client.py");
    // JSON string literals are also Python string literals for these ASCII fixtures.
    fs::write(&script, format!(
        "import socket\ns=socket.socket(socket.AF_UNIX)\ns.settimeout(10)\ns.connect({})\ns.sendall(({}+'\\n').encode())\ns.recv(65536)\n",
        json!(socket.to_str().unwrap()), json!(request),
    )).unwrap();
    let command = format!("python3 '{}'", script.display());
    let command = if nested {
        format!("/bin/sh -c \"{command}; :\"")
    } else {
        command
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let policy = r.app.runtime_config.uid_policy.clone();
    let server = std::thread::spawn(move || {
        let mut poll = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor belongs to the live listener.
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 10_000) },
            1,
            "client never connected"
        );
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        // A negative test may terminate its owned client before the refusal is written.
        let _ = crate::api::server::handle_connection(
            stream,
            &tx,
            &crate::api::EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
            &std::sync::Mutex::default(),
            &crate::api::client_listener::Health::default(),
            &policy,
        );
    });
    runtime
        .send_bytes(format!("{command}\n").into())
        .await
        .unwrap();
    r.app.terminal_runtimes.insert(terminal, runtime);
    let message = tokio::time::timeout(LIMIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    let caller = message.peer_pid.unwrap();
    let root = r.app.pane_child_pid(pane).unwrap();
    assert_eq!(
        crate::platform::process_parent_id(caller) == Some(root),
        !nested
    );
    assert_eq!(
        message.peer_uid,
        Some(crate::platform::peer::effective_uid())
    );
    (pane, message, server)
}

async fn dispatch(
    r: &mut AppQueue,
    mut message: crate::api::ApiRequestMessage,
    server: std::thread::JoinHandle<()>,
) -> Value {
    let wire = message.respond_to;
    let (tx, rx) = mpsc::channel();
    message.respond_to = tx;
    r.app.handle_api_request_message(message);
    let answer = ack(r, &rx).await;
    let _ = wire.send(answer.to_string());
    tokio::time::timeout(
        LIMIT,
        tokio::task::spawn_blocking(move || server.join().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    answer
}

#[tokio::test]
async fn shell2494_prompt_child_is_stamped_in_wire_and_durable_history() {
    let mut r = AppQueue::new(2494, |_| max_policy()).await;
    ready(&mut r).await;
    let (pane, message, server) = shell_request(&mut r, false, false).await;
    assert_eq!(
        dispatch(&mut r, message, server).await["result"]["item_id"],
        id(2494)
    );
    r.until(|r| !inputs(r).is_empty()).await;
    let stamp = format!("operator via pane {}", pane.raw());
    let input = inputs(&r).remove(0);
    assert!(input["input"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(&stamp));
    let history = String::from_utf8(disk(&r)).unwrap();
    assert!(
        history.contains(&format!("\"operator_via_pane\":{}", pane.raw())),
        "{history}"
    );
    let history = r
        .app
        .structured_workers
        .history(&r.app.state.structured_agents, &r.case.id)
        .unwrap();
    let mut cut = history.subscribe(None).unwrap();
    r.until(|_| loop {
        match cut.page().unwrap() {
            crate::structured::history::Item::Record(record) => {
                if record.raw.contains(&stamp) {
                    return true;
                }
            }
            crate::structured::history::Item::Synced(_)
            | crate::structured::history::Item::Progress => {}
            crate::structured::history::Item::Pending => return false,
            other => panic!("unexpected history item before stamped input: {other:?}"),
        }
    })
    .await;
    r.stop().await;
}

#[tokio::test]
async fn shell2494_grandchild_is_not_an_operator() {
    let mut r = AppQueue::new(2495, |_| max_policy()).await;
    ready(&mut r).await;
    let (_, message, server) = shell_request(&mut r, true, false).await;
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    assert!(inputs(&r).is_empty());
    r.stop().await;
}

#[tokio::test]
async fn shell2494_agent_pane_is_not_an_operator() {
    let mut r = AppQueue::new(2496, |_| max_policy()).await;
    ready(&mut r).await;
    let (pane, message, server) = shell_request(&mut r, false, false).await;
    let terminal = r.app.state.terminal_id_for_pane(0, pane).unwrap();
    r.app
        .state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .set_agent_name("agent-fixture".into())
        .unwrap();
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    assert!(inputs(&r).is_empty());
    r.stop().await;
}

#[tokio::test]
async fn shell2494_unknown_provenance_is_refused() {
    let mut r = AppQueue::new(2497, |_| max_policy()).await;
    ready(&mut r).await;
    let (pane, message, server) = shell_request(&mut r, false, false).await;
    r.app.state.workspaces[0].tabs[0]
        .panes
        .get_mut(&pane)
        .unwrap()
        .created_as_root_shell = false;
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    r.stop().await;
}

#[tokio::test]
async fn shell2494_exited_root_is_refused() {
    let mut r = AppQueue::new(2498, |_| max_policy()).await;
    ready(&mut r).await;
    let (pane, message, server) = shell_request(&mut r, false, false).await;
    let terminal = r.app.state.terminal_id_for_pane(0, pane).unwrap();
    r.app
        .terminal_runtimes
        .remove(&terminal)
        .unwrap()
        .shutdown();
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    r.stop().await;
}

#[tokio::test]
async fn shell2494_mismatched_uid_is_refused() {
    let mut r = AppQueue::new(2499, |_| max_policy()).await;
    ready(&mut r).await;
    let (_, message, server) = shell_request(&mut r, false, false).await;
    r.app.runtime_config.uid_policy.operator = message.peer_uid.unwrap().wrapping_add(1);
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    r.stop().await;
}

#[tokio::test]
async fn shell2494_nondefault_runtime_is_refused() {
    let mut r = AppQueue::new(2500, |_| max_policy()).await;
    ready(&mut r).await;
    let (pane, message, server) = shell_request(&mut r, false, true).await;
    let terminal = r.app.state.terminal_id_for_pane(0, pane).unwrap();
    assert!(!r
        .app
        .terminal_runtimes
        .get(&terminal)
        .unwrap()
        .was_spawned_with_default_shell());
    assert_eq!(
        dispatch(&mut r, message, server).await["error"]["code"],
        "not_authorized"
    );
    r.stop().await;
}
