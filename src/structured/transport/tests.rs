use super::*;
use crate::structured::launch::SessionConfigDir;
use std::fs;
use std::os::fd::FromRawFd;
use std::time::Instant;

fn pipe() -> (File, File) {
    let mut fds = [-1; 2];
    // SAFETY: pipe writes exactly two descriptor integers to this array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: successful pipe created two owned, distinct descriptors.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn wait<T>(transport: &Transport, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match transport.completion.recv_timeout(POLL.min(remaining)) {
            Ok(completion) => {
                if matches!(&completion, Completion::Reader(Err(error)) | Completion::Writer(Err(error)) if error.kind() == io::ErrorKind::Interrupted)
                {
                    let peer = transport
                        .completion
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()));
                    panic!("worker exited before condition: {completion:?}; peer: {peer:?}");
                }
                panic!("worker exited before condition: {completion:?}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(!remaining.is_zero(), "condition deadline with live workers")
            }
            Err(error) => panic!("workers disconnected before condition: {error}"),
        }
    }
}

fn receive(transport: &Transport) -> Received {
    transport
        .output
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| {
            panic!(
                "missing record: {error}; worker: {:?}",
                transport.completion.try_recv()
            )
        })
}

fn accepted(transport: &Transport) {
    let fact = receive(transport);
    assert!(matches!(
        fact.state_input(),
        Some(crate::structured::state::Input::Accepted { .. })
    ));
    assert_eq!(fact.record.origin, RecordOrigin::Flk);
}

fn receive_after_accepted(transport: &Transport) -> Received {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let received = transport
            .output
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                let completion = transport
                    .completion
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()));
                panic!("missing stdout record: {error}; worker: {completion:?}")
            });
        if !matches!(
            received.state_input(),
            Some(crate::structured::state::Input::Accepted { .. })
        ) {
            return received;
        }
    }
}

fn worker_done(transport: &Transport, reader: bool) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let completion = transport
            .completion
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("worker completion deadline");
        let (is_reader, result) = match completion {
            Completion::Reader(r) => (true, r),
            Completion::Writer(r) => (false, r),
        };
        if is_reader == reader {
            return result;
        }
        if let Err(error) = result {
            if error.kind() != io::ErrorKind::Interrupted {
                panic!("other worker failed: {error}");
            }
        }
    }
}

fn location(root: &SessionConfigDir) -> StreamLocation {
    StreamLocation {
        data_dir: root.path().to_owned(),
        storage: Default::default(),
        agent: "fixture-agent".into(),
        session: "fixture-session".into(),
    }
}

fn start(root: &SessionConfigDir) -> (Transport, File, File) {
    let (stdout, out) = pipe();
    let (input, stdin) = pipe();
    (
        Transport::start(stdout, stdin, location(root), "2.1.296".into()).expect("start transport"),
        out,
        input,
    )
}

#[test]
fn permission_request_has_durable_open_record() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    transport
        .try_submit(&serde_json::json!({"type":"user"}))
        .unwrap();
    writeln!(output, "{}", serde_json::json!({
        "type":"control_request", "request_id":"permission-example",
        "request":{"subtype":"can_use_tool", "tool_name":"Bash", "tool_use_id":"tool-example", "input":{"command":"true"}}
    })).unwrap();
    accepted(&transport);
    assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
    let open = receive(&transport).record;
    assert_eq!(open.origin, RecordOrigin::Flk);
    let value: Value = serde_json::from_str(&open.raw).unwrap();
    assert_eq!(value["type"], "flk_permission_request");
    assert_eq!(value["state"], "open");
    assert_eq!(value["request_id"], "permission-example");
    let stream = records(&root);
    assert_eq!(stream.len(), 3);
    assert_eq!(
        serde_json::from_str::<Value>(&stream[0].raw).unwrap()["type"],
        "flk_input_accepted"
    );
}

fn records(root: &SessionConfigDir) -> Vec<StreamRecord> {
    let text = fs::read_to_string(
        root.path()
            .join("sessions/fixture-agent/fixture-session.jsonl"),
    )
    .expect("durable stream");
    assert!(
        text.is_empty() || text.ends_with('\n'),
        "torn session record"
    );
    text.lines()
        .map(|s| serde_json::from_str(s).expect("complete session record"))
        .collect()
}

fn user(text: &str) -> Value {
    serde_json::json!({"type":"user","message":{"role":"user","content":text}})
}

fn reader_done(transport: &Transport) -> io::Result<()> {
    worker_done(transport, true)
}

#[test]
fn fixture_lines_reach_s1_and_durable_stream_including_final_line() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, _input) = start(&root);
    let fixture = include_str!("../../../tests/fixtures/claude-session/events.jsonl");
    let lines: Vec<_> = fixture.lines().take(3).collect();
    stdout
        .write_all(lines.join("\n").as_bytes())
        .expect("fixture stdout");
    drop(stdout);
    for (index, raw) in lines.iter().enumerate() {
        let received = receive_after_accepted(&transport);
        assert_eq!(received.record.seq, index as u64 + 1);
        assert_eq!(received.record.raw, *raw);
        assert_eq!(received.event, SessionEvent::parse(raw, "2.1.296"));
        let persisted = records(&root);
        assert!(persisted.len() > index, "persistence precedes delivery");
        assert_eq!(persisted[index].raw, *raw);
        assert!(received.record.received_at_ms > 0);
    }
    reader_done(&transport).expect("clean EOF");
}

#[test]
fn large_lines_oversize_unknown_and_invalid_utf8_are_explicit() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, _input) = start(&root);
    let large = serde_json::json!({"type":"assistant","message":{"content":"x".repeat(1024*1024)}})
        .to_string();
    let expected = large.clone();
    let producer = thread::spawn(move || {
        stdout.write_all(large.as_bytes()).expect("1 MiB event");
        stdout.write_all(b"\n").expect("separator");
        stdout
            .write_all(&vec![b'x'; MAX_LINE_BYTES + 99])
            .expect("oversize");
        stdout.write_all(b"\n\xff\xfe").expect("invalid final UTF8");
    });
    let event = receive_after_accepted(&transport);
    assert!(matches!(event.event, SessionEvent::Assistant(_)));
    assert_eq!(event.record.raw, expected);
    let event = receive_after_accepted(&transport);
    assert!(matches!(event.event, SessionEvent::Unknown { .. }));
    let summary: Value = serde_json::from_str(&event.record.raw).expect("summary");
    assert_eq!(summary["type"], "flk_oversize");
    assert_eq!(summary["bytes"], MAX_LINE_BYTES + 99);
    let event = receive_after_accepted(&transport);
    assert!(matches!(event.event, SessionEvent::Unknown { .. }));
    let summary: Value = serde_json::from_str(&event.record.raw).expect("summary");
    assert_eq!(summary["type"], "flk_invalid_utf8");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(summary["base64"].as_str().expect("base64"))
            .expect("original bytes"),
        b"\xff\xfe"
    );
    producer.join().expect("producer");
    reader_done(&transport).expect("EOF");
    assert_eq!(records(&root).len(), 3);
}

#[test]
fn lossy_result_prefix_reopens_gate_without_claiming_turn_success() {
    for (invalid_utf8, escaped) in [(false, false), (true, false), (false, true)] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
        let (transport, mut stdout, input) = start(&root);
        transport.try_submit(&user("first")).expect("idle submit");
        let mut input = BufReader::new(input);
        let mut envelope = String::new();
        input.read_line(&mut envelope).expect("first input");
        let producer = thread::spawn(move || {
            // Keep the harness's top-level event header before the lost body.
            let mut raw = Vec::new();
            raw.extend_from_slice(if escaped {
                b"{\"type\":\"res\\u0075lt\",\"padding\":\""
            } else {
                b"{\"type\":\"result\",\"padding\":\""
            });
            raw.extend(std::iter::repeat_n(
                b'x',
                if invalid_utf8 { 16 } else { MAX_LINE_BYTES },
            ));
            if invalid_utf8 {
                raw.push(0xff);
            }
            raw.extend_from_slice(b"\"}\n");
            stdout.write_all(&raw).expect("lossy result");
            stdout
        });
        let received = receive_after_accepted(&transport);
        assert_eq!(received.record.origin, RecordOrigin::Flk);
        assert_eq!(
            serde_json::from_str::<Value>(&received.record.raw).expect("loss summary")["turn_end"],
            "result"
        );
        assert!(matches!(received.event, SessionEvent::Unknown { .. }));
        assert!(
            received.record.result_subtype.is_none(),
            "loss cannot claim success"
        );
        transport
            .try_submit(&user("next"))
            .expect("lossy result ends turn");
        envelope.clear();
        input.read_line(&mut envelope).expect("next input");
        assert_eq!(
            serde_json::from_str::<Value>(&envelope).expect("envelope"),
            user("next")
        );
        let mut stdout = producer.join().expect("producer");
        stdout
            .write_all(b"{\"type\":\"result\",\"subtype\":\"success\"}\n")
            .expect("final result");
        receive_after_accepted(&transport);
        drop(stdout);
        reader_done(&transport).expect("EOF");
    }
}

#[test]
fn lossy_header_never_mistakes_nested_or_truncated_types_for_result() {
    for (prefix, expected) in [
        (
            b"{\"message\":{\"type\":\"result\"},\"type\":\"assistant\",\"padding\":\"".as_slice(),
            "not_result",
        ),
        (
            b"{\"message\":{\"type\":\"result\"},\"padding\":\"".as_slice(),
            "unknown",
        ),
        (b"{\"type\":\"res".as_slice(), "unknown"),
    ] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
        let (transport, mut stdout, input) = start(&root);
        transport.try_submit(&user("first")).expect("input");
        let mut input = BufReader::new(input);
        let mut envelope = String::new();
        input.read_line(&mut envelope).expect("first input");
        let producer = thread::spawn(move || {
            stdout.write_all(prefix).expect("prefix");
            stdout
                .write_all(&vec![b'x'; MAX_LINE_BYTES])
                .expect("lost body");
            stdout.write_all(b"\"}\n").expect("line end");
            stdout
        });
        let loss = receive_after_accepted(&transport);
        assert_eq!(
            serde_json::from_str::<Value>(&loss.record.raw).expect("summary")["turn_end"],
            expected
        );
        let error = transport
            .try_submit(&user("unsafe steering"))
            .expect_err("no confirmed boundary");
        if expected == "unknown" {
            assert!(error.to_string().contains("structured_turn_end_unknown"));
        } else {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        }
        let mut stdout = producer.join().expect("producer");
        stdout
            .write_all(b"{\"type\":\"result\",\"subtype\":\"success\"}\n")
            .expect("real boundary");
        receive_after_accepted(&transport);
        transport
            .try_submit(&user("safe next"))
            .expect("confirmed next turn");
        envelope.clear();
        input.read_line(&mut envelope).expect("next input");
        drop(stdout);
        reader_done(&transport).expect("EOF");
    }
}

#[test]
fn indeterminate_loss_reports_named_turn_state_until_a_real_result() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, input) = start(&root);
    transport.try_submit(&user("first")).expect("idle submit");
    let mut input = BufReader::new(input);
    let mut envelope = String::new();
    input.read_line(&mut envelope).expect("input");
    let producer = thread::spawn(move || {
        stdout
            .write_all(&vec![b'x'; MAX_LINE_BYTES + 1])
            .expect("unidentifiable loss");
        stdout.write_all(b"\n").expect("line end");
        stdout
    });
    receive_after_accepted(&transport);
    let error = transport
        .try_submit(&user("unsafe retry"))
        .expect_err("unknown boundary");
    assert_ne!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("structured_turn_end_unknown"));
    let mut stdout = producer.join().expect("producer");
    stdout
        .write_all(b"{\"type\":\"result\",\"subtype\":\"success\"}\n")
        .expect("confirmed boundary");
    receive_after_accepted(&transport);
    transport
        .try_submit(&user("confirmed next"))
        .expect("result repairs uncertainty");
    envelope.clear();
    input.read_line(&mut envelope).expect("next input");
    drop(stdout);
    reader_done(&transport).expect("EOF");
}

#[test]
fn private_idle_gate_reopens_on_unknown_result_and_records_subtype() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, input) = start(&root);
    assert!(transport
        .try_submit(&serde_json::json!({"type":"control_request"}))
        .is_err());
    transport.try_submit(&user("first")).expect("idle submit");
    assert_eq!(
        transport
            .try_submit(&user("second"))
            .expect_err("no steering")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let mut input = BufReader::new(input);
    let mut line = String::new();
    input.read_line(&mut line).expect("first input");
    assert_eq!(
        serde_json::from_str::<Value>(&line).expect("input"),
        user("first")
    );
    stdout
        .write_all(b"{\"type\":\"result\",\"subtype\":\"new_result\"}\n")
        .expect("unknown result");
    accepted(&transport);
    let received = receive(&transport);
    assert!(matches!(received.event, SessionEvent::Unknown { .. }));
    assert_eq!(
        received.record.result_subtype,
        Some(Value::String("new_result".into()))
    );
    transport
        .try_submit(&user("second"))
        .expect("unmodelled result ends turn");
    line.clear();
    input.read_line(&mut line).expect("second input");
    assert_eq!(
        serde_json::from_str::<Value>(&line).expect("input"),
        user("second")
    );
    stdout
        .write_all(b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}\n")
        .expect("result");
    accepted(&transport);
    receive(&transport);
    transport
        .try_submit(&user("third"))
        .expect("next idle submit");
    line.clear();
    input.read_line(&mut line).expect("third input");
    assert_eq!(
        serde_json::from_str::<Value>(&line).expect("input"),
        user("third")
    );
}

#[test]
fn full_stdin_pipe_does_not_block_submit_poll_or_close() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (stdout, _stdout) = pipe();
    let (mut input, mut stdin) = pipe();
    private_fs::set_nonblocking(&stdin).expect("nonblocking fixture writer");
    let mut filled = 0;
    loop {
        match stdin.write(&[b'p'; 8192]) {
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("prefill failed: {e}"),
        }
    }
    assert!(filled > 0, "fixture pipe really filled to WouldBlock");
    let mut transport =
        Transport::start(stdout, stdin, location(&root), "2.1.296".into()).expect("start");
    let begin = Instant::now();
    transport
        .try_submit(&user(&"x".repeat(1024 * 1024)))
        .expect("large input");
    assert!(matches!(
        transport.try_receive(),
        Err(mpsc::TryRecvError::Empty)
    ));
    transport.close_stdin();
    assert!(begin.elapsed() < Duration::from_secs(1));
    let result = worker_done(&transport, false);
    result.expect("close interrupts blocked writer");
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes).expect("stdin really closed");
    assert_eq!(
        bytes.len(),
        filled,
        "writer did not progress through the full pipe"
    );
    assert!(transport.try_submit(&user("late")).is_err());
}

#[test]
fn consumer_backpressure_is_bounded_and_cancellable() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, _input) = start(&root);
    stdout
        .write_all(b"one\ntwo\nthree\nfour\nfive\nsix\n")
        .expect("stdout");
    wait(&transport, || {
        (root
            .path()
            .join("sessions/fixture-agent/fixture-session.jsonl")
            .exists()
            && records(&root).len() == 5)
            .then_some(())
    });
    transport.cancel();
    assert_eq!(
        reader_done(&transport)
            .expect_err("explicit cancellation")
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(
        records(&root).len(),
        5,
        "four notifications plus one persisted pending record"
    );
    assert_eq!(transport.output.try_iter().count(), 4);
}

#[test]
fn reopen_continues_sequence_and_refuses_corrupt_streams() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    for seq in 1..=2 {
        let (transport, mut stdout, _input) = start(&root);
        stdout.write_all(b"line\n").expect("line");
        drop(stdout);
        assert_eq!(receive_after_accepted(&transport).record.seq, seq);
        reader_done(&transport).expect("EOF");
    }
    let path = root
        .path()
        .join("sessions/fixture-agent/fixture-session.jsonl");
    fs::write(&path, b"corrupt-interior\n{}").expect("corrupt stream");
    let (transport, _out, _input) = start(&root);
    assert!(reader_done(&transport).is_err());
    assert_eq!(fs::read(path).expect("preserved"), b"corrupt-interior\n{}");
}

#[test]
fn disk_setup_errors_are_reported_without_accepting_more_input() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    fs::write(root.path().join("sessions"), b"not a directory").expect("blocked disk path");
    let (transport, _out, _input) = start(&root);
    assert!(reader_done(&transport).is_err());
    assert!(transport.try_submit(&user("late")).is_err());
}

#[test]
fn oversized_input_and_unsafe_components_are_refused() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, _out, _input) = start(&root);
    assert!(transport
        .try_submit(&user(&"x".repeat(MAX_LINE_BYTES)))
        .is_err());
    transport
        .try_submit(&user("small"))
        .expect("gate still idle after refused input");
    let (stdout, _) = pipe();
    let (_, stdin) = pipe();
    let mut loc = location(&root);
    loc.agent = "../escape".into();
    assert!(Transport::start(stdout, stdin, loc, "2.1.296".into()).is_err());
}

#[test]
fn failed_persistence_never_publishes_or_reopens_idle_gate() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let path = root.path().join("read-only");
    fs::write(&path, b"").expect("fixture");
    let mut file = File::open(path).expect("read-only descriptor");
    let (tx, rx) = mpsc::sync_channel(4);
    let busy = AtomicBool::new(true);
    let cancel = AtomicBool::new(false);
    let raw = br#"{"type":"result","subtype":"success","is_error":false}"#;
    assert!(emit(
        &mut file,
        &mut 0,
        raw,
        raw.len() as u64,
        "2.1.296",
        &tx,
        &busy,
        &AtomicBool::new(false),
        &cancel,
        false,
        (
            &super::super::blobs::BlobStore::open(root.path()).expect("store"),
            2048,
            &super::super::history::History::default()
        )
    )
    .is_err());
    assert!(busy.load(Ordering::Acquire));
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
}

#[test]
fn broken_stdin_is_an_explicit_worker_error() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, _out, input) = start(&root);
    drop(input);
    transport
        .try_submit(&user("unreadable"))
        .expect("asynchronous offer");
    let result = worker_done(&transport, false);
    assert_eq!(
        result.expect_err("write failure").kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(transport.try_submit(&user("late")).is_err());
}

#[test]
fn existing_stream_links_and_unsafe_modes_are_refused() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (mut file, _, _) =
        open_stream(location(&root), &AtomicBool::new(false)).expect("private stream");
    file.write_all(b"").expect("fixture stream");
    drop(file);
    let path = root
        .path()
        .join("sessions/fixture-agent/fixture-session.jsonl");
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("unsafe mode");
    assert!(open_stream(location(&root), &AtomicBool::new(false)).is_err());
    fs::remove_file(&path).expect("remove fixture");
    let target = root.path().join("target");
    fs::write(&target, b"untouched").expect("target");
    symlink(&target, &path).expect("fixture symlink");
    assert!(open_stream(location(&root), &AtomicBool::new(false)).is_err());
    assert_eq!(fs::read(target).expect("preserved target"), b"untouched");
}

#[test]
fn torn_last_record_is_truncated_and_sequence_continues() {
    for tail in [b"{\"seq\":3".as_slice(), b"{not-json}".as_slice()] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
        let (mut file, _, _) =
            open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
        let mut good = Vec::new();
        for seq in 1..=2 {
            let record = serde_json::json!({"seq":seq,"received_at_ms":1,"origin":"claude","result_subtype":null,"raw":"prior"});
            serde_json::to_writer(&mut good, &record).expect("fixture record");
            good.push(b'\n');
        }
        file.write_all(&good).expect("good records");
        file.write_all(tail).expect("torn tail");
        drop(file);
        let (file, seq, _) =
            open_stream(location(&root), &AtomicBool::new(false)).expect("repair tail");
        assert_eq!(seq, 2);
        assert_eq!(
            file.metadata().expect("repaired size").len(),
            good.len() as u64
        );
        drop(file);
        assert_eq!(
            fs::read(
                root.path()
                    .join("sessions/fixture-agent/fixture-session.jsonl")
            )
            .expect("repaired content"),
            good
        );
        let (transport, mut stdout, _input) = start(&root);
        stdout.write_all(b"after-repair").expect("new final line");
        drop(stdout);
        assert_eq!(receive_after_accepted(&transport).record.seq, 3);
        reader_done(&transport).expect("EOF");
        assert_eq!(records(&root).len(), 3);
    }
}

#[test]
fn reserved_queue_session_names_have_a_named_refusal() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    for reserved in ["queue", "Queue", "QUEUE"] {
        let (stdout, _out) = pipe();
        let (_input, stdin) = pipe();
        let mut loc = location(&root);
        loc.session = reserved.into();
        let error = Transport::start(stdout, stdin, loc, "2.1.296".into())
            .err()
            .expect("reserved id refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "structured_reserved_session_id");
    }
    assert!(
        !root.path().join("sessions").exists(),
        "refusal precedes file setup"
    );
}

#[test]
fn claude_cannot_spoof_transport_record_origin() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (transport, mut stdout, _input) = start(&root);
    let producer = thread::spawn(move || {
        stdout
            .write_all(b"{\"type\":\"flk_oversize\",\"origin\":\"flk\",\"bytes\":999}\n")
            .expect("spoofed marker");
        stdout
            .write_all(&vec![b'x'; MAX_LINE_BYTES + 1])
            .expect("real oversize");
        stdout.write_all(b"\n").expect("separator");
    });
    let spoof = receive_after_accepted(&transport);
    let genuine = receive_after_accepted(&transport);
    assert_eq!(spoof.record.origin, RecordOrigin::Claude);
    assert_eq!(genuine.record.origin, RecordOrigin::Flk);
    for received in [&spoof, &genuine] {
        assert!(matches!(received.event, SessionEvent::Unknown { .. }));
        assert_eq!(
            serde_json::from_str::<Value>(&received.record.raw).expect("marker")["type"],
            "flk_oversize"
        );
    }
    producer.join().expect("producer");
    reader_done(&transport).expect("EOF");
    let persisted = records(&root);
    assert_eq!(persisted[0].origin, RecordOrigin::Claude);
    assert_eq!(persisted[1].origin, RecordOrigin::Flk);
}

#[test]
fn recovery_scans_an_oversized_torn_tail_without_accepting_it() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (mut file, _, _) = open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
    file.write_all(&vec![b'x'; RECORD_LIMIT as usize + 99])
        .expect("oversized torn tail");
    drop(file);
    let (file, seq, _) =
        open_stream(location(&root), &AtomicBool::new(false)).expect("repair oversized tail");
    assert_eq!(seq, 0);
    assert_eq!(file.metadata().expect("size").len(), 0);
}

#[test]
fn malformed_newline_terminated_final_record_is_corruption() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (mut file, _, _) = open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
    let bytes = b"{not-a-record}\n";
    file.write_all(bytes).expect("corrupt complete record");
    drop(file);
    assert!(
        open_stream(location(&root), &AtomicBool::new(false)).is_err(),
        "complete malformed record must be refused"
    );
    assert_eq!(
        fs::read(
            root.path()
                .join("sessions/fixture-agent/fixture-session.jsonl")
        )
        .expect("preserved corrupt file"),
        bytes
    );
}

#[test]
fn condition_wait_surfaces_the_dead_workers_error() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    fs::write(root.path().join("sessions"), b"not a directory").expect("disk failure fixture");
    let (transport, _out, _input) = start(&root);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait(&transport, || None::<()>)
    }))
    .expect_err("worker failure surfaces immediately");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("panic diagnostic");
    assert!(
        message.contains("Reader(Err"),
        "actual worker failure: {message}"
    );
    assert!(
        !message.contains("deadline"),
        "worker death is not a timeout"
    );
}

fn permission(output: &mut File, id: &str) {
    writeln!(output,"{}",serde_json::json!({"type":"control_request","request_id":id,
        "request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{"command":"true"}}})).unwrap();
}
fn waiting_ids(transport: &Transport) -> Vec<String> {
    transport
        .waiting_on_human()
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}
fn permission_handle(transport: &Transport, id: &str) -> u64 {
    wait(transport, || match transport.waiting_on_human() {
        Ok(handles) => Some(
            handles
                .into_iter()
                .find(|(request_id, _)| request_id == id)
                .expect("the request must be surfaced before deciding")
                .1,
        ),
        Err(ErrorCode::SessionBusy) => None,
        Err(code) => panic!("permission snapshot failed: {code}"),
    })
}
fn decide(id: &str, decision: permissions::Decision, request_seq: u64) -> AgentDecide {
    AgentDecide {
        agent: "fixture-agent".into(),
        request_id: id.into(),
        request_seq,
        decision,
        message: Some("operator reason".into()),
    }
}
fn owner() -> Principal<'static> {
    Principal::Token(crate::structured::permissions::auth::fixture(
        "fixture-client",
        true,
    ))
}
// SessionBusy is the API's nonblocking contention signal, not a decision.
fn call_decide(
    transport: &Transport,
    principal: &Principal<'_>,
    args: &AgentDecide,
) -> Result<(), ErrorCode> {
    wait(transport, || {
        match transport.agent_decide(principal, args) {
            Err(ErrorCode::SessionBusy) => None,
            result => Some(result),
        }
    })
}
fn input_line(transport: &Transport, input: &mut File) -> Value {
    private_fs::set_nonblocking(input).unwrap();
    let mut bytes = Vec::new();
    wait(transport, || {
        let mut byte = [0];
        match input.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => Some(serde_json::from_slice(&bytes).unwrap()),
            Ok(1) => {
                bytes.push(byte[0]);
                None
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            other => panic!("unexpected harness stdin: {other:?}"),
        }
    })
}

#[test]
fn permission_decisions_are_audited_before_control_stdin_and_keep_turn_busy() {
    for (decision, behavior) in [
        (permissions::Decision::AllowOnce, "allow"),
        (permissions::Decision::Deny, "deny"),
    ] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        transport.try_submit(&user("first")).unwrap();
        assert_eq!(input_line(&transport, &mut input), user("first"));
        permission(&mut output, "request-example");
        accepted(&transport);
        assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
        assert_eq!(receive(&transport).record.origin, RecordOrigin::Flk);
        assert_eq!(waiting_ids(&transport), vec!["request-example"]);
        let request_seq = permission_handle(&transport, "request-example");
        call_decide(
            &transport,
            &owner(),
            &decide("request-example", decision, request_seq),
        )
        .unwrap();
        let response = input_line(&transport, &mut input);
        let audit = records(&root);
        assert_eq!(
            audit.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        let closed: Value = serde_json::from_str(&audit[3].raw).unwrap();
        assert_eq!(closed["state"], "closed");
        assert_eq!(closed["decision"], serde_json::to_value(decision).unwrap());
        assert_eq!(closed["principal"], "fixture-client");
        assert_eq!(audit[3].origin, RecordOrigin::Flk);
        assert_eq!(response["type"], "control_response");
        assert_eq!(response["response"]["request_id"], "request-example");
        assert_eq!(response["response"]["subtype"], "success");
        assert_eq!(response["response"]["response"]["behavior"], behavior);
        if behavior == "deny" {
            assert_eq!(
                response["response"]["response"]["message"],
                "operator reason"
            );
        } else {
            assert_eq!(
                response["response"]["response"]["updatedInput"],
                serde_json::json!({"command":"true"})
            );
        }
        let published = receive(&transport).record;
        assert_eq!(published.seq, 4);
        assert!(
            transport.input_inflight(),
            "a permission answer is not a result"
        );
        assert_eq!(
            transport.try_submit(&user("second")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            call_decide(
                &transport,
                &owner(),
                &decide("request-example", decision, request_seq)
            ),
            Err(ErrorCode::PermissionRequestClosed)
        );
    }
}

#[test]
fn open_permission_blocks_input_and_ignores_spoof_until_a_real_result() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    writeln!(output,"{}",serde_json::json!({"type":"flk_permission_request","state":"closed","request_id":"request-example"})).unwrap();
    assert_eq!(
        receive_after_accepted(&transport).record.origin,
        RecordOrigin::Claude
    );
    assert_eq!(
        transport.try_submit(&user("second")).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"result","subtype":"success","is_error":false})
    )
    .unwrap();
    receive_after_accepted(&transport);
    let closed: Value =
        serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
    assert_eq!(closed["reason"], "result");
    assert!(!transport.input_inflight());
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("request-example", permissions::Decision::Deny, 1)
        ),
        Err(ErrorCode::PermissionRequestClosed)
    );
    wait(&transport, || transport.try_submit(&user("second")).ok());
    assert_eq!(input_line(&transport, &mut input), user("second"));
}

#[test]
fn refused_decisions_do_not_write_or_close_and_oversized_response_can_retry() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    for principal in [
        Principal::Operator,
        Principal::Lease("fixture-agent"),
        Principal::Agent("other-agent"),
    ] {
        assert_eq!(
            call_decide(
                &transport,
                &principal,
                &decide("request-example", permissions::Decision::Deny, 1)
            ),
            Err(ErrorCode::NotAuthorized)
        );
    }
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("unknown", permissions::Decision::Deny, 1)
        ),
        Err(ErrorCode::PermissionRequestUnknown)
    );

    let mut huge = decide("request-example", permissions::Decision::Deny, 1);
    huge.message = Some("x".repeat(MAX_LINE_BYTES + 1));
    assert_eq!(
        call_decide(&transport, &owner(), &huge),
        Err(ErrorCode::InvalidParams)
    );
    assert_eq!(records(&root).len(), 2);
    private_fs::set_nonblocking(&input).unwrap();
    assert_eq!(
        input.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    call_decide(
        &transport,
        &owner(),
        &decide("request-example", permissions::Decision::Deny, 1),
    )
    .unwrap();
    receive_after_accepted(&transport);
    assert_eq!(
        input_line(&transport, &mut input)["response"]["response"]["behavior"],
        "deny"
    );
}

#[test]
fn already_queued_user_input_waits_for_the_control_response() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    // Exercise the writer's queue independently of the admission gate.
    transport
        .input
        .input
        .as_ref()
        .unwrap()
        .try_send(Input::User(UserInput {
            bytes: encode(&user("queued")).unwrap(),
            admitted: Arc::new(AtomicBool::new(true)),
        }))
        .unwrap_or_else(|_| panic!("queue admission"));
    call_decide(
        &transport,
        &owner(),
        &decide("request-example", permissions::Decision::Deny, 1),
    )
    .unwrap();
    receive_after_accepted(&transport);
    assert_eq!(
        input_line(&transport, &mut input)["type"],
        "control_response"
    );
    assert_eq!(input_line(&transport, &mut input), user("queued"));
}

#[test]
fn unknown_persisted_raw_kind_reopens_without_permission_state() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let cancel = AtomicBool::new(false);
    let (mut file, mut seq, _) = open_stream(location(&root), &cancel).unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    let raw = br#"{"type":"future_permission_kind","request_id":"future-request"}"#;
    emit(
        &mut file,
        &mut seq,
        raw,
        raw.len() as u64,
        "2.1.296",
        &tx,
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &cancel,
        true,
        (
            &super::super::blobs::BlobStore::open(root.path()).expect("store"),
            2048,
            &super::super::history::History::default(),
        ),
    )
    .unwrap();
    assert!(matches!(
        rx.recv().unwrap().event,
        SessionEvent::Unknown { .. }
    ));
    drop(file);
    let (_file, seq, _) = open_stream(location(&root), &cancel).unwrap();
    assert_eq!(seq, 1);
    assert_eq!(records(&root)[0].raw, std::str::from_utf8(raw).unwrap());
    assert!(Book::new("fixture-agent".into()).waiting().is_empty());
}

#[test]
fn full_control_queue_refuses_without_reserving_the_request() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, _output, _input) = start(&root);
    let (tx, _rx) = mpsc::sync_channel(4);
    let mut handle = transport.input.clone();
    handle.decisions = tx;
    for n in 0..5 {
        let id = format!("request-{n}");
        let request=serde_json::from_value(serde_json::json!({"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{}})).unwrap();
        handle.permissions.lock().observe(&id, request).unwrap();
        assert_eq!(
            handle.agent_decide(&owner(), &decide(&id, permissions::Decision::Deny, n + 1)),
            if n == 4 {
                Err(ErrorCode::SessionBusy)
            } else {
                Ok(())
            }
        );
    }
    assert!(handle
        .permissions
        .lock()
        .prepare(
            &owner(),
            &decide("request-4", permissions::Decision::Deny, 5)
        )
        .is_ok());
}

#[test]
fn failed_decision_audit_never_enqueues_a_control_response() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let path = root.path().join("read-only");
    fs::write(&path, b"").unwrap();
    let mut file = File::open(path).unwrap();
    let mut book = Book::new("fixture-agent".into());
    let request=serde_json::from_value(serde_json::json!({"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{}})).unwrap();
    book.observe("request-example", request).unwrap();
    let response = book
        .prepare(
            &owner(),
            &decide("request-example", permissions::Decision::Deny, 1),
        )
        .unwrap();
    let control = Control {
        bytes: encode(&response.response).unwrap(),
        answer: response,
    };
    let (input, rx) = mpsc::sync_channel(4);
    let (_decisions, decisions) = mpsc::sync_channel(4);
    let plane = ControlPlane {
        accepted: mpsc::sync_channel(1).1,
        book: Arc::new(parking_lot::Mutex::new(book)),
        input,
        decisions,
    };
    let (tx, notifications) = mpsc::sync_channel(4);
    assert!(control::Recorder::new(
        &mut file,
        &mut 0,
        "2.1.296",
        &tx,
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &plane,
        (
            &super::super::blobs::BlobStore::open(root.path()).expect("store"),
            2048,
            &super::super::history::History::default()
        ),
    )
    .command(Command::Decide(control))
    .is_err());
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(
        notifications.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(
        plane.book.lock().waiting(),
        vec![("request-example".into(), 1)]
    );
}

fn replay_open_permissions(root: &SessionConfigDir) -> std::collections::BTreeSet<String> {
    let mut open = std::collections::BTreeSet::new();
    for record in records(root) {
        if record.origin != RecordOrigin::Flk {
            continue;
        }
        let value: Value = serde_json::from_str(&record.raw).unwrap();
        if value["type"] != "flk_permission_request" {
            continue;
        }
        let id = value["request_id"].as_str().unwrap().to_owned();
        match value["state"].as_str().unwrap() {
            "open" => assert!(open.insert(id)),
            "closed" => assert!(
                open.remove(&id),
                "close must follow exactly one open: {value}"
            ),
            state => panic!("unknown permission state {state}"),
        }
    }
    open
}

#[test]
fn eof_closes_every_open_permission_in_the_durable_replay() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    for id in ["request-a", "request-b"] {
        permission(&mut output, id);
        receive_after_accepted(&transport);
        receive_after_accepted(&transport);
    }
    drop(output);
    reader_done(&transport).unwrap();
    assert!(replay_open_permissions(&root).is_empty());
    let closed: Vec<Value> = records(&root)
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.raw).ok())
        .filter(|v| v["state"] == "closed")
        .collect();
    assert_eq!(closed.len(), 2);
    for close in closed {
        assert_eq!(close["reason"], "ended");
        assert!(close.get("decision").is_none());
    }
}

#[test]
fn cancellation_closes_all_requests_even_with_a_full_notification_queue() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    for id in ["request-a", "request-b", "request-c"] {
        permission(&mut output, id);
    }
    wait(&transport, || {
        let path = root
            .path()
            .join("sessions/fixture-agent/fixture-session.jsonl");
        let text = fs::read_to_string(path).ok()?;
        (text
            .lines()
            .filter_map(|s| serde_json::from_str::<StreamRecord>(s).ok())
            .count()
            >= 6)
            .then_some(())
    });
    transport.cancel();
    assert_eq!(
        reader_done(&transport).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(replay_open_permissions(&root).is_empty());
    assert_eq!(records(&root).len(), 9);
}

#[test]
fn unauthorized_decisions_reveal_neither_contention_nor_lifecycle() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, _output, _input) = start(&root);
    {
        let _lock = transport.input.permissions.lock();
        assert_eq!(
            transport.agent_decide(
                &Principal::Operator,
                &decide("unknown", permissions::Decision::Deny, 1)
            ),
            Err(ErrorCode::NotAuthorized)
        );
        assert_eq!(
            transport.close_permissions(
                &Principal::Operator,
                None,
                permissions::CloseReason::Interrupted
            ),
            Err(ErrorCode::NotAuthorized)
        );
    }
    transport.cancel();
    assert_eq!(
        transport.agent_decide(
            &Principal::Lease("fixture-agent"),
            &decide("unknown", permissions::Decision::Deny, 1)
        ),
        Err(ErrorCode::NotAuthorized)
    );
}

#[test]
fn second_held_input_is_a_typed_refusal_without_writing_either_input() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    for text in ["first", "second"] {
        transport
            .input
            .input
            .as_ref()
            .unwrap()
            .try_send(Input::User(UserInput {
                bytes: encode(&user(text)).unwrap(),
                admitted: Arc::new(AtomicBool::new(true)),
            }))
            .unwrap_or_else(|_| panic!("fixture input queue"));
    }
    let error = worker_done(&transport, false).unwrap_err();
    assert_eq!(
        error.get_ref().and_then(|e| e.downcast_ref::<ErrorCode>()),
        Some(&ErrorCode::PermissionInputAlreadyHeld)
    );
    private_fs::set_nonblocking(&input).unwrap();
    match input.read(&mut [0]) {
        Ok(0) => (),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
        other => panic!("held input was written: {other:?}"),
    }
}

fn close_permission(
    transport: &Transport,
    id: Option<&str>,
    reason: permissions::CloseReason,
) -> Result<(), ErrorCode> {
    wait(transport, || {
        match transport.close_permissions(&owner(), id, reason) {
            Err(ErrorCode::SessionBusy) => None,
            result => Some(result),
        }
    })
}

#[test]
fn host_closures_audit_cancel_result_and_interrupt_without_sending_decisions() {
    for reason in [
        permissions::CloseReason::Cancelled,
        permissions::CloseReason::Result,
        permissions::CloseReason::Interrupted,
    ] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        for id in ["request-a", "request-b"] {
            permission(&mut output, id);
            receive_after_accepted(&transport);
            receive_after_accepted(&transport);
        }
        close_permission(&transport, Some("request-a"), reason).unwrap();
        let close: Value =
            serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
        assert_eq!(close["reason"], serde_json::to_value(reason).unwrap());
        assert!(close.get("decision").is_none());
        assert_eq!(
            call_decide(
                &transport,
                &owner(),
                &decide("request-a", permissions::Decision::AllowOnce, 1)
            ),
            Err(ErrorCode::PermissionRequestClosed)
        );
        assert_eq!(waiting_ids(&transport), vec!["request-b"]);
        close_permission(&transport, None, permissions::CloseReason::Interrupted).unwrap();
        receive_after_accepted(&transport);
        assert!(replay_open_permissions(&root).is_empty());
        private_fs::set_nonblocking(&input).unwrap();
        assert_eq!(
            input.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn harness_cancel_closes_only_its_request_and_never_sends_a_decision() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    for id in ["request-a", "request-b"] {
        permission(&mut output, id);
        receive_after_accepted(&transport);
        receive_after_accepted(&transport);
    }
    writeln!(
        output,
        "{}",
        serde_json::json!({
            "type": "control_cancel_request", "request_id": "request-a"
        })
    )
    .unwrap();
    assert!(matches!(
        receive_after_accepted(&transport).event,
        SessionEvent::ControlCancel(_)
    ));
    let close: Value =
        serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
    assert_eq!(close["state"], "closed");
    assert_eq!(close["request_id"], "request-a");
    assert_eq!(close["reason"], "cancelled");
    assert!(close.get("decision").is_none());
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("request-a", permissions::Decision::AllowOnce, 1)
        ),
        Err(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(waiting_ids(&transport), vec!["request-b"]);
    // Duplicate and unrelated cancellation must not fabricate another close.
    for id in ["request-a", "unknown-request"] {
        writeln!(
            output,
            "{}",
            serde_json::json!({
                "type": "control_cancel_request", "request_id": id
            })
        )
        .unwrap();
        assert!(matches!(
            receive_after_accepted(&transport).event,
            SessionEvent::ControlCancel(_)
        ));
    }
    call_decide(
        &transport,
        &owner(),
        &decide(
            "request-b",
            permissions::Decision::Deny,
            permission_handle(&transport, "request-b"),
        ),
    )
    .unwrap();
    let close: Value =
        serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
    assert_eq!(close["request_id"], "request-b");
    // The following answer is a causal barrier: no cancelled answer preceded it.
    assert_eq!(
        input_line(&transport, &mut input)["response"]["request_id"],
        "request-b"
    );
    assert!(replay_open_permissions(&root).is_empty());
}

#[test]
fn a_cancelled_reservation_cannot_deliver_a_stale_control_response() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "cancelled-request");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    let stale = transport
        .input
        .permissions
        .lock()
        .prepare(
            &owner(),
            &decide("cancelled-request", permissions::Decision::AllowOnce, 1),
        )
        .unwrap();
    let bytes = encode(&stale.response).unwrap();
    close_permission(
        &transport,
        Some("cancelled-request"),
        permissions::CloseReason::Cancelled,
    )
    .unwrap();
    receive_after_accepted(&transport);
    transport
        .input
        .decisions
        .try_send(Command::Decide(Control {
            answer: stale,
            bytes,
        }))
        .unwrap_or_else(|_| panic!("stale command admission"));
    permission(&mut output, "next-request");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    call_decide(
        &transport,
        &owner(),
        &decide(
            "next-request",
            permissions::Decision::Deny,
            permission_handle(&transport, "next-request"),
        ),
    )
    .unwrap();
    receive_after_accepted(&transport);
    assert_eq!(
        input_line(&transport, &mut input)["response"]["request_id"],
        "next-request"
    );
    assert!(replay_open_permissions(&root).is_empty());
}

#[test]
fn nested_result_preserves_permissions_but_top_level_error_retires_them() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    permission(&mut output, "request-example");
    receive_after_accepted(&transport);
    receive_after_accepted(&transport);
    writeln!(output,"{}",serde_json::json!({"type":"result","subtype":"success","is_error":false,"parent_tool_use_id":"parent-tool"})).unwrap();
    receive_after_accepted(&transport);
    assert_eq!(waiting_ids(&transport), vec!["request-example"]);
    writeln!(output,"{}",serde_json::json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":[]})).unwrap();
    receive_after_accepted(&transport);
    assert_eq!(
        serde_json::from_str::<Value>(&receive_after_accepted(&transport).record.raw).unwrap()
            ["reason"],
        "result"
    );
    assert!(replay_open_permissions(&root).is_empty());
}

#[test]
fn oversized_permission_is_denied_with_bounded_audit_and_runtime_continues() {
    for target in [MAX_LINE_BYTES + 4096, MAX_LINE_BYTES] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        // Put correlation after input: retaining only the prefix cannot answer.
        let prefix = r#"{"request":{"input":{"data":""#;
        let suffix = r#""},"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example"},"type":"control_request","request_id":"large-request"}"#;
        let raw = format!(
            "{prefix}{}{suffix}",
            "x".repeat(target - prefix.len() - suffix.len())
        );
        assert_eq!(raw.len(), target);
        let producer = thread::spawn(move || {
            writeln!(output, "{raw}").unwrap();
            output
        });
        assert!(receive_after_accepted(&transport).record.raw.len() <= MAX_LINE_BYTES);
        let open: Value =
            serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
        assert_eq!(open["state"], "open");
        assert_eq!(open["request"]["input"]["truncated"], true);
        assert_eq!(open["request"]["input"]["wire_bytes"], target as u64);
        assert_eq!(
            open["request"]["input"]["wire_sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        let closed: Value =
            serde_json::from_str(&receive_after_accepted(&transport).record.raw).unwrap();
        assert_eq!(closed["decision"], "deny");
        assert_eq!(
            closed["error_code"],
            ErrorCode::PermissionRequestTooLarge.as_str()
        );
        let response = input_line(&transport, &mut input);
        assert_eq!(response["response"]["request_id"], "large-request");
        assert_eq!(response["response"]["response"]["behavior"], "deny");
        assert_eq!(
            response["response"]["response"]["message"],
            ErrorCode::PermissionRequestTooLarge.as_str()
        );
        let mut output = producer.join().unwrap();
        permission(&mut output, "next-request");
        receive_after_accepted(&transport);
        receive_after_accepted(&transport);
        call_decide(
            &transport,
            &owner(),
            &decide(
                "next-request",
                permissions::Decision::AllowOnce,
                permission_handle(&transport, "next-request"),
            ),
        )
        .unwrap();
        receive_after_accepted(&transport);
        assert_eq!(
            input_line(&transport, &mut input)["response"]["request_id"],
            "next-request"
        );
        assert!(replay_open_permissions(&root).is_empty());
        assert!(records(&root).iter().all(|r| r.raw.len() <= MAX_LINE_BYTES));
    }
}

#[test]
fn refused_close_restores_both_open_and_reserved_requests() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, _output, _input) = start(&root);
    let (tx, _rx) = mpsc::sync_channel(1);
    let mut handle = transport.input.clone();
    handle.decisions = tx;
    for id in ["reserved", "open"] {
        let request=serde_json::from_value(serde_json::json!({"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{}})).unwrap();
        handle.permissions.lock().observe(id, request).unwrap();
    }
    handle
        .agent_decide(
            &owner(),
            &decide("reserved", permissions::Decision::Deny, 1),
        )
        .unwrap();
    for id in ["open", "reserved"] {
        assert_eq!(
            handle.close_permissions(&owner(), Some(id), permissions::CloseReason::Cancelled),
            Err(ErrorCode::SessionBusy)
        );
    }
    let mut book = handle.permissions.lock();
    assert!(book
        .prepare(&owner(), &decide("open", permissions::Decision::Deny, 2))
        .is_ok());
    assert!(book.begin_answer("reserved", 1));
}

fn grant_request(
    output: &mut File,
    id: &str,
    tool: &str,
    input: Value,
    suggestions: Option<Value>,
) {
    let mut request = serde_json::json!({"subtype":"can_use_tool","tool_name":tool,"tool_use_id":"tool-example","input":input});
    if let Some(rules) = suggestions {
        request["permission_suggestions"] = rules;
    }
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"control_request","request_id":id,"request":request})
    )
    .unwrap();
}
fn opened(transport: &Transport) {
    assert_eq!(receive(transport).record.origin, RecordOrigin::Claude);
    let record = receive(transport).record;
    assert_eq!(record.origin, RecordOrigin::Flk);
    assert_eq!(
        serde_json::from_str::<Value>(&record.raw).unwrap()["state"],
        "open"
    );
}
fn close_with(
    transport: &Transport,
    input: &mut File,
    id: &str,
    decision: permissions::Decision,
) -> Value {
    call_decide(
        transport,
        &owner(),
        &decide(id, decision, permission_handle(transport, id)),
    )
    .unwrap();
    let record = receive(transport).record;
    assert_eq!(
        serde_json::from_str::<Value>(&record.raw).unwrap()["state"],
        "closed"
    );
    input_line(transport, input)
}

#[test]
fn session_grant_matches_canonical_input_but_other_input_and_tools_prompt() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    let original: Value =
        serde_json::from_str(r#"{"z":0,"options":{"b":2,"a":1},"items":[1,2]}"#).unwrap();
    grant_request(&mut output, "grant", "Bash", original.clone(), None);
    opened(&transport);
    let response = close_with(
        &transport,
        &mut input,
        "grant",
        permissions::Decision::AllowAlways,
    );
    assert_eq!(response["response"]["response"]["updatedInput"], original);
    let reordered =
        serde_json::from_str(r#"{"items":[1,2],"options":{"a":1,"b":2},"z":0}"#).unwrap();
    grant_request(&mut output, "match", "Bash", reordered, None);
    let automatic_request = receive_after_accepted(&transport);
    let mut live_projection = crate::structured::state::SessionState::new("fixture-agent".into());
    let update = live_projection.reduce(
        automatic_request.state_input().unwrap(),
        Instant::now(),
        1000,
        Duration::from_secs(30),
    );
    assert!(
        update.needs_human.is_empty(),
        "an automatic request must never publish needs-human at its raw record"
    );
    assert!(receive_after_accepted(&transport).state_input().is_none());
    let automatic = receive_after_accepted(&transport).record;
    let audit: Value = serde_json::from_str(&automatic.raw).unwrap();
    assert_eq!(audit["state"], "closed");
    assert_eq!(audit["source"], "session_grant");
    assert_eq!(audit["grant_request_id"], "grant");
    assert_eq!(audit["principal"], "fixture-client");
    assert_eq!(
        input_line(&transport, &mut input)["response"]["response"]["behavior"],
        "allow"
    );
    for (id, tool, value) in [
        (
            "different-input",
            "Bash",
            serde_json::json!({"z":1,"options":{"a":1,"b":2},"items":[1,2]}),
        ),
        ("different-tool", "Read", original),
        (
            "different-array",
            "Bash",
            serde_json::json!({"z":0,"options":{"a":1,"b":2},"items":[2,1]}),
        ),
    ] {
        grant_request(&mut output, id, tool, value, None);
        opened(&transport);
        // Automatic decisions reserve before publishing open, so an overbroad
        // match makes this manual decision fail rather than race the assertion.
        assert_eq!(
            close_with(&transport, &mut input, id, permissions::Decision::Deny)["response"]
                ["response"]["behavior"],
            "deny"
        );
    }
    let stream = records(&root);
    assert_eq!(stream.len(), 15);
    assert_eq!(
        stream.iter().map(|r| r.seq).collect::<Vec<_>>(),
        (1..=15).collect::<Vec<_>>()
    );
    let mut projection = crate::structured::state::SessionState::new("fixture-agent".into());
    let now = Instant::now();
    for record in stream {
        let seq = record.seq;
        let event = SessionEvent::parse(&record.raw, "2.1.296");
        let received = Received {
            persisted: true,
            record,
            event,
            accepted: None,
        };
        if let Some(input) = received.state_input() {
            let update = projection.reduce(input, now, 1000, Duration::from_secs(30));
            if (4..=6).contains(&seq) {
                assert!(
                    update.needs_human.is_empty(),
                    "automatic close must clear S4 before another result"
                );
            }
        }
    }
}

#[test]
fn session_grant_suggestions_ignore_non_session_destinations() {
    for (n, rule) in [
        serde_json::json!({"destination":"userSettings"}),
        serde_json::json!({"destination":"projectSettings"}),
        serde_json::json!({"destination":"localSettings"}),
        serde_json::json!({"destination":"cliArg"}),
        serde_json::json!({"destination":null}),
        serde_json::json!({}),
    ]
    .into_iter()
    .enumerate()
    {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        let id = format!("invalid-{n}");
        let mut invalid_rule =
            serde_json::json!({"type":"addRules","behavior":"allow","rules":[{"toolName":"Bash"}]});
        for (key, value) in rule.as_object().unwrap() {
            invalid_rule[key] = value.clone();
        }
        grant_request(
            &mut output,
            &id,
            "Bash",
            serde_json::json!({}),
            Some(
                serde_json::json!([{"type":"addRules","behavior":"allow","destination":"session","rules":[{"toolName":"Bash"}]},invalid_rule]),
            ),
        );
        opened(&transport);
        assert_exact_suggestion_fallback(&root, &transport, &mut output, &mut input, &id);
    }
}

#[test]
fn session_grant_absent_null_and_empty_suggestions_use_exact_fallback() {
    for suggestions in [None, Some(Value::Null), Some(serde_json::json!([]))] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        grant_request(
            &mut output,
            "grant",
            "Bash",
            serde_json::json!({"command":"true"}),
            suggestions,
        );
        opened(&transport);
        close_with(
            &transport,
            &mut input,
            "grant",
            permissions::Decision::AllowAlways,
        );
        grant_request(
            &mut output,
            "match",
            "Bash",
            serde_json::json!({"command":"true"}),
            None,
        );
        opened(&transport);
        assert_eq!(
            serde_json::from_str::<Value>(&receive_after_accepted(&transport).record.raw).unwrap()
                ["source"],
            "session_grant"
        );
        assert_eq!(
            input_line(&transport, &mut input)["response"]["response"]["behavior"],
            "allow"
        );
    }
}

fn settings_snapshot(
    path: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "sessions" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            files.extend(settings_snapshot(&entry.path()));
        } else {
            files.insert(entry.path(), fs::read(entry.path()).unwrap());
        }
    }
    files
}

#[test]
fn session_grant_expires_on_resume_is_agent_isolated_and_writes_no_settings() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    for name in ["home/.claude", "config", "claude"] {
        let dir = root.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            br#"{"permissions":{"defaultMode":"default"}}"#,
        )
        .unwrap();
    }
    // Each nextest test is a process. All possible policy roots are fixtures.
    std::env::set_var("HOME", root.path().join("home"));
    std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
    std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
    let settings = settings_snapshot(root.path());
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    opened(&transport);
    close_with(
        &transport,
        &mut input,
        "request-example",
        permissions::Decision::AllowAlways,
    );
    let (other_stdout, mut other_output) = pipe();
    let (mut other_input, other_stdin) = pipe();
    let mut other_location = location(&root);
    other_location.agent = "other-agent".into();
    let other =
        Transport::start(other_stdout, other_stdin, other_location, "2.1.296".into()).unwrap();
    permission(&mut other_output, "request-example");
    opened(&other);
    let mut other_decision = decide("request-example", permissions::Decision::Deny, 1);
    other_decision.agent = "other-agent".into();
    call_decide(&other, &owner(), &other_decision).unwrap();
    receive(&other);
    assert_eq!(
        input_line(&other, &mut other_input)["response"]["response"]["behavior"],
        "deny"
    );
    drop(other_output);
    reader_done(&other).unwrap();
    drop(other);
    drop(output);
    reader_done(&transport).unwrap();
    transport.cancel();
    assert_eq!(
        worker_done(&transport, false).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    drop(transport);
    // Same agent and persisted session, but a new live runtime: no grant survives.
    let (resumed, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    assert_eq!(receive(&resumed).record.seq, 4);
    assert_eq!(receive(&resumed).record.seq, 5);
    assert_eq!(
        close_with(
            &resumed,
            &mut input,
            "request-example",
            permissions::Decision::Deny
        )["response"]["response"]["behavior"],
        "deny"
    );
    assert_eq!(
        settings_snapshot(root.path()),
        settings,
        "only session audit files may change"
    );
}

#[test]
fn every_book_close_immediately_clears_s4_needs_human() {
    use crate::api::agent_state::Phase;
    use crate::structured::state::{Input as StateInput, SessionState};
    for mode in [
        "allow",
        "deny",
        "cancel",
        "result",
        "interrupt",
        "eof",
        "stop",
    ] {
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let (transport, mut output, mut input) = start(&root);
        let mut state = SessionState::new("fixture-agent".into());
        let now = Instant::now();
        permission(&mut output, "request-example");
        let request = receive_after_accepted(&transport);
        let update = state.reduce(
            request.state_input().unwrap(),
            now,
            1000,
            Duration::from_secs(30),
        );
        assert_eq!(update.state.phase, Phase::NeedsHuman);
        assert_eq!(update.needs_human.len(), 1);
        let open = receive_after_accepted(&transport);
        // Audit open is bookkeeping, not another stdout progress event.
        assert!(open.state_input().is_none());
        match mode {
            "allow" | "deny" => {
                let decision = if mode == "allow" {
                    permissions::Decision::AllowOnce
                } else {
                    permissions::Decision::Deny
                };
                call_decide(
                    &transport,
                    &owner(),
                    &decide("request-example", decision, 1),
                )
                .unwrap();
                input_line(&transport, &mut input);
            }
            "cancel" => {
                writeln!(output, "{}", serde_json::json!({"type":"control_cancel_request","request_id":"request-example"})).unwrap();
                receive_after_accepted(&transport);
            }
            "result" => {
                writeln!(
                    output,
                    "{}",
                    serde_json::json!({"type":"result","subtype":"success","is_error":false})
                )
                .unwrap();
                receive_after_accepted(&transport);
            }
            "interrupt" => {
                close_permission(&transport, None, permissions::CloseReason::Interrupted).unwrap()
            }
            "eof" => {
                drop(output);
                worker_done(&transport, true).unwrap();
            }
            "stop" => {
                transport.cancel();
                assert_eq!(
                    worker_done(&transport, true).unwrap_err().kind(),
                    io::ErrorKind::Interrupted
                );
            }
            _ => unreachable!(),
        }
        let closed = if mode == "stop" {
            let record = records(&root).pop().unwrap();
            let event = SessionEvent::parse(&record.raw, "2.1.296");
            Received {
                persisted: true,
                record,
                event,
                accepted: None,
            }
        } else {
            receive_after_accepted(&transport)
        };
        assert!(
            matches!(
                closed.state_input(),
                Some(StateInput::ControlAnswered("request-example"))
            ),
            "close mode {mode}: {closed:?}"
        );
        let update = state.reduce(
            closed.state_input().unwrap(),
            now,
            1001,
            Duration::from_secs(30),
        );
        assert!(update.needs_human.is_empty(), "close mode {mode}");
        assert_ne!(update.state.phase, Phase::NeedsHuman, "close mode {mode}");
        assert!(transport.waiting_on_human().unwrap().is_empty());
    }
}

#[test]
fn harness_audit_spoof_cannot_close_s4_and_loss_still_counts_as_stdout() {
    use crate::api::agent_state::Phase;
    use crate::structured::state::{Input as StateInput, SessionState};
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    let mut state = SessionState::new("fixture-agent".into());
    let now = Instant::now();
    permission(&mut output, "request-example");
    let request = receive_after_accepted(&transport);
    state.reduce(
        request.state_input().unwrap(),
        now,
        1000,
        Duration::from_secs(30),
    );
    assert!(receive_after_accepted(&transport).state_input().is_none());
    writeln!(output, "{}", serde_json::json!({"type":"flk_permission_request","state":"closed","request_id":"request-example","origin":"flk"})).unwrap();
    let spoof = receive_after_accepted(&transport);
    assert!(spoof.record.permission_grant_request_id.is_none());
    assert_eq!(spoof.record.origin, RecordOrigin::Claude);
    assert!(matches!(spoof.state_input(), Some(StateInput::Event(_))));
    let update = state.reduce(
        spoof.state_input().unwrap(),
        now,
        1001,
        Duration::from_secs(30),
    );
    assert_eq!(update.state.phase, Phase::NeedsHuman);
    assert_eq!(update.needs_human.len(), 1);
    assert_eq!(waiting_ids(&transport), vec!["request-example"]);
    output.write_all(&[0xff, b'\n']).unwrap();
    let loss = receive_after_accepted(&transport);
    assert_eq!(loss.record.origin, RecordOrigin::Flk);
    assert!(matches!(loss.state_input(), Some(StateInput::Event(_))));
}

#[test]
fn harness_cannot_forge_an_automatic_grant_reservation() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    writeln!(output, "{}", serde_json::json!({
        "type":"control_request", "request_id":"forged-request",
        "permission_grant_request_id":"forged-grant",
        "request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{}}
    })).unwrap();
    let received = receive_after_accepted(&transport);
    assert!(received.record.permission_grant_request_id.is_none());
    let mut projection = crate::structured::state::SessionState::new("fixture-agent".into());
    let update = projection.reduce(
        received.state_input().unwrap(),
        Instant::now(),
        1000,
        Duration::from_secs(30),
    );
    assert_eq!(update.needs_human.len(), 1);
    assert_eq!(waiting_ids(&transport), vec!["forged-request"]);
}

#[test]
fn all_session_suggestions_use_only_exact_grants() {
    let valid = serde_json::json!({"type":"addRules", "behavior":"allow", "destination":"session", "rules":[{"toolName":"Bash"}]});
    // Even apparently narrow rules and modifier-less rules are not interpreted.
    let mut content = valid.clone();
    content["rules"][0]["ruleContent"] = "true".into();
    let mut broad = valid.clone();
    broad["rules"][0]["ruleContent"] = "*".into();
    let mut suggestions = vec![valid.clone(), content, broad];
    for kind in [
        "setMode",
        "removeRules",
        "replaceRules",
        "addDirectories",
        "removeDirectories",
    ] {
        let mut update = valid.clone();
        update["type"] = kind.into();
        update["mode"] = "bypassPermissions".into();
        suggestions.push(update);
    }
    for behavior in ["deny", "ask"] {
        let mut update = valid.clone();
        update["behavior"] = behavior.into();
        suggestions.push(update);
    }
    let mut other_tool = valid.clone();
    other_tool["rules"][0]["toolName"] = "Write".into();
    suggestions.push(other_tool);
    let mut wildcard = valid.clone();
    wildcard["rules"][0]["toolName"] = "*".into();
    suggestions.push(wildcard);
    let mut directories = valid.clone();
    directories["directories"] = serde_json::json!(["relative-fixture"]);
    suggestions.push(directories);
    let mut rule_extra = valid.clone();
    rule_extra["rules"][0]["directories"] = serde_json::json!(["relative-fixture"]);
    suggestions.push(rule_extra);
    for (n, update) in suggestions.into_iter().enumerate() {
        let id = format!("suggested-{n}");
        let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        for name in ["home/.claude", "config", "claude"] {
            let dir = root.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("settings.json"),
                br#"{"permissions":{"defaultMode":"default"}}"#,
            )
            .unwrap();
        }
        // nextest isolates each test process; all policy roots are fixtures.
        std::env::set_var("HOME", root.path().join("home"));
        std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
        std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
        let settings = settings_snapshot(root.path());
        let (transport, mut output, mut input) = start(&root);
        grant_request(
            &mut output,
            &id,
            "Bash",
            serde_json::json!({}),
            Some(serde_json::json!([valid, update])),
        );
        opened(&transport);
        assert_exact_suggestion_fallback(&root, &transport, &mut output, &mut input, &id);
        assert_eq!(settings_snapshot(root.path()), settings);
    }
}

// Start at the harness request and prove the decision closes it in one call,
// installs only the exact grant, and leaves a durable explanation of discarded rules.
fn assert_exact_suggestion_fallback(
    root: &SessionConfigDir,
    transport: &Transport,
    output: &mut File,
    input: &mut File,
    id: &str,
) {
    call_decide(
        transport,
        &owner(),
        &decide(
            id,
            permissions::Decision::AllowAlways,
            permission_handle(transport, id),
        ),
    )
    .unwrap();
    let close: Value = serde_json::from_str(&receive(transport).record.raw).unwrap();
    assert_eq!(close["state"], "closed");
    assert_eq!(close["decision"], "allow_always");
    assert_eq!(close["ignored_suggestions"]["request_id"], id);
    assert_eq!(close["ignored_suggestions"]["reason"], "not_forwarded");
    let response = input_line(transport, input);
    assert_eq!(response["response"]["response"]["behavior"], "allow");
    assert!(response["response"]["response"]
        .get("updatedPermissions")
        .is_none());
    assert!(transport.waiting_on_human().unwrap().is_empty());
    assert_eq!(
        call_decide(
            transport,
            &owner(),
            &decide(id, permissions::Decision::Deny, 1)
        ),
        Err(ErrorCode::PermissionRequestClosed)
    );
    grant_request(output, "exact-match", "Bash", serde_json::json!({}), None);
    opened(transport);
    let automatic: Value = serde_json::from_str(&receive(transport).record.raw).unwrap();
    assert_eq!(automatic["source"], "session_grant");
    assert_eq!(automatic["grant_request_id"], id);
    assert_eq!(
        input_line(transport, input)["response"]["response"]["behavior"],
        "allow"
    );
    grant_request(
        output,
        "different-input",
        "Bash",
        serde_json::json!({"command":"different"}),
        None,
    );
    opened(transport);
    assert_eq!(waiting_ids(transport), vec!["different-input"]);
    assert_eq!(
        close_with(
            transport,
            input,
            "different-input",
            permissions::Decision::Deny
        )["response"]["response"]["behavior"],
        "deny"
    );
    assert!(replay_open_permissions(root).is_empty());
    // The closed record references the full original suggestion bundle in open.
    let audit = records(root);
    let open: Value = serde_json::from_str(&audit[1].raw).unwrap();
    assert_eq!(open["request_id"], id);
    let harness: Value = serde_json::from_str(&audit[0].raw).unwrap();
    assert_eq!(
        open["request"]["permission_suggestions"],
        harness["request"]["permission_suggestions"]
    );
}

#[test]
fn session_grant_capacity_refuses_without_closing_or_sending() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    for n in 0..permissions::MAX_GRANTS {
        let id = format!("grant-{n}");
        grant_request(&mut output, &id, "Bash", serde_json::json!({"n":n}), None);
        opened(&transport);
        close_with(
            &transport,
            &mut input,
            &id,
            permissions::Decision::AllowAlways,
        );
    }
    grant_request(
        &mut output,
        "overflow",
        "Bash",
        serde_json::json!({"n":"overflow"}),
        Some(
            serde_json::json!([{"type":"addRules","behavior":"allow","destination":"session","rules":[{"toolName":"Bash"}]}]),
        ),
    );
    opened(&transport);
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide(
                "overflow",
                permissions::Decision::AllowAlways,
                permission_handle(&transport, "overflow")
            )
        ),
        Err(ErrorCode::PermissionGrantLimit)
    );
    assert_eq!(waiting_ids(&transport), vec!["overflow"]);
    // The first response is the later allow-once decision, not an implicit
    // allow-always answer or forwarded harness rules after the refusal.
    let response = close_with(
        &transport,
        &mut input,
        "overflow",
        permissions::Decision::AllowOnce,
    );
    assert_eq!(response["response"]["request_id"], "overflow");
    assert_eq!(response["response"]["response"]["behavior"], "allow");
    assert!(response["response"]["response"]
        .get("updatedPermissions")
        .is_none());
    assert!(replay_open_permissions(&root).is_empty());
}

// These tests have one response outstanding, so a buffered read cannot consume
// a later reply. Reject EOF, worker failure or a missing response within wait().
fn single_permission_response(transport: &Transport, input: &mut File) -> Value {
    private_fs::set_nonblocking(input).unwrap();
    let mut bytes = Vec::new();
    wait(transport, || {
        let mut chunk = [0; 4096];
        match input.read(&mut chunk) {
            Ok(0) => panic!("stdin closed before permission response"),
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                bytes.ends_with(b"\n").then(|| {
                    serde_json::from_slice(&bytes).expect("one complete permission response")
                })
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
            Err(error) => panic!("permission response read failed: {error}"),
        }
    })
}

#[test]
fn permission_session_survives_two_thousand_prompt_close_cycles() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    let mut last_request_seq = 0;
    for n in 0..2000 {
        let id = format!("cycle-{n}");
        permission(&mut output, &id);
        assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
        let open: Value = serde_json::from_str(&receive(&transport).record.raw).unwrap();
        assert_eq!(open["state"], "open");
        last_request_seq = open["request_seq"].as_u64().unwrap();
        call_decide(
            &transport,
            &owner(),
            &decide(&id, permissions::Decision::Deny, last_request_seq),
        )
        .unwrap();
        let close: Value = serde_json::from_str(&receive(&transport).record.raw).unwrap();
        assert_eq!(close["request_id"], id);
        assert_eq!(close["state"], "closed");
        let response = single_permission_response(&transport, &mut input);
        assert_eq!(response["response"]["request_id"], id);
        assert_eq!(response["response"]["response"]["behavior"], "deny");
    }
    assert!(matches!(
        transport.try_completion(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("cycle-1999", permissions::Decision::Deny, last_request_seq)
        ),
        Err(ErrorCode::PermissionRequestClosed)
    );
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("cycle-0", permissions::Decision::Deny, 1)
        ),
        Err(ErrorCode::PermissionRequestUnknown)
    );
    drop(output);
    reader_done(&transport).unwrap();
}

#[test]
fn open_permission_limit_denies_only_excess_requests_and_keeps_session_alive() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    for n in 0..16 {
        permission(&mut output, &format!("open-{n}"));
        receive(&transport);
        receive(&transport);
    }
    for n in 0..3 {
        let id = format!("excess-{n}");
        permission(&mut output, &id);
        assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
        let refused = receive(&transport);
        let audit: Value = serde_json::from_str(&refused.record.raw).unwrap();
        assert_eq!(audit["state"], "closed");
        assert_eq!(audit["error_code"], "permission_request_limit");
        assert_eq!(audit["decision"], "deny");
        assert_eq!(audit["request_id"], id);
        assert!(
            matches!(refused.state_input(), Some(crate::structured::state::Input::ControlAnswered(value)) if value == id)
        );
        let response = single_permission_response(&transport, &mut input);
        assert_eq!(response["response"]["request_id"], id);
        assert_eq!(response["response"]["response"]["behavior"], "deny");
        assert_eq!(
            response["response"]["response"]["message"],
            "permission_request_limit"
        );
        assert_eq!(
            call_decide(
                &transport,
                &owner(),
                &decide(
                    &id,
                    permissions::Decision::Deny,
                    audit["request_seq"].as_u64().unwrap()
                )
            ),
            Err(ErrorCode::PermissionRequestClosed)
        );
        assert_eq!(transport.waiting_on_human().unwrap().len(), 16);
    }
    call_decide(
        &transport,
        &owner(),
        &decide("open-0", permissions::Decision::AllowOnce, 1),
    )
    .unwrap();
    receive(&transport);
    assert_eq!(
        single_permission_response(&transport, &mut input)["response"]["response"]["behavior"],
        "allow"
    );
    // The second response proves that the writer already released open-0.
    call_decide(
        &transport,
        &owner(),
        &decide(
            "open-1",
            permissions::Decision::Deny,
            permission_handle(&transport, "open-1"),
        ),
    )
    .unwrap();
    receive(&transport);
    single_permission_response(&transport, &mut input);
    assert_eq!(transport.waiting_on_human().unwrap().len(), 14);
    permission(&mut output, "following-request");
    receive(&transport);
    let open: Value = serde_json::from_str(&receive(&transport).record.raw).unwrap();
    assert_eq!(open["state"], "open");
    assert_eq!(transport.waiting_on_human().unwrap().len(), 15);
    assert!(matches!(
        transport.try_completion(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn oversized_request_at_open_limit_is_denied_without_retiring_other_requests() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    for n in 0..permissions::MAX_OPEN_PERMISSION_REQUESTS {
        permission(&mut output, &format!("open-{n}"));
        receive(&transport);
        receive(&transport);
    }
    let raw = serde_json::json!({"type":"control_request","request_id":"oversized-excess", "request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{"data":"x".repeat(MAX_LINE_BYTES)}}});
    let producer = thread::spawn(move || {
        writeln!(output, "{raw}").unwrap();
        output
    });
    let summary = receive(&transport);
    assert!(summary.record.raw.len() < MAX_LINE_BYTES);
    let close: Value = serde_json::from_str(&receive(&transport).record.raw).unwrap();
    assert_eq!(close["request_id"], "oversized-excess");
    assert_eq!(
        close["error_code"],
        ErrorCode::PermissionRequestLimit.as_str()
    );
    assert_eq!(
        single_permission_response(&transport, &mut input)["response"]["response"]["behavior"],
        "deny"
    );
    assert_eq!(
        transport.waiting_on_human().unwrap().len(),
        permissions::MAX_OPEN_PERMISSION_REQUESTS
    );
    assert!(matches!(
        transport.try_completion(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let _output = producer.join().unwrap();
}

#[test]
fn permission_handle_reaches_audit_snapshot_and_decision_and_advances_on_resume() {
    use crate::structured::state::SessionState;
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let mut previous = 0;
    for _ in 0..2 {
        let (transport, mut output, mut input) = start(&root);
        writeln!(output, "{}", serde_json::json!({"type":"control_request", "request_id":"reused",
            "request_seq":previous, "request":{"subtype":"can_use_tool", "tool_name":"Bash", "tool_use_id":"tool-example", "input":{}}})).unwrap();
        let raw = receive(&transport);
        let seq = raw.record.permission_request_seq.unwrap();
        assert!(seq > previous);
        let mut state = SessionState::new("fixture-agent".into());
        let now = Instant::now();
        let update = state.reduce(raw.state_input().unwrap(), now, 0, Duration::from_secs(60));
        assert_eq!(update.permission_requests, [("reused".into(), seq)]);
        assert_eq!(
            transport.waiting_on_human().unwrap(),
            update.permission_requests
        );
        let audit: Value = serde_json::from_str(&receive(&transport).record.raw).unwrap();
        assert_eq!(audit["request_seq"], seq);
        assert_eq!(
            call_decide(
                &transport,
                &owner(),
                &decide("reused", permissions::Decision::AllowAlways, previous)
            ),
            Err(ErrorCode::PermissionRequestUnknown)
        );
        call_decide(
            &transport,
            &owner(),
            &decide("reused", permissions::Decision::Deny, seq),
        )
        .unwrap();
        let closed = receive(&transport);
        assert_eq!(
            serde_json::from_str::<Value>(&closed.record.raw).unwrap()["request_seq"],
            seq
        );
        assert!(state
            .reduce(
                closed.state_input().unwrap(),
                now,
                0,
                Duration::from_secs(60)
            )
            .permission_requests
            .is_empty());
        assert_eq!(
            single_permission_response(&transport, &mut input)["response"]["response"]["behavior"],
            "deny"
        );
        drop(output);
        reader_done(&transport).unwrap();
        transport.cancel();
        assert_eq!(
            worker_done(&transport, false).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        previous = seq;
    }
}

#[test]
fn blob_references_resolve_while_partial_deltas_are_live_only_and_restart_seq_is_dense() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut stdout, _input) = start(&root);
    let partial = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"text":"partial"}}}"#;
    writeln!(stdout, "{partial}").unwrap();
    let live = receive(&transport);
    assert!(!live.persisted);
    assert_eq!(live.record.seq, 0);
    assert_eq!(live.record.raw, partial);
    assert!(records(&root).is_empty());
    let block = serde_json::json!({"type":"thinking","thinking":"x".repeat(3000)});
    let raw =
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[block]}})
            .to_string();
    writeln!(stdout, "{raw}").unwrap();
    let live = receive(&transport);
    assert!(live.persisted);
    assert_eq!(live.record.seq, 1);
    assert_eq!(live.record.raw, raw, "live data stays complete");
    let stored = records(&root);
    assert_eq!(stored.len(), 1);
    let projected: Value = serde_json::from_str(&stored[0].raw).unwrap();
    let reference: super::super::blobs::BlobRef =
        serde_json::from_value(projected["message"]["content"][0].clone()).unwrap();
    let bytes = super::super::blobs::BlobStore::open(root.path())
        .unwrap()
        .get(&reference.blob)
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), block);
    drop(stdout);
    reader_done(&transport).unwrap();
    drop(transport);
    let (transport, mut stdout, _input) = start(&root);
    writeln!(stdout, "{}", user("after restart")).unwrap();
    assert_eq!(receive(&transport).record.seq, 2);
    drop(stdout);
    reader_done(&transport).unwrap();
    assert_eq!(
        records(&root).iter().map(|r| r.seq).collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn oversized_and_invalid_utf8_partial_summaries_never_enter_durable_stream() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut stdout, _input) = start(&root);
    let prefix = br#"{"type":"stream_event","event":{"type":"content_block_delta","text":""#;
    let mut oversized = prefix.to_vec();
    oversized.resize(MAX_LINE_BYTES + 1, b'x');
    oversized.extend_from_slice(b"\"}}\n");
    stdout.write_all(&oversized).unwrap();
    let live = receive(&transport);
    assert!(!live.persisted);
    assert_eq!(live.record.origin, RecordOrigin::Flk);
    assert_eq!(live.record.seq, 0);
    let mut invalid = prefix.to_vec();
    invalid.extend_from_slice(b"\xff\"}}\n");
    stdout.write_all(&invalid).unwrap();
    let live = receive(&transport);
    assert!(!live.persisted);
    assert_eq!(live.record.origin, RecordOrigin::Flk);
    assert_eq!(live.record.seq, 0);
    drop(stdout);
    reader_done(&transport).unwrap();
    assert!(records(&root).is_empty());
}

#[test]
fn history_provenance_survives_restart_and_version_change() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"future-history","counter":1})
    )
    .unwrap();
    let original = receive(&transport).record;
    assert_eq!(original.known, Some(false));
    assert!(original.shape.as_ref().unwrap().0.contains("2.1.296"));
    let mut extended = serde_json::to_value(&original).unwrap();
    extended["future_metadata"] = serde_json::json!({"new":true});
    let reread: StreamRecord = serde_json::from_value(extended).unwrap();
    assert_eq!(reread.shape, original.shape);
    drop(output);
    reader_done(&transport).unwrap();
    drop(transport);
    let (stdout, mut output) = pipe();
    let (_input, stdin) = pipe();
    let restarted = Transport::start(
        stdout,
        stdin,
        location(&root),
        "different-observed-version".into(),
    )
    .unwrap();
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"future-history","counter":1})
    )
    .unwrap();
    let later = receive(&restarted).record;
    assert_eq!(later.seq, original.seq + 1);
    assert_ne!(later.shape, original.shape);
    let mut cut = restarted.history.clone().subscribe(None).unwrap();
    let crate::structured::history::Item::Record(replayed) = cut.page().unwrap() else {
        panic!("old record")
    };
    assert_eq!(replayed.seq, original.seq);
    assert_eq!(replayed.known, original.known);
    assert_eq!(replayed.shape, original.shape);
    drop(output);
    reader_done(&restarted).unwrap();
}

#[test]
fn history_and_state_receiver_share_permission_audits_but_not_partial_deltas() {
    use crate::structured::history::Item;
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    let delta = include_str!("../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .find(|line| line.contains("stream_event"))
        .unwrap();
    writeln!(output, "{delta}").unwrap();
    assert!(!receive(&transport).persisted);
    let mut cut = transport.history.clone().subscribe(None).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(0)));
    writeln!(output,"{}",serde_json::json!({"type":"control_request","request_id":"history-permission",
        "request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{"command":"true"}}})).unwrap();
    let request = receive(&transport);
    let open = receive(&transport);
    assert!(matches!(request.event, SessionEvent::CanUseTool(_)));
    assert!(open.state_input().is_none());
    drop(output);
    reader_done(&transport).unwrap();
    let closed = receive(&transport);
    assert!(matches!(
        closed.state_input(),
        Some(crate::structured::state::Input::ControlAnswered(
            "history-permission"
        ))
    ));
    let mut states = Vec::new();
    for expected in 1..=3 {
        let Item::Record(record) = cut.page().unwrap() else {
            panic!("stored permission record")
        };
        assert_eq!(record.seq, expected);
        let value: Value = serde_json::from_str(&record.raw).unwrap();
        if value["type"] == "flk_permission_request" {
            states.push(value["state"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(states, vec!["open", "closed"]);
    assert!(matches!(cut.page().unwrap(), Item::End));
    assert_eq!(records(&root).len(), 3);
}

#[test]
fn history_live_publication_uses_persisted_blob_projection() {
    use crate::structured::history::Item;
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"user","message":{}})
    )
    .unwrap();
    receive(&transport);
    let mut cut = transport.history.clone().subscribe(Some(1)).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(1)));
    let content = "large result".repeat(400);
    let input = serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-example","content":content}]}});
    writeln!(output, "{input}").unwrap();
    let live = receive(&transport);
    assert_eq!(
        serde_json::from_str::<Value>(&live.record.raw).unwrap(),
        input
    );
    let Item::Record(stored) = cut.page().unwrap() else {
        panic!("durable live continuation")
    };
    assert_eq!(stored.seq, 2);
    assert_eq!(stored.known, Some(true));
    let projected: Value = serde_json::from_str(&stored.raw).unwrap();
    assert!(projected["message"]["content"][0]["blob"].is_string());
    assert!(projected["message"]["content"][0].get("content").is_none());
    assert_eq!(stored.raw, records(&root)[1].raw);
    drop(output);
    reader_done(&transport).unwrap();
}

#[test]
fn history_partial_deltas_never_advance_a_durable_barrier() {
    use crate::structured::history::Item;
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, _input) = start(&root);
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"user","message":{}})
    )
    .unwrap();
    receive(&transport);
    let mut cut = transport.history.clone().subscribe(Some(1)).unwrap();
    assert!(matches!(cut.page().unwrap(), Item::Synced(1)));
    let delta = include_str!("../../../tests/fixtures/claude-session/events.jsonl")
        .lines()
        .find(|line| line.contains("stream_event"))
        .unwrap();
    writeln!(output, "{delta}").unwrap();
    assert!(!receive(&transport).persisted);
    assert!(matches!(cut.page().unwrap(), Item::Pending));
    let mut fresh = transport.history.clone().subscribe(Some(1)).unwrap();
    assert!(matches!(fresh.page().unwrap(), Item::Synced(1)));
    assert_eq!(records(&root).len(), 1);
    drop(output);
    reader_done(&transport).unwrap();
}

#[test]
fn admission_replay_is_skipped_and_stdout_cannot_forge_acceptance() {
    for origin in [RecordOrigin::Flk, RecordOrigin::Claude] {
        let record = StreamRecord {
            seq: 1,
            known: None,
            shape: None,
            received_at_ms: 1000,
            origin,
            result_subtype: None,
            permission_grant_request_id: None,
            permission_request_seq: None,
            raw: serde_json::json!({"type":"flk_input_accepted", "at_ms":1000}).to_string(),
        };
        let event = SessionEvent::parse(&record.raw, "2.1.296");
        let received = Received {
            persisted: true,
            record,
            event,
            accepted: None,
        };
        if origin == RecordOrigin::Flk {
            assert!(
                received.state_input().is_none(),
                "replay has no launch clock"
            );
        } else {
            let input = received.state_input().expect("stdout is progress");
            assert!(matches!(&input, crate::structured::state::Input::Event(_)));
            let now = Instant::now();
            let mut reducer = crate::structured::state::SessionState::new("fixture-agent".into());
            assert!(reducer
                .reduce(input, now, 1000, Duration::from_secs(1))
                .observe_at
                .is_none());
        }
    }
}

#[test]
fn corrupt_blob_is_repaired_while_the_session_keeps_streaming() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut stdout, _input) = start(&root);
    let block = serde_json::json!({"type":"thinking","thinking":"x".repeat(3000)});
    let bytes = serde_json::to_vec(&block).unwrap();
    let store = super::super::blobs::BlobStore::open(root.path()).unwrap();
    let reference = store.put(bytes.as_slice(), bytes.len() as u64).unwrap();
    fs::write(root.path().join("blobs").join(&reference.blob), b"damaged").unwrap();
    let raw = serde_json::json!({"type":"assistant","message":{"content":[block]}}).to_string();
    writeln!(stdout, "{raw}").unwrap();
    writeln!(stdout, "{}", user("after repair")).unwrap();
    // EOF makes reader success or failure observable without waiting for absent output.
    drop(stdout);
    reader_done(&transport).expect("a repaired blob must not kill the stream reader");
    let first = receive(&transport);
    assert_eq!(first.record.raw, raw);
    assert!(first.persisted);
    assert_eq!(first.record.seq, 1);
    let second = receive(&transport);
    assert_eq!(
        serde_json::from_str::<Value>(&second.record.raw).unwrap(),
        user("after repair")
    );
    assert_eq!(second.record.seq, 2);
    let stored = records(&root);
    assert_eq!(stored.len(), 2);
    let projected: Value = serde_json::from_str(&stored[0].raw).unwrap();
    assert_eq!(projected["message"]["content"][0]["blob"], reference.blob);
    assert_eq!(store.get(&reference.blob).unwrap(), bytes);
}

#[test]
fn disconnected_acceptance_observer_requests_stop_without_admitting_bytes() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, _stdout, _input) = start(&root);
    let mut handle = transport.input_handle();
    let (sender, receiver) = mpsc::sync_channel(1);
    drop(receiver);
    handle.accepted = sender;
    let stop = crate::structured::supervision::StopRequest::default();
    let error = handle
        .try_submit_observed(&user("first"), || stop.request())
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(stop.requested());
    assert!(handle.cancel.load(Ordering::Acquire));
    assert!(!handle.input_inflight());
}

#[test]
fn stored_permission_pair_replays_as_known_events() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "typed-request");
    receive(&transport);
    receive(&transport);
    let seq = permission_handle(&transport, "typed-request");
    call_decide(
        &transport,
        &owner(),
        &decide("typed-request", permissions::Decision::Deny, seq),
    )
    .unwrap();
    receive(&transport);
    single_permission_response(&transport, &mut input);
    let audits: Vec<_> = records(&root)
        .into_iter()
        .filter(|record| {
            serde_json::from_str::<Value>(&record.raw).unwrap()["type"] == "flk_permission_request"
        })
        .collect();
    assert_eq!(audits.len(), 2);
    let examples: Vec<Value> =
        include_str!("../../../web/client/test/fixtures/wire/permission-decision.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter_map(|line| line.get("frame").and_then(|frame| frame.get("ev")).cloned())
            .collect();
    assert_eq!(examples.len(), 2);
    for (record, example) in audits.into_iter().zip(examples) {
        assert_eq!(
            serde_json::from_str::<Value>(&record.raw).unwrap(),
            example,
            "producer/fixture drift"
        );
        assert_eq!(
            record.raw,
            serde_json::to_string(&example).unwrap(),
            "wire byte drift"
        );
        let event = SessionEvent::parse(&record.raw, "fixture");
        assert!(
            !matches!(event, SessionEvent::Unknown { .. }),
            "permission audit must be known: {}",
            record.raw
        );
        let known: crate::structured::claude::KnownSessionEvent =
            serde_json::from_str(&record.raw).unwrap();
        assert_eq!(
            serde_json::to_value(known).unwrap(),
            serde_json::from_str::<Value>(&record.raw).unwrap()
        );
        let received = Received {
            record,
            event,
            persisted: true,
            accepted: None,
        };
        if example["state"] == "closed" {
            assert!(matches!(
                received.state_input(),
                Some(crate::structured::state::Input::ControlAnswered(
                    "typed-request"
                ))
            ));
        } else {
            assert!(received.state_input().is_none());
        }
    }
}

fn seed_permission_crash(root: &SessionConfigDir, automatic: bool) -> u64 {
    let request: crate::structured::claude::ToolRequest = serde_json::from_value(serde_json::json!({"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{"command":"true"}})).unwrap();
    let mut book = Book::new("fixture-agent".into());
    let mut audits = Vec::new();
    if automatic {
        audits.push(book.observe("grant-origin", request.clone()).unwrap());
        let args = permissions::AgentDecide {
            agent: "fixture-agent".into(),
            request_id: "grant-origin".into(),
            request_seq: 1,
            decision: permissions::Decision::AllowAlways,
            message: None,
        };
        let answer = book.prepare(&owner(), &args).unwrap();
        assert!(book.begin_answer(&answer.id, answer.request_seq));
        audits.push((*answer.audit).clone());
        book.audited(&answer);
        book.written(answer);
        book.resume_after(audits.len() as u64);
    }
    let open = book.observe("orphan", request).unwrap();
    let request_seq = open.request_seq;
    audits.push(open);
    if automatic {
        let reserved = book
            .automatic("orphan")
            .unwrap()
            .expect("exact grant reserved");
        assert_eq!(reserved.audit.grant_request_id(), Some("grant-origin"));
        assert!(book.waiting().is_empty());
    }
    write_permission_crash_prefix(root, audits);
    // Crash cut: the real producer's open is fsynced, with no terminal or reserved-answer close.
    request_seq
}

fn write_permission_crash_prefix(
    root: &SessionConfigDir,
    audits: Vec<crate::structured_wire::PermissionRecord>,
) {
    let opened = open_stream(location(root), &AtomicBool::new(false)).unwrap();
    let (mut file, mut seq) = (opened.0, opened.1);
    let blobs = super::super::blobs::BlobStore::open(root.path()).unwrap();
    let history = super::super::history::History::default();
    history
        .initialize(&file, seq, file.metadata().unwrap().len())
        .unwrap();
    for audit in audits {
        let line = serde_json::to_vec(&serde_json::to_value(audit).unwrap()).unwrap();
        persist(
            &mut file,
            &mut seq,
            &line,
            line.len() as u64,
            "fixture",
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            true,
            None,
            None,
            (&blobs, location(root).storage.blob_min_bytes, &history),
        )
        .unwrap();
    }
}

fn end_restarted_transport(transport: &Transport, output: File) {
    drop(output);
    reader_done(transport).unwrap();
    transport.cancel();
    assert_eq!(
        worker_done(transport, false).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn restart_closes_orphan_before_admitting_a_new_control_request() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let old_seq = seed_permission_crash(&root, false);
    let (transport, mut output, _input) = start(&root);
    permission(&mut output, "orphan");
    let recovered = receive(&transport);
    let value: Value = serde_json::from_str(&recovered.record.raw).unwrap();
    assert_eq!(value["reason"], "session_ended");
    assert_eq!(value["request_id"], "orphan");
    assert_eq!(value["request_seq"], old_seq);
    assert!(value.get("decision").is_none());
    assert!(matches!(
        recovered.event,
        SessionEvent::PermissionRequest(_)
    ));
    let raw = receive(&transport);
    assert_eq!(raw.record.origin, RecordOrigin::Claude);
    assert!(raw.record.permission_request_seq.unwrap() > recovered.record.seq);
    receive(&transport);
    end_restarted_transport(&transport, output);
    let closes: Vec<Value> = records(&root)
        .iter()
        .map(|record| serde_json::from_str(&record.raw).unwrap())
        .filter(|raw: &Value| {
            raw["request_id"] == "orphan"
                && raw["state"] == "closed"
                && raw["request_seq"] == old_seq
        })
        .collect();
    assert_eq!(closes.len(), 1);
}

#[test]
fn second_restart_appends_no_duplicate_permission_close() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    seed_permission_crash(&root, false);
    let (transport, output, _input) = start(&root);
    drop(output);
    reader_done(&transport).unwrap();
    transport.cancel();
    worker_done(&transport, false).unwrap_err();
    let before = fs::read(
        root.path()
            .join("sessions/fixture-agent/fixture-session.jsonl"),
    )
    .unwrap();
    assert_eq!(records(&root).len(), 2);
    let (transport, output, _input) = start(&root);
    end_restarted_transport(&transport, output);
    assert_eq!(
        fs::read(
            root.path()
                .join("sessions/fixture-agent/fixture-session.jsonl")
        )
        .unwrap(),
        before
    );
}

#[test]
fn interrupted_automatic_grant_is_closed_after_restart() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let request_seq = seed_permission_crash(&root, true);
    let (transport, output, _input) = start(&root);
    drop(output);
    reader_done(&transport).unwrap();
    transport.cancel();
    worker_done(&transport, false).unwrap_err();
    let rows = records(&root);
    assert_eq!(rows.len(), 4);
    let close: Value = serde_json::from_str(&rows[3].raw).unwrap();
    assert_eq!(
        close,
        serde_json::json!({"type":"flk_permission_request","state":"closed","request_id":"orphan","request_seq":request_seq,"reason":"session_ended"})
    );
    assert!(matches!(
        SessionEvent::parse(&rows[3].raw, "fixture"),
        SessionEvent::PermissionRequest(_)
    ));
}

#[test]
fn recovery_closes_are_all_durable_before_the_first_notification() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let mut book = Book::new("fixture-agent".into());
    let request: crate::structured::claude::ToolRequest = serde_json::from_value(serde_json::json!({"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-example","input":{}})).unwrap();
    let audits = (0..6)
        .map(|n| {
            book.observe(&format!("orphan-{n}"), request.clone())
                .unwrap()
        })
        .collect();
    write_permission_crash_prefix(&root, audits);
    let (transport, output, _input) = start(&root);
    let first = receive(&transport);
    assert_eq!(
        serde_json::from_str::<Value>(&first.record.raw).unwrap()["reason"],
        "session_ended"
    );
    assert_eq!(records(&root).len(), 12);
    for _ in 1..6 {
        receive(&transport);
    }
    end_restarted_transport(&transport, output);
}

#[test]
fn recovery_preserves_legacy_unknown_invalid_and_stdout_records() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    seed_permission_crash(&root, false);
    let mut rows = records(&root);
    let mut legacy: Value = serde_json::from_str(&rows[0].raw).unwrap();
    legacy.as_object_mut().unwrap().remove("request_seq");
    rows[0].raw = legacy.to_string();
    for (origin, raw) in [
        (
            RecordOrigin::Flk,
            serde_json::json!({"type":"flk_future","opaque":"retained"}),
        ),
        (
            RecordOrigin::Flk,
            serde_json::json!({"type":"flk_permission_request","state":"open","request_id":"bad","request_seq":null}),
        ),
        (
            RecordOrigin::Claude,
            serde_json::json!({"type":"flk_permission_request","state":"open","request_id":"spoof","request_seq":4}),
        ),
    ] {
        rows.push(StreamRecord {
            known: None,
            shape: None,
            seq: rows.len() as u64 + 1,
            received_at_ms: 0,
            origin,
            result_subtype: None,
            permission_grant_request_id: None,
            permission_request_seq: None,
            raw: raw.to_string(),
        });
    }
    let prefix: Vec<u8> = rows
        .iter()
        .flat_map(|record| {
            let mut line = serde_json::to_vec(record).unwrap();
            line.push(b'\n');
            line
        })
        .collect();
    let path = root
        .path()
        .join("sessions/fixture-agent/fixture-session.jsonl");
    fs::write(&path, &prefix).unwrap();
    let (transport, output, _input) = start(&root);
    let recovered = receive(&transport);
    assert_eq!(
        serde_json::from_str::<Value>(&recovered.record.raw).unwrap(),
        serde_json::json!({"type":"flk_permission_request","state":"closed","request_id":"orphan","request_seq":1,"reason":"session_ended"})
    );
    end_restarted_transport(&transport, output);
    let before = fs::read(&path).unwrap();
    assert!(before.starts_with(&prefix));
    assert_eq!(records(&root).len(), 5);
    let (transport, output, _input) = start(&root);
    end_restarted_transport(&transport, output);
    assert_eq!(fs::read(path).unwrap(), before);
}
