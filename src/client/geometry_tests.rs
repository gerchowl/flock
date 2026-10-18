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
    use portable_pty::{CommandBuilder, PtySize};
    const MARKER: &[u8] = b"GEOMETRY_GUARD_RESTORED";
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let before = pair.master.get_termios().expect("owned PTY modes");
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "client::geometry_tests::startup_guard_worker",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
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
    let mut child = pair.slave.spawn_command(command).unwrap();
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
            Ok(chunk) => output.extend(chunk),
            Err(_) => break false,
        }
    };
    if !restored {
        crate::platform::terminal_geometry::terminate_test_child(child.as_mut()).unwrap();
    }
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= exit_deadline {
            crate::platform::terminal_geometry::terminate_test_child(child.as_mut()).unwrap();
            break child.wait().unwrap();
        }
        std::thread::yield_now();
    };
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
    assert_eq!(before.local_flags, after.local_flags);
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
    let initial = current_terminal_geometry(false).unwrap();
    assert_eq!((initial.0, initial.1), (132, 40));
    assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    // Drive the actual pre-handshake return against an isolated socket.
    let directory = crate::structured::launch::SessionConfigDir::create(Path::new("/tmp")).unwrap();
    let socket_path = directory.path().join("client.sock");
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
        |_| Err(GeometryError::Gone(io::ErrorKind::NotConnected.into())),
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
    let (mut handshake_peer, _) = listener.accept().unwrap();
    handshake_peer.set_nonblocking(true).unwrap();
    assert_eq!(io::Read::read(&mut handshake_peer, &mut [0]).unwrap(), 0);
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
                Err(GeometryError::Gone(io::ErrorKind::NotConnected.into())),
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
