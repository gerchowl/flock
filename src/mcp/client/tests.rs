use super::*;

fn configured(
    structured: bool,
    credential: Option<OsString>,
    socket: Option<OsString>,
) -> io::Result<ApiClient> {
    super::configured(
        structured,
        credential.map(|raw| zeroize::Zeroizing::new(raw.into_encoded_bytes())),
        socket,
    )
}
use crate::{
    api::schema::{EmptyParams, Method},
    mcp::{
        bridge::{FlockCall, LocalApi},
        channel,
    },
};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn structured_configuration_never_downgrades_or_echoes_secrets() {
    let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
    for (mode, credential, socket) in [
        (true, None, Some("/tmp/fixture.sock")),
        (
            true,
            Some("sensitive-invalid-credential"),
            Some("/tmp/fixture.sock"),
        ),
        (true, Some(secret.as_str()), None),
        (true, Some(secret.as_str()), Some("relative.sock")),
        (false, Some(secret.as_str()), Some("/tmp/fixture.sock")),
    ] {
        let error =
            configured(mode, credential.map(Into::into), socket.map(Into::into)).unwrap_err();
        assert!(!error.to_string().contains(&secret));
        assert!(!format!("{error:?}").contains("sensitive-invalid-credential"));
    }
    let client = configured(
        true,
        Some(secret.clone().into()),
        Some("/tmp/fixture.sock".into()),
    )
    .unwrap();
    assert!(!format!("{client:?}").contains(&secret));
}

#[test]
fn local_calls_compatibility_probes_and_channel_reattachment_share_scoping() {
    let (done, completed) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let root = crate::test_support::TempDir::socket();
        let socket = root.path().join("a");
        let listener = UnixListener::bind(&socket).unwrap();
        let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
        let client = configured(
            true,
            Some(secret.clone().into()),
            Some(socket.into_os_string()),
        )
        .unwrap();
        let peer = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for i in 0..7 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let value: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(value["credential"], secret);
                assert!(value.get("method").is_none());
                let method = value["request"]["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                let response = match i {
                    0 => json!({"id":"c","error":{"code":"not_authorized","message":"refused"}}),
                    1 => {
                        json!({"id":"c","error":{"code":"unknown_method","message":"unknown method"}})
                    }
                    2 => {
                        json!({"id":"c","result":{"type":"pong","protocol":crate::protocol::PROTOCOL_VERSION,"version":env!("CARGO_PKG_VERSION")}})
                    }
                    3 | 5 => json!({"id":"c","result":{"pane":"fixture-pane"}}),
                    _ => json!({"id":"c","result":{}}),
                };
                writeln!(stream, "{response}").unwrap();
            }
            methods
        });
        let refusal = LocalApi(client.clone())
            .call(Method::AgentList(EmptyParams::default()))
            .unwrap();
        assert_eq!(refusal["error"]["code"], "not_authorized");
        LocalApi(client.clone())
            .call(Method::AgentList(EmptyParams::default()))
            .unwrap();
        let out: channel::SharedOut = Arc::new(Mutex::new(Vec::<u8>::new()));
        let opts = channel::ChannelOptions::off();
        for _ in 0..2 {
            assert!(channel::run_feed(&out, &opts, &client).is_err());
        }
        assert_eq!(
            peer.join().unwrap(),
            [
                "agent.list",
                "agent.list",
                "ping",
                "msg.wake",
                "events.subscribe",
                "msg.wake",
                "events.subscribe"
            ]
        );
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(5))
        .expect("transport fixture must complete within fail ceiling");
}

#[test]
fn credential_take_precedes_endpoint_lookup_on_configuration_refusal() {
    let taken = std::cell::Cell::new(false);
    let result = capture_using(
        true,
        || {
            taken.set(true);
            Ok(Some(zeroize::Zeroizing::new(b"invalid-secret".to_vec())))
        },
        || {
            assert!(
                taken.get(),
                "credential take did not run before endpoint lookup"
            );
            None
        },
    );
    assert!(result.is_err());
    assert!(taken.get());
}
