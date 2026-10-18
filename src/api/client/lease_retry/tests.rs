use super::*;
use serde_json::{json, Value};
use std::{cell::Cell, sync::Mutex};
#[derive(Debug)]
struct ClockLog(Mutex<Vec<Duration>>, Instant, Mutex<Duration>);
impl Default for ClockLog {
    fn default() -> Self {
        Self(
            Mutex::new(Vec::new()),
            Instant::now(),
            Mutex::new(Duration::ZERO),
        )
    }
}
impl Clock for ClockLog {
    fn now(&self) -> Instant {
        self.1 + *self.2.lock().unwrap()
    }
    fn wait(&self, duration: Duration) {
        self.0.lock().unwrap().push(duration);
        *self.2.lock().unwrap() += duration;
    }
}
#[test]
fn only_scoped_busy_admission_retries_and_final_refusal_is_typed() {
    for (scoped, code, expected) in [
        (true, "lease_busy", 3),
        (false, "lease_busy", 1),
        (true, "not_authorized", 1),
        (true, "lease_ancestry_limit", 1),
    ] {
        let clock = ClockLog::default();
        let calls = Cell::new(0);
        let result = attempts(
            scoped,
            &clock,
            None,
            || {
                calls.set(calls.get() + 1);
                Ok::<_, ()>(json!({"error":{"code":code,"message":"refused"}}))
            },
            |v| v,
        )
        .unwrap();
        assert_eq!(calls.get(), expected);
        assert_eq!(
            clock.0.lock().unwrap().as_slice(),
            vec![LEASE_BACKOFF; expected - 1]
        );
        assert_eq!(result["error"]["code"], code);
    }
}
#[test]
fn admitted_result_and_ambiguous_transport_failure_never_retry() {
    let clock = ClockLog::default();
    let calls = Cell::new(0);
    let result = attempts(
        true,
        &clock,
        None,
        || {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(if calls.get() == 1 {
                json!({"error":{"code":"lease_busy"}})
            } else {
                json!({"result":{}})
            })
        },
        |v| v,
    )
    .unwrap();
    assert!(result.get("result").is_some());
    assert_eq!(calls.get(), 2);
    assert_eq!(clock.0.lock().unwrap().len(), 1);
    let result: Result<Value, _> = attempts(true, &clock, None, || Err("ambiguous write"), |v| v);
    assert_eq!(result.unwrap_err(), "ambiguous write");
    assert_eq!(clock.0.lock().unwrap().len(), 1);
}

#[test]
fn scoped_calls_timeout_calls_transport_retry_and_subscription_share_three_attempt_budget() {
    use crate::api::{
        client::ApiClient,
        schema::{Method, Request},
    };
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
        sync::Arc,
        time::Instant,
    };
    let root = crate::test_support::TempDir::socket();
    let path = root.path().join("a");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let ceiling = Instant::now() + Duration::from_secs(5);
        for _ in 0..12 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < ceiling,
                            "scoped admission producer stayed silent"
                        );
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line = String::new();
            assert!(BufReader::new(&stream).read_line(&mut line).unwrap() > 0);
            let request: Value = serde_json::from_str(&line).unwrap();
            assert!(request["credential"].is_string());
            assert!(request.get("method").is_none());
            writeln!(stream, "{}", json!({"id":request["request"]["id"],"error":{"code":"lease_busy","message":"refused"}})).unwrap();
        }
        listener
    });
    let secret = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [42; 32]);
    let credential =
        crate::structured::lease::LeaseCredential::from_env_bytes(secret.into_bytes()).unwrap();
    let clock = Arc::new(ClockLog::default());
    let mut client = ApiClient::scoped(path, credential);
    client.retry_clock = clock.clone();
    let request = Request {
        id: "busy".into(),
        method: Method::AgentList(Default::default()),
    };
    let responses = [
        client.request_value(&request).unwrap(),
        client
            .request_value_with_timeout(&request, Duration::from_secs(1))
            .unwrap(),
        client
            .request_value_retrying(&request, Duration::from_secs(1), None)
            .unwrap(),
        client
            .subscribe_value(
                &Request {
                    id: "sub".into(),
                    method: Method::EventsSubscribe(crate::api::schema::EventsSubscribeParams {
                        after: None,
                        subscriptions: vec![],
                    }),
                },
                None,
            )
            .unwrap()
            .0,
    ];
    for response in responses {
        assert_eq!(response["error"]["code"], "lease_busy");
    }
    assert_eq!(clock.0.lock().unwrap().as_slice(), &[LEASE_BACKOFF; 8]);
    let listener = server.join().unwrap();
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn a_caller_deadline_cannot_be_extended_by_a_busy_retry() {
    let clock = ClockLog::default();
    let calls = Cell::new(0);
    let result = attempts(
        true,
        &clock,
        Some(clock.now()),
        || {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(json!({"error":{"code":"lease_busy"}}))
        },
        |v| v,
    )
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert!(clock.0.lock().unwrap().is_empty());
    assert_eq!(result["error"]["code"], "lease_busy");
}

#[test]
fn advancing_clock_deadline_cuts_off_second_retry() {
    let clock = ClockLog::default();
    let calls = Cell::new(0);
    let start = clock.now();
    let result = attempts(
        true,
        &clock,
        Some(start + Duration::from_millis(40)),
        || {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(json!({"error":{"code":"lease_busy"}}))
        },
        |v| v,
    )
    .unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(clock.now() - start, LEASE_BACKOFF);
    assert_eq!(*clock.0.lock().unwrap(), [LEASE_BACKOFF]);
    assert_eq!(result["error"]["code"], "lease_busy");
}
