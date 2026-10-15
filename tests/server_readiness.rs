mod support;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;

const BANNER: &str = "flk server running; you can use any flk CLI command in another terminal.\r\n";

#[test]
fn readiness_rejects_mismatched_lines_without_another_read() {
    struct NoFurtherRead;
    impl Read for NoFurtherRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("mismatched announcement must fail before another read")
        }
    }
    for output in [
        format!("{BANNER}api socket: wrong.sock\n"),
        format!("{BANNER}unexpected line\n"),
        format!("{BANNER}api socket: fixture.sock\nlogs: wrong-order\n"),
        format!("{BANNER}{BANNER}"),
    ] {
        let err = support::readiness::read_announcement(
            io::Cursor::new(output.as_bytes()).chain(NoFurtherRead),
            Path::new("fixture.sock"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(&output));
    }
}

#[test]
fn readiness_accepts_canonical_and_symlink_socket_paths() {
    let base = std::env::temp_dir().join(format!("ready-alias-{}", std::process::id()));
    std::fs::create_dir_all(base.join("real")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("alias")).unwrap();
    std::fs::write(base.join("real/socket"), "").unwrap();
    let expected = base.join("real/socket").canonicalize().unwrap();
    let output = format!(
        "{BANNER}api socket: {}\nclient socket: client.sock\n",
        base.join("alias/socket").display()
    );
    let result = support::readiness::read_announcement(output.as_bytes(), &expected);
    std::fs::remove_dir_all(base).unwrap();
    result.unwrap();
}

#[test]
fn readiness_wording_and_order_match_print_ready_message() {
    let source = include_str!("../src/server/headless.rs");
    let body = source.split("fn print_ready_message(").nth(1).unwrap();
    let formats: Vec<_> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("eprintln!(\""))
        .filter_map(|line| line.split('"').next())
        .take(3)
        .collect();
    assert_eq!(
        formats,
        [
            BANNER.trim_end_matches(['\r', '\n']),
            "api socket: {}",
            "client socket: {}",
        ],
        "pin the first three emitted formats and their order"
    );
    let output = formats
        .iter()
        .map(|line| line.replace("{}", "fixture.sock"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    support::readiness::read_announcement(output.as_bytes(), Path::new("fixture.sock"))
        .expect("server wording/order must agree with the harness reader");
}

#[test]
fn every_binary_has_a_runner_hang_ceiling_in_both_profiles() {
    let config: toml::Value = toml::from_str(include_str!("../.config/nextest.toml")).unwrap();
    let default = &config["profile"]["default"];
    assert_eq!(default["slow-timeout"]["period"].as_str(), Some("60s"));
    assert_eq!(
        default["slow-timeout"]["terminate-after"].as_integer(),
        Some(7)
    );
    // CI inherits the default ceiling rather than replacing it with warnings only.
    assert!(config["profile"]["ci"].get("slow-timeout").is_none());
    for profile in ["default", "ci"] {
        if let Some(overrides) = config["profile"][profile].get("overrides") {
            for entry in overrides.as_array().unwrap() {
                if let Some(slow) = entry.get("slow-timeout") {
                    assert!(
                        slow["terminate-after"].as_integer().is_some_and(|n| n > 0),
                        "{profile} override can disable the hang ceiling: {entry}"
                    );
                }
            }
        }
    }
}

#[test]
fn measured_binary_exceptions_keep_their_hang_ceilings() {
    let config: toml::Value = toml::from_str(include_str!("../.config/nextest.toml")).unwrap();
    let overrides = config["profile"]["default"]["overrides"]
        .as_array()
        .unwrap();
    for (binary, test, periods) in [
        ("delegate", None, 3),
        ("agent_settled", None, 3),
        ("api_ping", None, 3),
        ("cli_wrapper", None, 3),
        ("server_readiness", None, 3),
        ("live_handoff", None, 3),
        ("mesh_delivery", None, 11),
        ("live_handoff", Some("rollback_store_open_failure_preserves_panes_and_original_error"), 3),
        ("flk", Some("app::agent_restart::tests::restart_api_resumes_same_session_flags_env_and_delivers_continuation"), 3),
    ] {
        let entry = overrides
            .iter()
            .find(|entry| {
                let filter = entry["filter"].as_str().unwrap();
                // A binary-wide ceiling also pins every named case in that binary.
                let matches_test = !filter.contains("test(")
                    || test.is_some_and(|test| filter.contains(&format!("test(={test})")));
                filter.contains(&format!("binary(={binary})"))
                    && matches_test
                    && entry.get("slow-timeout").is_some()
            })
            .expect("measured binary or test ceiling must remain explicit");
        assert_eq!(entry["slow-timeout"]["period"].as_str(), Some("60s"));
        assert_eq!(
            entry["slow-timeout"]["terminate-after"].as_integer(),
            Some(periods)
        );
    }
}

#[test]
fn readiness_waits_for_the_complete_announcement_without_a_clock() {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    // The reader reaches this gate only after consuming the banner and API
    // line. The writer controls readiness, regardless of scheduling speed.
    struct GatedReader {
        stream: UnixStream,
        remaining: usize,
        blocked: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Read for GatedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                self.blocked.send(()).unwrap();
                self.release.recv().unwrap();
                self.remaining = usize::MAX;
            }
            let len = buf.len().min(self.remaining);
            let n = self.stream.read(&mut buf[..len])?;
            self.remaining -= n;
            Ok(n)
        }
    }
    let prefix = format!("{BANNER}api socket: fixture.sock\r\n");
    writer.write_all(prefix.as_bytes()).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = support::readiness::read_announcement(
            GatedReader {
                stream: reader,
                remaining: prefix.len(),
                blocked: blocked_tx,
                release: release_rx,
            },
            Path::new("fixture.sock"),
        );
        done_tx.send(result).unwrap();
    });
    blocked_rx.recv().unwrap();
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    writer.write_all(b"client socket: client.sock\r\n").unwrap();
    release_tx.send(()).unwrap();
    done_rx.recv().unwrap().unwrap();
    worker.join().unwrap();
}

#[test]
fn readiness_rejects_error_paths_and_other_sandboxes() {
    for output in [
        "error: flk server is already running\napi socket: fixture.sock\n".to_string(),
        format!("{BANNER}api socket: fixture.sock\n"),
    ] {
        let err =
            support::readiness::read_announcement(output.as_bytes(), Path::new("fixture.sock"))
                .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains(&output));
    }
}

#[test]
fn readiness_reports_a_failed_child_without_hanging_on_the_parent_slave() {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let base = std::env::temp_dir().join(format!("flk-ready-{}", std::process::id()));
    let mut cmd = portable_pty::CommandBuilder::new("/bin/sh");
    for (key, value) in
        support::environment::isolated_env(&base.join("config"), &base.join("runtime"))
    {
        cmd.env(key, value);
    }
    cmd.env_remove("FLOCK_SOCKET_PATH");
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env_remove("FLOCK_PANE_ID");
    cmd.env_remove("FLOCK_AGENT_ID");
    cmd.args(["-c", "printf 'startup failed\\n' >&2; exit 7"]);
    let mut child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().unwrap();
    let err = support::readiness::read_announcement(reader, Path::new("fixture.sock")).unwrap_err();
    assert!(err.to_string().contains("startup failed"), "{err}");
    drop(pair.master);
    assert!(!child.wait().unwrap().success());
    std::fs::remove_dir_all(base).unwrap();
}

/// Every normal live-handoff launch must consume the shared announcement.
/// Importers use the handoff wire handshake, not this startup path.
#[test]
fn readiness_live_handoff_launches_use_the_shared_reader() {
    let source = include_str!("live_handoff.rs");
    assert!(
        !source.contains("wait_for_socket("),
        "socket existence is not startup readiness"
    );
    for name in [
        "spawn_server_exe",
        "spawn_named_session_server",
        "spawn_default_session_server",
    ] {
        let body = source
            .split(&format!("fn {name}("))
            .nth(1)
            .unwrap()
            .split("\nfn ")
            .next()
            .unwrap();
        assert!(
            body.contains("drop(pair.slave)"),
            "{name} must release the parent slave"
        );
        assert!(
            body.contains("support::readiness::wait_for_server("),
            "{name} must wait for readiness"
        );
        assert!(
            body.find("let spawned = SpawnedFlock").unwrap()
                < body.find("support::readiness::wait_for_server(").unwrap(),
            "{name} must own cleanup before a readiness panic"
        );
    }
}

/// Mesh startup and restart use their own initialized listeners and app.
#[test]
fn fleet_spawn_and_restart_observe_their_own_readiness() {
    let mut fleet = support::fleet::spawn(
        "startup-ready",
        &[support::fleet::NodeSpec::new("nodea", "alpha", &[])],
    );
    for restart in [false, true] {
        let node = fleet.node_mut("nodea");
        if restart {
            node.restart();
        }
        let reply: serde_json::Value =
            serde_json::from_str(&node.api(r#"{"id":"check-ready","method":"ping","params":{}}"#))
                .unwrap();
        assert!(
            reply.get("result").is_some(),
            "initialized API answers: {reply}"
        );
        drop(node.attach());
    }
}

struct StartupFixture {
    server: support::server_startup::ServerStartup,
    _directory: support::socket_dir::SocketDir,
}

impl StartupFixture {
    fn spawn(script: &str, ceiling: std::time::Duration, output: Option<UnixStream>) -> Self {
        let directory = support::socket_dir::SocketDir::reserve("flk-start");
        let api = directory.path().join("api.sock");
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut cmd = portable_pty::CommandBuilder::new("/bin/sh");
        for (key, value) in support::environment::isolated_env(
            &directory.path().join("config"),
            &directory.path().join("runtime"),
        ) {
            cmd.env(key, value);
        }
        cmd.args(["-c", script, "fixture"]);
        cmd.arg(&api);
        let child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
        drop(pair.slave);
        let server = match output {
            Some(output) => support::server_startup::ServerStartup::with_output(
                pair.master,
                child,
                &api,
                ceiling,
                output,
            ),
            None => support::server_startup::ServerStartup::new(pair.master, child, &api, ceiling),
        }
        .unwrap();
        Self {
            server,
            _directory: directory,
        }
    }
}

const STARTUP_CEILING: std::time::Duration = std::time::Duration::from_secs(30);
const READY_SCRIPT: &str = "printf '%s\\napi socket: %s\\nclient socket: %s\\n' \
    'flk server running; you can use any flk CLI command in another terminal.' \
    \"$1\" \"$1-client\"; read ignored";

#[test]
fn owned_startup_requires_one_complete_announcement() {
    let mut fixture = StartupFixture::spawn(READY_SCRIPT, STARTUP_CEILING, None);
    fixture.server.wait_ready().unwrap();
    assert!(!fixture.server.has_exited().unwrap());
    assert!(fixture
        .server
        .wait_ready()
        .unwrap_err()
        .to_string()
        .contains("already observed"));
    fixture.server.stop();
    fixture.server.stop();
}

#[test]
fn owned_startup_silent_and_partial_live_producers_fail_at_the_ceiling() {
    for script in [
        "read ignored",
        "printf '%s\\napi socket: %s\\n' \
         'flk server running; you can use any flk CLI command in another terminal.' \
         \"$1\"; read ignored",
    ] {
        let mut fixture =
            StartupFixture::spawn(script, std::time::Duration::from_millis(100), None);
        let error = fixture.server.wait_ready().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            !fixture.server.has_exited().unwrap(),
            "producer must remain alive"
        );
    }
}

#[test]
fn owned_startup_rejects_mismatched_and_excessive_output() {
    for script in [
        "printf '%s\\napi socket: wrong.sock\\n' \
         'flk server running; you can use any flk CLI command in another terminal.'; read ignored",
        "printf '%65537s' x; read ignored",
    ] {
        let mut fixture = StartupFixture::spawn(script, STARTUP_CEILING, None);
        let error = fixture.server.wait_ready().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        assert!(!fixture.server.has_exited().unwrap());
    }
}

#[test]
fn owned_startup_child_exit_retains_failure_output() {
    let mut fixture = StartupFixture::spawn(
        "printf 'startup failed\\n' >&2; exit 7",
        STARTUP_CEILING,
        None,
    );
    let error = fixture.server.wait_ready().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    assert!(error.to_string().contains("startup failed"), "{error}");
}

#[test]
fn owned_startup_child_exit_fails_with_a_live_output_writer() {
    let (output, mut retained_writer) = UnixStream::pair().unwrap();
    retained_writer
        .write_all(b"startup failed with writer held open\n")
        .unwrap();
    let mut fixture = StartupFixture::spawn("exit 7", STARTUP_CEILING, Some(output));
    let error = fixture.server.wait_ready().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    assert!(error.to_string().contains("writer held open"), "{error}");
    fixture.server.stop();
    // Retain the independent writer until both observers have joined.
    drop(retained_writer);
}

#[test]
fn owned_startup_drains_after_readiness_and_cancels_with_writer_alive() {
    let (output, mut writer) = UnixStream::pair().unwrap();
    let mut fixture = StartupFixture::spawn("read ignored", STARTUP_CEILING, Some(output));
    write!(
        writer,
        "{BANNER}api socket: {}\nclient socket: fixture-client.sock\n",
        fixture._directory.path().join("api.sock").display()
    )
    .unwrap();
    fixture.server.wait_ready().unwrap();
    // This exceeds the socket buffer and startup capture cap. The drain must
    // continue after readiness, with a failure-only bound on this fixture write.
    writer.set_write_timeout(Some(STARTUP_CEILING)).unwrap();
    writer.write_all(&vec![b'x'; 256 * 1024]).unwrap();
    assert!(!fixture.server.has_exited().unwrap());
    fixture.server.stop();
    drop(writer);
}

#[test]
fn owned_startup_shared_cleanup_does_not_reap_the_observed_child() {
    let (output, writer) = UnixStream::pair().unwrap();
    let mut fixture = StartupFixture::spawn("read ignored", STARTUP_CEILING, Some(output));
    support::cleanup_registered_flock_pids();
    let error = fixture.server.wait_ready().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    assert!(fixture.server.has_exited().unwrap());
    fixture.server.stop();
    drop(writer);
}

struct StartupPeer {
    cancel: UnixStream,
    worker: Option<std::thread::JoinHandle<io::Result<()>>>,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StartupPeer {
    fn spawn(api: &Path, response: Option<&'static str>, exit_child: Option<u32>) -> Self {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::Ordering;
        let listener = std::os::unix::net::UnixListener::bind(api).unwrap();
        let (cancel, mut stop) = UnixStream::pair().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = requests.clone();
        let worker = std::thread::spawn(move || {
            let stages = support::readiness::Stages::new(STARTUP_CEILING);
            if stages.readable(&[stop.as_raw_fd(), listener.as_raw_fd()])? == 0 {
                return Ok(());
            }
            let (mut stream, _) = listener.accept()?;
            stream.set_write_timeout(Some(STARTUP_CEILING))?;
            let request = support::readiness::read_socket_line(
                &mut stream,
                |fd| {
                    if stages.readable(&[stop.as_raw_fd(), fd])? == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "peer cancelled",
                        ));
                    }
                    Ok(())
                },
                "startup request EOF",
            )?;
            let request: serde_json::Value = serde_json::from_str(&request)?;
            assert_eq!(request["id"], "startup-ready");
            assert_eq!(request["method"], "ping");
            observed.fetch_add(1, Ordering::SeqCst);
            if let Some(pid) = exit_child {
                // The fixture retains this owned child's unreaped PID.
                assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
            }
            if let Some(response) = response {
                writeln!(stream, "{response}")?;
            } else {
                // Hold the response writer until its owner cancels and joins us.
                // The foreground startup wait supplies the failure ceiling.
                io::copy(&mut stop, &mut io::sink())?;
            }
            Ok(())
        });
        Self {
            cancel,
            worker: Some(worker),
            requests,
        }
    }

    fn finish(&mut self) {
        match self.cancel.shutdown(std::net::Shutdown::Both) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotConnected => {}
            Err(error) => panic!("cancel startup peer: {error}"),
        }
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

impl Drop for StartupPeer {
    fn drop(&mut self) {
        let _ = self.cancel.shutdown(std::net::Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn announce_fixture(fixture: &StartupFixture, writer: &mut UnixStream) {
    write!(
        writer,
        "{BANNER}api socket: {}\nclient socket: fixture-client.sock\n",
        fixture._directory.path().join("api.sock").display()
    )
    .unwrap();
}

#[test]
fn owned_startup_responsiveness_requires_one_matching_pong() {
    use std::sync::atomic::Ordering;
    for response in [
        r#"{"id":"startup-ready","result":{"type":"pong"}}"#,
        r#"{"id":"another-request","result":{"type":"pong"}}"#,
        r#"{"id":"startup-ready","error":{"message":"not ready"}}"#,
    ] {
        let (output, mut writer) = UnixStream::pair().unwrap();
        let mut fixture = StartupFixture::spawn("read ignored", STARTUP_CEILING, Some(output));
        let mut peer = StartupPeer::spawn(
            &fixture._directory.path().join("api.sock"),
            Some(response),
            None,
        );
        announce_fixture(&fixture, &mut writer);
        let result = fixture.server.wait_responsive();
        if response.contains("another-request") || response.contains("error") {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        } else {
            result.unwrap();
        }
        peer.finish();
        assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn owned_startup_silent_api_fails_at_the_shared_ceiling() {
    use std::sync::atomic::Ordering;
    let (output, mut writer) = UnixStream::pair().unwrap();
    let mut fixture = StartupFixture::spawn("read ignored", STARTUP_CEILING, Some(output));
    let mut peer = StartupPeer::spawn(&fixture._directory.path().join("api.sock"), None, None);
    announce_fixture(&fixture, &mut writer);
    let error = fixture.server.wait_responsive().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert!(!fixture.server.has_exited().unwrap());
    peer.finish();
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
}

#[test]
fn owned_startup_dead_child_cannot_hide_behind_a_live_api_writer() {
    let (output, mut writer) = UnixStream::pair().unwrap();
    let mut fixture = StartupFixture::spawn("read ignored", STARTUP_CEILING, Some(output));
    let mut peer = StartupPeer::spawn(
        &fixture._directory.path().join("api.sock"),
        None,
        Some(fixture.server.process_id()),
    );
    announce_fixture(&fixture, &mut writer);
    let error = fixture.server.wait_responsive().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    assert!(fixture.server.has_exited().unwrap());
    fixture.server.stop();
    peer.finish();
}
