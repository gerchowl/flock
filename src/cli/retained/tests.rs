use super::*;
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}
#[test]
fn retained_cli_requires_exclusive_selection_and_all_confirmation() {
    assert!(matches!(
        parse(&args(&["list"])),
        Some(Method::SessionRetainedList(_))
    ));
    assert!(matches!(
        parse(&args(&["purge", "--dead"])),
        Some(Method::SessionRetainedPurge(_))
    ));
    assert!(matches!(
        parse(&args(&["purge", "--all", "--yes", "--older-than", "24h"])),
        Some(Method::SessionRetainedPurge(PurgeParams {
            older_than_ms: Some(86_400_000),
            ..
        }))
    ));
    for raw in [
        &["purge"][..],
        &["purge", "--all"],
        &["purge", "--dead", "--all", "--yes"],
        &["purge", "--dead", "--dead"],
        &["purge", "--dead", "--older-than"],
        &["purge", "--dead", "--older-than", "bad"],
        &["list", "--all"],
        &["purge", "--all", "--yes", "--yes"],
    ] {
        assert!(parse(&args(raw)).is_none(), "{raw:?}");
    }
}
#[test]
fn retained_doctor_text_reports_both_kinds_and_the_operator_purge_hint() {
    let result = ResponseResult::Retained {
        records: vec![],
        forward: 2,
        dead: 129,
        removed: 0,
        purge_hint: Some("flk session retained purge --dead".into()),
    };
    let message = text(&result, true).unwrap();
    assert!(message.contains("FORWARD 2, DEAD 129"));
    assert!(message.contains("flk session retained purge --dead"));
    assert!(!message.contains("slot="));
}

#[tokio::test]
async fn retained_doctor_socket_round_trip_reports_the_actual_registry() {
    use crate::api::client::{ApiClient, ConnectionTarget};
    use std::{
        os::{fd::AsRawFd, unix::net::UnixListener},
        sync::{atomic::AtomicBool, Arc, Mutex},
        time::Duration,
    };
    let root = crate::test_support::TempDir::socket();
    let socket = root.path().join("api.sock");
    assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = crate::app::App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    );
    app.runtime_config.uid_policy = crate::structured::identity::UidPolicy {
        operator: crate::platform::peer::effective_uid(),
        dedicated: Ok(None).into(),
    };
    let policy = app.runtime_config.uid_policy.clone();
    app.state.structured_agents = serde_json::from_value(
        serde_json::json!([null,{"harness":"future-one"},{"harness":"future-two"}]),
    )
    .unwrap();
    let server = std::thread::spawn(move || {
        let mut poll = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the initialized pollfd borrows this thread's listener.
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 10_000) },
            1,
            "bounded doctor socket accept"
        );
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        crate::api::server::handle_connection(
            stream,
            &tx,
            &crate::api::EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
            &Mutex::default(),
            &policy,
        )
        .unwrap();
    });
    let method = select(&[], true).unwrap();
    let client = std::thread::spawn(move || {
        ApiClient::for_target(ConnectionTarget::SocketPath(socket))
            .request_value_with_timeout(
                &Request {
                    id: "cli:retained".into(),
                    method,
                },
                Duration::from_secs(10),
            )
            .unwrap()
    });
    let msg = tokio::time::timeout(Duration::from_secs(10), app.api_rx.recv())
        .await
        .expect("bounded doctor dispatch")
        .expect("doctor producer closed");
    app.current_api_peer_pid = msg.peer_pid;
    app.current_api_peer_birth = Some(msg.peer_birth);
    app.current_api_peer_uid = Some(msg.peer_uid);
    msg.respond_to
        .send(app.handle_api_request(msg.request))
        .unwrap();
    let response = client.join().unwrap();
    server.join().unwrap();
    let result: ResponseResult = serde_json::from_value(response["result"].clone()).unwrap();
    let rendered = text(&result, true).unwrap();
    assert!(rendered.contains("FORWARD 2, DEAD 1"), "{rendered}");
    assert!(
        rendered.contains("flk session retained purge"),
        "{rendered}"
    );
}

#[test]
fn retained_methods_pin_read_and_mutation_retry_boundaries() {
    let list = select(&[], true).unwrap();
    assert!(list.is_retry_safe());
    assert!(!list.needs_current_protocol());
    let purge = parse(&args(&["purge", "--dead"])).unwrap();
    assert!(!purge.is_retry_safe());
    assert!(purge.needs_current_protocol());
}
