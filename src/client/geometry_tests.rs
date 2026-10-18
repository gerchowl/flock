use super::*;

#[test]
fn lost_terminal_poll_detaches_without_sending_a_resize() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let mut samples = 0;
    resize_poll_loop_with(
        tx,
        (132, 40, 0, 0),
        &quit,
        || {
            samples += 1;
            assert!(samples <= 2, "geometry loss must terminate the poller");
            if samples == 1 {
                Ok((132, 40, 0, 0))
            } else {
                // Bound even the old fallback implementation: it must fail, not hang.
                quit.store(true, Ordering::Release);
                Err(GeometryError::Gone(io::ErrorKind::NotConnected.into()))
            }
        },
        || {},
    );
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn missing_terminal_at_poll_start_detaches_without_waiting() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    resize_poll_loop_with(
        tx,
        (132, 40, 0, 0),
        &quit,
        || {
            quit.store(true, Ordering::Release);
            Err(GeometryError::Gone(io::ErrorKind::NotConnected.into()))
        },
        || panic!("unavailable terminal must detach before polling"),
    );
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn real_geometry_change_is_forwarded_before_later_terminal_loss() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let mut samples = [
        Ok((132, 40, 8, 16)),
        Ok((160, 50, 9, 18)),
        Err(GeometryError::Gone(io::ErrorKind::NotConnected.into())),
    ]
    .into_iter();
    resize_poll_loop_with(
        tx,
        (132, 40, 8, 16),
        &quit,
        || samples.next().expect("poller must exit on loss"),
        || {},
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientLoopEvent::Resize(160, 50, 9, 18))
    ));
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn transient_poll_error_keeps_geometry_without_resize_or_detach() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let logs = crate::logging::capture_logs(|| {
        resize_poll_loop_with(
            tx,
            (132, 40, 8, 16),
            &quit,
            || {
                quit.store(true, Ordering::Release);
                Err(GeometryError::Transient(io::ErrorKind::Interrupted.into()))
            },
            || {},
        )
    });
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(logs.contains("client.geometry_query_failed"), "{logs}");
    assert!(
        logs.contains("transient_query_failure") && logs.contains("geometry_retained"),
        "{logs}"
    );
}

#[test]
fn startup_classifies_loss_and_preserves_known_geometry_on_transient_failure() {
    for (stage, previous) in [
        ("handshake", None),
        ("loop_startup", Some((132, 40, 8, 16))),
    ] {
        let logs = crate::logging::capture_logs(|| {
            let geometry = resolve_terminal_geometry(
                previous,
                Err(GeometryError::Gone(io::ErrorKind::NotConnected.into())),
                stage,
            )
            .unwrap();
            assert!(geometry.is_none());
        });
        assert!(
            logs.contains(stage) && logs.contains("tty_gone") && logs.contains("detached"),
            "{logs}"
        );
        let transient = resolve_terminal_geometry(
            previous,
            Err(GeometryError::Transient(io::ErrorKind::Interrupted.into())),
            stage,
        );
        match previous {
            Some(known) => assert_eq!(transient.unwrap(), Some(known)),
            None => assert_eq!(transient.unwrap_err().kind(), io::ErrorKind::Interrupted),
        }
    }
}

#[test]
fn clean_terminal_loss_exit_is_final_even_with_reconnect_enabled() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let signal = AttachSignal::default();
    let plan = (
        "fixture.example".to_string(),
        crate::config::ReconnectPolicy {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(80),
            deadline: Duration::from_secs(1),
        },
    );
    let (_tx, mut input) = tokio::sync::mpsc::channel(4);
    let quit = AtomicBool::new(false);
    let mut attempts = 0;
    let result = attach_until_done(
        |_| {
            attempts += 1;
            assert!(attempts <= 1, "terminal loss must not redial");
            Ok(())
        },
        &signal,
        Some(&plan),
        &mut HandoffRetry::default(),
        &runtime,
        &mut input,
        &quit,
    );
    assert!(result.is_ok());
    assert_eq!(attempts, 1);
}

/// Exercise the real guard only in an exec'd child with its own controlling PTY.
#[test]
fn startup_loss_paths_restore_an_isolated_terminal_guard() {
    use portable_pty::PtySize;
    run_pty_worker(
        "startup_guard_worker",
        PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        },
        true,
    );
}

fn run_pty_worker(worker: &str, size: portable_pty::PtySize, controlling: bool) {
    use portable_pty::CommandBuilder;
    const MARKER: &[u8] = b"GEOMETRY_GUARD_RESTORED";
    let pair = portable_pty::native_pty_system().openpty(size).unwrap();
    let before = pair.master.get_termios().expect("owned PTY modes");
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        &format!("client::geometry_tests::{worker}"),
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    command.set_controlling_tty(controlling);
    command.env("TERM", "xterm-256color");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("FLOCK_")) {
            command.env_remove(key);
        }
    }
    command.env("FLOCK_TEST_TTY_CHILD", "1");
    if std::env::var_os("FLOCK_TEST_TTY_SILENT").is_some() {
        command.env("FLOCK_TEST_TTY_SILENT", "1");
    }
    let child = pair.slave.spawn_command(command).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (sent, received) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        use std::io::Read;
        loop {
            let mut chunk = [0; 4096];
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    if sent.send(chunk[..count].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let restored = loop {
        if output.windows(MARKER.len()).any(|part| part == MARKER) {
            break true;
        }
        if output.len() > 64 * 1024 {
            break false;
        }
        match received.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                output.extend(chunk);
                if output
                    .windows(b"RESIZE_GEOMETRY_NOW".len())
                    .any(|part| part == b"RESIZE_GEOMETRY_NOW")
                {
                    pair.master
                        .resize(portable_pty::PtySize {
                            rows: 40,
                            cols: 132,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .unwrap();
                }
            }
            Err(_) => break false,
        }
    };
    let status = finish_pty_child(child, !restored);
    let after = pair.master.get_termios().expect("retained owned PTY modes");
    drop(pair.slave);
    drop(received);
    reading.join().unwrap();
    assert!(
        restored,
        "helper failed to restore modes: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(status.success());
    assert_eq!(before.input_flags, after.input_flags);
    assert_eq!(before.output_flags, after.output_flags);
    assert_eq!(before.control_flags, after.control_flags);
    assert_eq!(
        crate::platform::terminal_geometry::tests::configured_local_flags(
            before.local_flags.bits()
        ),
        crate::platform::terminal_geometry::tests::configured_local_flags(after.local_flags.bits()),
    );
    assert_eq!(before.control_chars, after.control_chars);
}

#[test]
#[ignore = "exec-only helper needs an isolated controlling PTY"]
fn startup_guard_worker() {
    assert!(
        std::env::var_os("FLOCK_TEST_TTY_CHILD").is_some(),
        "requires an owned test PTY"
    );
    // Regression control for a producer that remains alive without readiness.
    if std::env::var_os("FLOCK_TEST_TTY_SILENT").is_some() {
        loop {
            std::thread::park();
        }
    }

    io::stdout().write_all(b"GEOMETRY_STAGE_QUERY\n").unwrap();
    let initial = terminal_geometry_query()(false).unwrap();
    assert_eq!((initial.0, initial.1), (132, 40));
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    // Drive the actual pre-handshake return against an isolated socket.
    let directory = geometry_fixture_dir();
    let socket_path = directory.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut pending = None;
    let (_input, mut input) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let signal = AttachSignal::default();
    let sound = crate::config::SoundConfig::default();
    assert!(run_attach_attempt(
        &socket_path,
        RenderEncoding::TerminalAnsi,
        None,
        true,
        false,
        Box::new(|_| Err(GeometryError::Gone(io::ErrorKind::NotConnected.into()))),
        None,
        &mut pending,
        &mut input,
        &runtime,
        &quit,
        &sound,
        3,
        false,
        Instant::now(),
        true,
        &signal
    )
    .is_ok());
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    io::stdout().write_all(b"GEOMETRY_STAGE_SETUP\n").unwrap();
    let guard = setup_direct_attach_terminal().unwrap();
    assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
    io::stdout().write_all(b"GEOMETRY_STAGE_STARTUP\n").unwrap();
    let (socket, mut loop_peer) = UnixStream::pair().unwrap();
    loop_peer
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let result = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            run_client_loop(
                socket,
                initial,
                Box::new(|_| Err(GeometryError::Gone(io::ErrorKind::NotConnected.into()))),
                quit.clone(),
                sound,
                3,
                false,
                false,
                false,
                RenderEncoding::TerminalAnsi,
                None,
                None,
                &mut input,
                Instant::now(),
            ),
        )
        .await
    });
    quit.store(true, Ordering::Release);
    assert!(result.as_ref().is_ok_and(|result| result.is_ok()));
    assert_eq!(io::Read::read(&mut loop_peer, &mut [0]).unwrap(), 0);
    assert!(finish_attach_session(guard, Ok(()), true, &AtomicBool::new(false)).is_ok());
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    // A pre-handshake exit can inherit raw mode from a previous redial leg.
    // The outer guard, already used by the real attach driver, owns that restore.
    let held = HeldRestoreGuard::new();
    let guard = setup_direct_attach_terminal().unwrap();
    RECONNECT_HOLD.store(true, Ordering::Release);
    drop(guard);
    assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
    assert!(resolve_terminal_geometry(
        None,
        Err(GeometryError::Gone(io::ErrorKind::NotConnected.into())),
        "handshake"
    )
    .unwrap()
    .is_none());
    held.finish();
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    io::stdout()
        .write_all(b"GEOMETRY_GUARD_RESTORED\n")
        .unwrap();
    io::stdout().flush().unwrap();
}

/// The waiter owns the child throughout: no copied PID can outlive reaping.
/// A bounded channel wait replaces the former yield loop. Cancellation kills
/// only this owned child through its standard process handle.
fn finish_pty_child(
    child: Box<dyn portable_pty::Child + Send + Sync>,
    cancel: bool,
) -> portable_pty::ExitStatus {
    finish_pty_child_with_timeout(
        child,
        cancel,
        INITIAL_GEOMETRY_TIMEOUT + Duration::from_secs(3),
    )
}

/// Bound completion by the supplied cap, then kill/reap only the owned child.
fn finish_pty_child_with_timeout(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    cancel: bool,
    timeout: Duration,
) -> portable_pty::ExitStatus {
    let (stop, stopped) = std::sync::mpsc::channel();
    let (sent, received) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || {
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            match stopped.recv_timeout(Duration::from_millis(100)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    terminate_test_child(child.as_mut()).unwrap();
                    break child.wait().unwrap();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
            }
        };
        let _ = sent.send(status);
    });
    if cancel {
        // The child may already have exited before cancellation was requested.
        let _ = stop.send(());
    }
    let status = match received.recv_timeout(timeout) {
        Ok(status) => status,
        Err(_) => {
            let _ = stop.send(());
            received
                .recv_timeout(Duration::from_secs(5))
                .expect("owned helper must stop")
        }
    };
    waiting.join().unwrap();
    status
}
fn terminate_test_child(child: &mut (dyn portable_pty::Child + Send + Sync)) -> io::Result<()> {
    let child = child
        .as_any_mut()
        .downcast_mut::<std::process::Child>()
        .expect("the Unix PTY backend owns a standard child");
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    Ok(())
}

#[test]
fn zero_startup_geometry_waits_until_sized_and_transient_logs_only_transition() {
    let clock = std::cell::Cell::new(Instant::now());
    let samples = std::cell::Cell::new(0);
    let logs = crate::logging::capture_logs(|| {
        let result = wait_for_initial_geometry(
            &AtomicBool::new(false),
            || {
                let sample = samples.get();
                samples.set(sample + 1);
                if sample < 3 {
                    Err(GeometryError::Transient(io::ErrorKind::WouldBlock.into()))
                } else {
                    Ok((132, 40, 0, 0))
                }
            },
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
        )
        .unwrap();
        assert_eq!(result, Some((132, 40, 0, 0)));
    });
    assert_eq!(samples.get(), 4);
    assert_eq!(
        logs.matches("client.geometry_query_failed").count(),
        1,
        "{logs}"
    );
}

#[test]
fn zero_geometry_deadline_is_visible_typed_and_nonzero_without_invented_size() {
    let clock = std::cell::Cell::new(Instant::now());
    let started = clock.get();
    let result = wait_for_initial_geometry(
        &AtomicBool::new(false),
        || Err(GeometryError::Transient(io::ErrorKind::WouldBlock.into())),
        || clock.get(),
        |duration| clock.set(clock.get() + duration),
    );
    assert_eq!(clock.get() - started, INITIAL_GEOMETRY_TIMEOUT);
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let client_error = ClientError::TerminalGeometry(error);
    assert!(client_error
        .to_string()
        .contains("timeout: terminal geometry"));
    assert!(!client_error.to_string().contains("80x24"));
    assert!(!AttachAttemptError::Handshake(client_error).is_redial_failure());
}

#[test]
fn stdio_pty_without_controlling_tty_attaches() {
    run_pty_worker(
        "stdio_attach_worker",
        portable_pty::PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        },
        false,
    );
}

#[test]
#[ignore = "exec-only fixture requires stdio on an isolated PTY without a controlling tty"]
fn stdio_attach_worker() {
    assert!(std::env::var_os("FLOCK_TEST_TTY_CHILD").is_some());
    // guardrails-ok(hermetic): this exec fixture owns a session without a controlling tty.
    assert!(std::fs::File::open("/dev/tty").is_err());
    exercise_geometry_attach(terminal_geometry_query());
    io::stdout()
        .write_all(b"GEOMETRY_GUARD_RESTORED\n")
        .unwrap();
    io::stdout().flush().unwrap();
}

/// A successful real Hello/Welcome exchange proves startup reached attachment.
fn exercise_geometry_attach(mut probe: GeometryQuery) {
    let directory = geometry_fixture_dir();
    let path = directory.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // A finite nonblocking accept wait lets cleanup work even if startup fails.
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut socket = runtime
            .block_on(async {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .expect("startup never connected")
                    .unwrap()
                    .0
            })
            .into_std()
            .unwrap();
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let hello: ClientMessage = protocol::read_message(&mut socket, MAX_FRAME_SIZE).unwrap();
        assert!(matches!(
            hello,
            ClientMessage::Hello {
                cols: 132,
                rows: 40,
                ..
            }
        ));
        protocol::write_message(
            &mut socket,
            &ServerMessage::Welcome {
                version: PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            },
        )
        .unwrap();
        let mut end = [0];
        assert_eq!(io::Read::read(&mut socket, &mut end).unwrap(), 0);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut measured = false;
    let query: GeometryQuery = Box::new(move |kitty| {
        if measured {
            Err(GeometryError::Gone(io::Error::from_raw_os_error(libc::EIO)))
        } else {
            let result = probe(kitty);
            measured = result.is_ok();
            result
        }
    });
    let (_sent, mut input) = tokio::sync::mpsc::channel(4);
    let mut pending = None;
    let signal = AttachSignal::default();
    let result = run_attach_attempt(
        &path,
        RenderEncoding::TerminalAnsi,
        None,
        true,
        false,
        query,
        None,
        &mut pending,
        &mut input,
        &runtime,
        &Arc::new(AtomicBool::new(false)),
        &crate::config::SoundConfig::default(),
        3,
        false,
        Instant::now(),
        false,
        &signal,
    );
    server.join().unwrap();
    assert!(result.is_ok());
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
}

#[test]
fn transient_poll_logging_is_only_on_failure_state_transitions() {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let mut count = 0;
    let logs = crate::logging::capture_logs(|| {
        resize_poll_loop_with(
            tx,
            (132, 40, 0, 0),
            &quit,
            || {
                count += 1;
                if count == 5 {
                    quit.store(true, Ordering::Release);
                }
                if count == 3 {
                    Ok((132, 40, 0, 0))
                } else {
                    Err(GeometryError::Transient(io::ErrorKind::Interrupted.into()))
                }
            },
            || {},
        )
    });
    assert_eq!(
        logs.matches("client.geometry_query_failed").count(),
        2,
        "{logs}"
    );
}

#[test]
fn real_zero_pty_startup_attaches_when_sized_before_deadline() {
    run_pty_worker(
        "zero_attach_worker",
        portable_pty::PtySize {
            rows: 0,
            cols: 0,
            pixel_width: 0,
            pixel_height: 0,
        },
        true,
    );
}
#[test]
#[ignore = "exec-only fixture starts with an isolated zero-size PTY"]
fn zero_attach_worker() {
    assert!(std::env::var_os("FLOCK_TEST_TTY_CHILD").is_some());
    let mut probe = terminal_geometry_query();
    let mut signaled = false;
    exercise_geometry_attach(Box::new(move |kitty| {
        let result = probe(kitty);
        if !signaled
            && matches!(&result, Err(GeometryError::Transient(error)) if error.kind() == io::ErrorKind::WouldBlock)
        {
            signaled = true;
            io::stdout().write_all(b"RESIZE_GEOMETRY_NOW\n").unwrap();
            io::stdout().flush().unwrap();
        }
        result
    }));
    io::stdout()
        .write_all(b"GEOMETRY_GUARD_RESTORED\n")
        .unwrap();
    io::stdout().flush().unwrap();
}

#[path = "geometry_tests/driver.rs"]
mod driver;

#[test]
fn no_tty_startup_has_visible_error_and_nonzero_exit() {
    driver::expect_failure("no_tty", "terminal_unavailable: no terminal is available");
}
#[test]
fn unsized_startup_has_visible_deadline_error_and_nonzero_exit() {
    driver::expect_failure(
        "deadline",
        "timeout: terminal geometry did not become nonzero",
    );
}

#[test]
fn startup_geometry_quit_is_clean_before_and_during_the_wait() {
    let quit = AtomicBool::new(true);
    assert!(wait_for_initial_geometry(
        &quit,
        || panic!("quit must precede the probe"),
        Instant::now,
        |_| panic!("quit must not wait")
    )
    .unwrap()
    .is_none());
    quit.store(false, Ordering::Release);
    let clock = std::cell::Cell::new(Instant::now());
    let mut samples = 0;
    let result = wait_for_initial_geometry(
        &quit,
        || {
            samples += 1;
            Err(GeometryError::Transient(io::ErrorKind::WouldBlock.into()))
        },
        || clock.get(),
        |delay| {
            quit.store(true, Ordering::Release);
            clock.set(clock.get() + delay);
        },
    );
    assert!(result.unwrap().is_none());
    assert_eq!(samples, 1);
}

#[test]
fn geometry_wait_never_reserves_a_server_handshake_slot() {
    let root = geometry_fixture_dir();
    let socket = root.path().join("s");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let quit = Arc::new(AtomicBool::new(false));
    let probe_quit = quit.clone();
    let query: GeometryQuery = Box::new(move |_| {
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "startup probe must precede socket connect"
        );
        probe_quit.store(true, Ordering::Release);
        Err(GeometryError::Transient(io::ErrorKind::WouldBlock.into()))
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_tx, mut input) = tokio::sync::mpsc::channel(4);
    let result = run_attach_attempt(
        &socket,
        RenderEncoding::TerminalAnsi,
        None,
        true,
        false,
        query,
        None,
        &mut None,
        &mut input,
        &runtime,
        &quit,
        &crate::config::SoundConfig::default(),
        3,
        false,
        Instant::now(),
        false,
        &AttachSignal::default(),
    );
    assert!(result.is_ok());
}

#[test]
fn local_geometry_failure_does_not_retry_in_an_open_handoff_window() {
    let mut retry = HandoffRetry::default();
    let refusal = AttachAttemptError::Handshake(ClientError::HandshakeRejected {
        version: PROTOCOL_VERSION,
        error: protocol::LIVE_HANDOFF_ATTACH_NOTICE.into(),
    });
    assert!(retry.should_retry(&refusal, Duration::ZERO));
    for no_tty in [true, false] {
        let failure = AttachAttemptError::Handshake(ClientError::TerminalGeometry(
            geometry_startup_error(no_tty),
        ));
        assert!(!retry.should_retry(&failure, Duration::ZERO));
        assert!(!failure.is_redial_failure());
    }
}

/// A compact name leaves room for the socket even under macOS's long temp root.
struct GeometryFixture(std::path::PathBuf);
impl GeometryFixture {
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for GeometryFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn geometry_fixture_dir() -> GeometryFixture {
    let mut random = [0; 4];
    getrandom::fill(&mut random).unwrap();
    let path = std::env::temp_dir().join(format!("g{:08x}", u32::from_ne_bytes(random)));
    crate::platform::structured::create_private_dir(&path).unwrap();
    GeometryFixture(path)
}

#[test]
fn geometry_fixture_directory_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = geometry_fixture_dir();
    assert_eq!(
        std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
