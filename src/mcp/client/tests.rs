use super::*;
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
fn credential_is_removed_before_any_endpoint_read_even_when_configuration_refuses() {
    let removed = std::cell::Cell::new(false);
    let reads = std::cell::Cell::new(0);
    let result = capture_using(
        true,
        |name| {
            reads.set(reads.get() + 1);
            if name == CREDENTIAL_ENV {
                assert!(!removed.get());
                Some("invalid-secret".into())
            } else {
                assert!(
                    removed.get(),
                    "credential still present when startup proceeds"
                );
                None
            }
        },
        || removed.set(true),
    );
    assert!(result.is_err());
    assert!(removed.get());
    assert_eq!(reads.get(), 2);
}

#[global_allocator]
static ALLOCATOR: SecretAudit = SecretAudit;
struct SecretAudit;
thread_local! {
    static AUDIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LEAKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
// SAFETY: System owns every allocation. Zero-initializing capacity also makes
// inspection of unused bytes valid. Inspection precedes deallocation and never
// allocates itself. This allocator exists only in the test binary.
unsafe impl std::alloc::GlobalAlloc for SecretAudit {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // The audited startup path's matching allocations are encoded UTF-8
        // byte buffers. Exclude object layouts and inspect only their live bytes.
        if layout.size() == 44
            && layout.align() == 1
            && AUDIT.try_with(|active| active.get()).unwrap_or(false)
        {
            // SAFETY: the allocation is still live and contains at least eight bytes.
            if unsafe { std::slice::from_raw_parts(ptr, 8) } == b"KioqKioq" {
                let _ = LEAKED.try_with(|count| count.set(count.get() + 1));
            }
        }
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}
#[test]
fn startup_credential_buffers_are_zeroized_before_deallocation() {
    let secret = base64::engine::general_purpose::STANDARD.encode([42; 32]);
    // Prove the audit catches an ordinary unzeroized buffer before relying on
    // its zero count for the startup path. Black-box ownership keeps it live.
    let control = OsString::from(&secret);
    LEAKED.set(0);
    AUDIT.set(true);
    drop(std::hint::black_box(control));
    AUDIT.set(false);
    assert_eq!(LEAKED.get(), 1, "audit failed to detect its control buffer");
    for structured in [true, false] {
        let raw = OsString::from(&secret);
        let socket = OsString::from("/tmp/fixture.sock");
        LEAKED.set(0);
        AUDIT.set(true);
        let result = std::hint::black_box(configured(structured, Some(raw), Some(socket)));
        assert_eq!(result.is_ok(), structured);
        drop(result);
        AUDIT.set(false);
        assert_eq!(LEAKED.get(), 0, "credential bytes freed before zeroization");
    }
}
