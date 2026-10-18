use super::*;
use std::io::{BufRead, BufReader, Write};

// Exercise the real socket credential capture and application dispatcher. Account
// policy classifies the real UID; it never changes accounts or requires privilege.
async fn socket(r: &mut AppQueue, p: AgentSendParams) -> Value {
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    client.set_read_timeout(Some(LIMIT)).unwrap();
    server.set_read_timeout(Some(LIMIT)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let policy = r.app.runtime_config.uid_policy.clone();
    let server = std::thread::spawn(move || {
        crate::api::server::handle_connection(
            server,
            &tx,
            &crate::api::EventHub::default(),
            &Arc::new(std::sync::atomic::AtomicBool::new(true)),
            None,
            &std::sync::Mutex::default(),
            &policy,
        )
        .unwrap();
    });
    let notify = r.notify.clone();
    let (done, response) = mpsc::channel();
    let client = std::thread::spawn(move || {
        let mut client = client;
        writeln!(
            client,
            "{}",
            serde_json::to_string(&Request {
                id: "security-send".into(),
                method: Method::AgentSend(p),
            })
            .unwrap()
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let result = serde_json::from_str::<Value>(&line).unwrap();
        done.send(result).unwrap();
        notify.notify_one();
    });
    let message = tokio::time::timeout(LIMIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.peer_pid, Some(std::process::id()));
    r.app.handle_api_request_message(message);
    let mut answer = None;
    r.until(|_| {
        answer = response.try_recv().ok();
        answer.is_some()
    })
    .await;
    server.join().unwrap();
    client.join().unwrap();
    answer.unwrap()
}
fn operator(r: &mut AppQueue) {
    r.app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
}
fn pane(r: &mut AppQueue, name: &str) -> String {
    r.app.state.workspaces = vec![crate::workspace::Workspace::test_new("send-security")];
    r.app.state.ensure_test_terminals();
    let pane = r.app.state.workspaces[0].focused_pane_id().unwrap();
    let terminal = r.app.state.workspaces[0]
        .pane_state(pane)
        .unwrap()
        .attached_terminal_id
        .clone();
    let state = r.app.state.terminals.get_mut(&terminal).unwrap();
    state.launch_argv = Some(vec!["claude".into()]);
    state.agent_name = Some(name.into());
    let id = state.agent_id.to_string();
    // Existing ancestry fixture seam binds the kernel-attested socket process.
    r.app.test_pane_child_pids.insert(pane, std::process::id());
    id
}
#[tokio::test]
async fn send_security_socket_operator_command() {
    let mut r = AppQueue::new(1201, |_| max_policy()).await;
    ready(&mut r).await;
    operator(&mut r);
    let p = params(&r.case.id, 1201, "/compact");
    assert_eq!(socket(&mut r, p).await["result"]["item_id"], id(1201));
    r.until(|r| !inputs(r).is_empty()).await;
    assert_eq!(inputs(&r)[0]["executed_command"], true);
    r.stop().await;
}
#[tokio::test]
async fn send_security_socket_agent_slash_is_framed() {
    let mut r = AppQueue::new(1202, |_| max_policy()).await;
    ready(&mut r).await;
    operator(&mut r);
    let agent = "sender.example".to_string();
    let mut lease = r
        .app
        .session_leases
        .reserve(&agent, "caller-fixture", None)
        .unwrap();
    lease.bind(std::process::id()).unwrap();
    let p = params(&r.case.id, 1202, "/compact");
    assert_eq!(socket(&mut r, p).await["result"]["item_id"], id(1202));
    r.until(|r| !inputs(r).is_empty()).await;
    let input = inputs(&r).remove(0);
    assert_eq!(input["executed_command"], false);
    assert!(input["input"]["message"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with(&format!("From {}", json!({"agent":agent}))));
    r.stop().await;
}
#[tokio::test]
async fn send_security_socket_relay_cannot_be_operator() {
    let mut r = AppQueue::new(1203, |_| max_policy()).await;
    ready(&mut r).await;
    operator(&mut r);
    let pid = std::process::id();
    r.app
        .inbound
        .attach(
            pid,
            crate::platform::process_start_time(pid).unwrap(),
            crate::platform::process_start_time,
        )
        .unwrap();
    r.app
        .inbound
        .enroll(
            pid,
            crate::mesh::hello::Enrollment {
                peer: "relay.example".into(),
                source: crate::mesh::store::PinSource::Inbound,
                pin_origin: Default::default(),
                node_id: Some("verified-node".into()),
                state: "pinned".into(),
                reason: None,
            },
            crate::platform::process_start_time,
        )
        .unwrap();
    let before = disk(&r);
    let p = params(&r.case.id, 1203, "/compact");
    let response = socket(&mut r, p).await;
    assert_eq!(response["error"]["code"], "not_authorized", "{response}");
    assert_eq!(disk(&r), before);
    assert!(inputs(&r).is_empty());
    r.stop().await;
}
#[tokio::test]
async fn send_security_socket_unknown_uid_is_refused() {
    let mut r = AppQueue::new(1204, |_| max_policy()).await;
    ready(&mut r).await;
    r.app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid().wrapping_add(1),
        dedicated: Ok(None).into(),
    };
    let before = disk(&r);
    let p = params(&r.case.id, 1204, "/compact");
    assert_eq!(socket(&mut r, p).await["error"]["code"], "not_authorized");
    assert_eq!(disk(&r), before);
    assert!(inputs(&r).is_empty());
    r.stop().await;
}
#[tokio::test]
async fn send_security_socket_id_and_name_collisions_are_ambiguous() {
    for exact in [false, true] {
        let mut r = AppQueue::new(if exact { 1205 } else { 1206 }, |_| max_policy()).await;
        ready(&mut r).await;
        operator(&mut r);
        let target = if exact {
            r.case.id.clone()
        } else {
            "worker-1206".into()
        };
        pane(&mut r, &target);
        r.app.test_pane_child_pids.clear();
        let p = params(&target, 1205, "collision");
        assert_eq!(
            socket(&mut r, p).await["error"]["code"],
            "agent_target_ambiguous"
        );
        r.stop().await;
    }
}
#[tokio::test]
async fn send_security_socket_self_is_refused() {
    let mut r = AppQueue::new(1207, |_| max_policy()).await;
    ready(&mut r).await;
    operator(&mut r);
    let mut lease = r
        .app
        .session_leases
        .reserve(&r.case.id, "self-fixture", None)
        .unwrap();
    lease.bind(std::process::id()).unwrap();
    let before = disk(&r);
    let p = params("self", 1207, "recursive turn");
    assert_eq!(
        socket(&mut r, p).await["error"]["code"],
        "self_message_forbidden"
    );
    assert_eq!(disk(&r), before);
    r.stop().await;
}

#[tokio::test]
async fn send_security_socket_terminal_fallback_keeps_legacy_option_validation() {
    let mut r = AppQueue::new(1208, |_| max_policy()).await;
    ready(&mut r).await;
    operator(&mut r);
    pane(&mut r, "terminal-only");
    r.app.test_pane_child_pids.clear();
    let mut p = params("terminal-only", 1208, "terminal turn");
    p.write_timeout_ms = Some(json!(100));
    let response = socket(&mut r, p).await;
    assert_eq!(response["error"]["code"], "invalid_params");
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("write_timeout_ms"));
    assert!(inputs(&r).is_empty());
    r.stop().await;
}
