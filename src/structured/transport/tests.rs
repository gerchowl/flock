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
    let deadline = Instant::now() + Duration::from_secs(10);
    transport
        .output
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| {
            let completion = transport
                .completion
                .recv_timeout(deadline.saturating_duration_since(Instant::now()));
            panic!("missing stdout record: {error}; worker: {completion:?}")
        })
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
    assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
    let open = receive(&transport).record;
    assert_eq!(open.origin, RecordOrigin::Flk);
    let value: Value = serde_json::from_str(&open.raw).unwrap();
    assert_eq!(value["type"], "flk_permission_request");
    assert_eq!(value["state"], "open");
    assert_eq!(value["request_id"], "permission-example");
    assert_eq!(records(&root).len(), 2);
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
        let received = receive(&transport);
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
    let event = receive(&transport);
    assert!(matches!(event.event, SessionEvent::Assistant(_)));
    assert_eq!(event.record.raw, expected);
    let event = receive(&transport);
    assert!(matches!(event.event, SessionEvent::Unknown { .. }));
    let summary: Value = serde_json::from_str(&event.record.raw).expect("summary");
    assert_eq!(summary["type"], "flk_oversize");
    assert_eq!(summary["bytes"], MAX_LINE_BYTES + 99);
    let event = receive(&transport);
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
        let received = receive(&transport);
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
        receive(&transport);
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
        let loss = receive(&transport);
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
        receive(&transport);
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
    receive(&transport);
    let error = transport
        .try_submit(&user("unsafe retry"))
        .expect_err("unknown boundary");
    assert_ne!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("structured_turn_end_unknown"));
    let mut stdout = producer.join().expect("producer");
    stdout
        .write_all(b"{\"type\":\"result\",\"subtype\":\"success\"}\n")
        .expect("confirmed boundary");
    receive(&transport);
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
        assert_eq!(receive(&transport).record.seq, seq);
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
        false
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
    let (mut file, _) =
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
        let (mut file, _) = open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
        let mut good = Vec::new();
        for seq in 1..=2 {
            let record = serde_json::json!({"seq":seq,"received_at_ms":1,"origin":"claude","result_subtype":null,"raw":"prior"});
            serde_json::to_writer(&mut good, &record).expect("fixture record");
            good.push(b'\n');
        }
        file.write_all(&good).expect("good records");
        file.write_all(tail).expect("torn tail");
        drop(file);
        let (file, seq) =
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
        assert_eq!(receive(&transport).record.seq, 3);
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
    let spoof = receive(&transport);
    let genuine = receive(&transport);
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
    let (mut file, _) = open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
    file.write_all(&vec![b'x'; RECORD_LIMIT as usize + 99])
        .expect("oversized torn tail");
    drop(file);
    let (file, seq) =
        open_stream(location(&root), &AtomicBool::new(false)).expect("repair oversized tail");
    assert_eq!(seq, 0);
    assert_eq!(file.metadata().expect("size").len(), 0);
}

#[test]
fn malformed_newline_terminated_final_record_is_corruption() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).expect("fixture root");
    let (mut file, _) = open_stream(location(&root), &AtomicBool::new(false)).expect("stream");
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
fn decide(id: &str, decision: permissions::Decision) -> AgentDecide {
    AgentDecide {
        agent: "fixture-agent".into(),
        request_id: id.into(),
        decision,
        message: Some("operator reason".into()),
    }
}
fn owner() -> Principal<'static> {
    Principal::Token {
        subject: "fixture-client",
        operate: true,
    }
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
        receive(&transport);
        receive(&transport);
        assert_eq!(
            transport.waiting_on_human().unwrap(),
            vec!["request-example"]
        );
        call_decide(&transport, &owner(), &decide("request-example", decision)).unwrap();
        let response = input_line(&transport, &mut input);
        let audit = records(&root);
        assert_eq!(
            audit.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let closed: Value = serde_json::from_str(&audit[2].raw).unwrap();
        assert_eq!(closed["state"], "closed");
        assert_eq!(closed["decision"], serde_json::to_value(decision).unwrap());
        assert_eq!(closed["principal"], "fixture-client");
        assert_eq!(audit[2].origin, RecordOrigin::Flk);
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
        assert_eq!(published.seq, 3);
        assert!(
            transport.input_inflight(),
            "a permission answer is not a result"
        );
        assert_eq!(
            transport.try_submit(&user("second")).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            call_decide(&transport, &owner(), &decide("request-example", decision)),
            Err(ErrorCode::PermissionRequestClosed)
        );
    }
}

#[test]
fn open_permission_blocks_input_after_result_and_ignores_harness_audit_spoof() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive(&transport);
    receive(&transport);
    writeln!(output,"{}",serde_json::json!({"type":"flk_permission_request","state":"closed","request_id":"request-example"})).unwrap();
    assert_eq!(receive(&transport).record.origin, RecordOrigin::Claude);
    writeln!(
        output,
        "{}",
        serde_json::json!({"type":"result","subtype":"success","is_error":false})
    )
    .unwrap();
    receive(&transport);
    assert!(!transport.input_inflight());
    assert_eq!(
        transport.try_submit(&user("second")).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    call_decide(
        &transport,
        &owner(),
        &decide("request-example", permissions::Decision::Deny),
    )
    .unwrap();
    receive(&transport);
    assert_eq!(
        input_line(&transport, &mut input)["type"],
        "control_response"
    );
    wait(&transport, || {
        transport.waiting_on_human().ok().filter(Vec::is_empty)
    });
    wait(&transport, || transport.try_submit(&user("second")).ok());
    assert_eq!(input_line(&transport, &mut input), user("second"));
}

#[test]
fn refused_decisions_do_not_write_or_close_and_oversized_response_can_retry() {
    let root = SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let (transport, mut output, mut input) = start(&root);
    permission(&mut output, "request-example");
    receive(&transport);
    receive(&transport);
    for principal in [
        Principal::Operator,
        Principal::Lease("fixture-agent"),
        Principal::Agent("other-agent"),
    ] {
        assert_eq!(
            call_decide(
                &transport,
                &principal,
                &decide("request-example", permissions::Decision::Deny)
            ),
            Err(ErrorCode::NotAuthorized)
        );
    }
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("unknown", permissions::Decision::Deny)
        ),
        Err(ErrorCode::PermissionRequestUnknown)
    );
    assert_eq!(
        call_decide(
            &transport,
            &owner(),
            &decide("request-example", permissions::Decision::AllowAlways)
        ),
        Err(ErrorCode::PermissionDecisionUnsupported)
    );
    let mut huge = decide("request-example", permissions::Decision::Deny);
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
        &decide("request-example", permissions::Decision::Deny),
    )
    .unwrap();
    receive(&transport);
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
    receive(&transport);
    receive(&transport);
    // Exercise the writer's queue independently of the admission gate.
    transport
        .input
        .input
        .as_ref()
        .unwrap()
        .try_send(Input::User(encode(&user("queued")).unwrap()))
        .unwrap_or_else(|_| panic!("queue admission"));
    call_decide(
        &transport,
        &owner(),
        &decide("request-example", permissions::Decision::Deny),
    )
    .unwrap();
    receive(&transport);
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
    let (mut file, mut seq) = open_stream(location(&root), &cancel).unwrap();
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
    )
    .unwrap();
    assert!(matches!(
        rx.recv().unwrap().event,
        SessionEvent::Unknown { .. }
    ));
    drop(file);
    let (_file, seq) = open_stream(location(&root), &cancel).unwrap();
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
            handle.agent_decide(&owner(), &decide(&id, permissions::Decision::Deny)),
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
        .prepare(&owner(), &decide("request-4", permissions::Decision::Deny))
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
            &decide("request-example", permissions::Decision::Deny),
        )
        .unwrap();
    let control = Control {
        bytes: encode(&response.response).unwrap(),
        answer: response,
    };
    let (input, rx) = mpsc::sync_channel(4);
    let (_decisions, decisions) = mpsc::sync_channel(4);
    let plane = ControlPlane {
        book: Arc::new(parking_lot::Mutex::new(book)),
        input,
        decisions,
    };
    let (tx, notifications) = mpsc::sync_channel(4);
    assert!(answer(
        &mut file,
        &mut 0,
        control,
        "2.1.296",
        &tx,
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &plane
    )
    .is_err());
    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert!(matches!(
        notifications.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert_eq!(plane.book.lock().waiting(), vec!["request-example"]);
}
