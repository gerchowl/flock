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

fn wait<T>(mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "worker exceeded fixture deadline"
        );
        thread::sleep(POLL);
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
    wait(|| match transport.try_completion() {
        Ok(Completion::Reader(result)) => Some(result),
        Ok(Completion::Writer(_)) | Err(mpsc::TryRecvError::Empty) => None,
        Err(e) => panic!("missing reader completion: {e}"),
    })
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
        let received = wait(|| transport.try_receive().ok());
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
    let event = wait(|| transport.try_receive().ok());
    assert!(matches!(event.event, SessionEvent::Assistant(_)));
    assert_eq!(event.record.raw, expected);
    let event = wait(|| transport.try_receive().ok());
    assert!(matches!(event.event, SessionEvent::Unknown { .. }));
    let summary: Value = serde_json::from_str(&event.record.raw).expect("summary");
    assert_eq!(summary["type"], "flk_oversize");
    assert_eq!(summary["bytes"], MAX_LINE_BYTES + 99);
    let event = wait(|| transport.try_receive().ok());
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
    let received = wait(|| transport.try_receive().ok());
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
    wait(|| transport.try_receive().ok());
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
    thread::sleep(Duration::from_millis(20));
    transport.close_stdin();
    assert!(begin.elapsed() < Duration::from_secs(1));
    let result = wait(|| match transport.try_completion() {
        Ok(Completion::Writer(result)) => Some(result),
        _ => None,
    });
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
    wait(|| {
        (root
            .path()
            .join("sessions/fixture-agent/fixture-session.jsonl")
            .exists()
            && records(&root).len() == 5)
            .then_some(())
    });
    thread::sleep(Duration::from_millis(20));
    assert_eq!(
        records(&root).len(),
        5,
        "four notifications plus one persisted pending record"
    );
    transport.cancel();
    assert_eq!(
        reader_done(&transport)
            .expect_err("explicit cancellation")
            .kind(),
        io::ErrorKind::Interrupted
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
        assert_eq!(wait(|| transport.try_receive().ok()).record.seq, seq);
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
        &cancel
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
    let result = wait(|| match transport.try_completion() {
        Ok(Completion::Writer(result)) => Some(result),
        _ => None,
    });
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
    for tail in [b"{\"seq\":3".as_slice(), b"{not-json}\n".as_slice()] {
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
        assert_eq!(wait(|| transport.try_receive().ok()).record.seq, 3);
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
    let spoof = wait(|| transport.try_receive().ok());
    let genuine = wait(|| transport.try_receive().ok());
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
