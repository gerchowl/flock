//! Exec-isolated probes of the real client driver's diagnostics and exit status.
use super::*;
use std::process::Stdio;

/// Scheduling and cleanup margin for exec-isolated probes on loaded runners.
const DRIVER_CLEANUP_MARGIN: Duration = Duration::from_secs(3);
/// Allow the real startup deadline to expire before the fixture cancels the child.
const DRIVER_FINISH_CAP: Duration = INITIAL_GEOMETRY_TIMEOUT.saturating_add(DRIVER_CLEANUP_MARGIN);
/// Allow one second for quit plus cleanup margin. A geometry timeout still fails
/// the clean-exit and diagnostic assertions, even when it precedes this ceiling.
const INTERRUPT_FINISH_CAP: Duration = Duration::from_secs(1).saturating_add(DRIVER_CLEANUP_MARGIN);

pub(super) fn expect_failure(mode: &str, expected: &str) {
    let (status, output) = probe(mode);
    assert_eq!(
        status.exit_code(),
        crate::cli::exit::FAILURE as u32,
        "{output}"
    );
    assert!(output.contains(expected), "{output}");
}

#[test]
fn stdout_file_with_stdin_tty_attaches_through_real_driver() {
    let (status, output) = probe("stdin_tty");
    assert!(status.success(), "{output}");
}

#[test]
fn failed_client_keeps_its_diagnostic_when_the_server_is_cancelled() {
    expect_failure(
        "stdin_tty_deadline",
        "timeout: terminal geometry did not become nonzero",
    );
}

#[test]
fn real_sigint_during_geometry_wait_exits_cleanly_before_the_deadline() {
    let (status, output) = probe("interrupt");
    assert!(
        status.success(),
        "SIGINT must exit cleanly and promptly: {output}"
    );
    assert!(!output.contains("geometry_deadline"), "{output}");
    assert!(!output.contains("timeout: terminal geometry"), "{output}");
}

/// Return errors instead of panicking on a server-side timeout. The caller must
/// be able to see the client's stderr when startup fails before connecting.
fn serve(
    listener: std::os::unix::net::UnixListener,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut peer = runtime
        .block_on(async {
            let listener = tokio::net::UnixListener::from_std(listener)?;
            let accepted = tokio::time::timeout(DRIVER_FINISH_CAP, listener.accept());
            let result = tokio::select! {
                biased;
                _ = &mut stop => return Err(io::Error::new(
                    io::ErrorKind::Interrupted, "client exited before connecting",
                )),
                result = accepted => result,
            };
            result
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "client never reached the private listener",
                    )
                })?
                .map(|(peer, _)| peer)
        })?
        .into_std()?;
    peer.set_nonblocking(false)?;
    peer.set_read_timeout(Some(DRIVER_FINISH_CAP))?;
    let hello: ClientMessage = protocol::read_message(&mut peer, MAX_FRAME_SIZE)
        .map_err(|error| io::Error::other(error.to_string()))?;
    if !matches!(
        hello,
        ClientMessage::Hello {
            cols: 132,
            rows: 40,
            ..
        }
    ) {
        return Err(io::Error::other(format!(
            "unexpected geometry hello: {hello:?}"
        )));
    }
    for message in [
        ServerMessage::Welcome {
            version: PROTOCOL_VERSION,
            encoding: RenderEncoding::TerminalAnsi,
            error: None,
        },
        ServerMessage::ServerShutdown {
            reason: Some("detached".into()),
        },
    ] {
        protocol::write_message(&mut peer, &message)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
    let mut buffer = [0; 4096];
    while io::Read::read(&mut peer, &mut buffer)? != 0 {}
    Ok(())
}

fn probe(mode: &str) -> (portable_pty::ExitStatus, String) {
    probe_with_interrupt(
        mode,
        crate::platform::terminal_geometry::tests::interrupt_child,
    )
}

fn probe_with_interrupt(
    mode: &str,
    interrupt: impl FnOnce(&mut (dyn portable_pty::Child + Send + Sync)) -> io::Result<()>,
) -> (portable_pty::ExitStatus, String) {
    let root = geometry_fixture_dir();
    let socket = root.path().join("s");
    assert!(
        socket.as_os_str().len() < 104,
        "fixture socket path exceeds sun_path"
    );
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let success = mode == "stdin_tty";
    let has_server = success || mode == "stdin_tty_deadline";
    let (stop_server, stopped) = tokio::sync::oneshot::channel();
    let (server, idle_listener) = if has_server {
        (
            Some(std::thread::spawn(move || serve(listener, stopped))),
            None,
        )
    } else {
        (None, Some(listener))
    };
    let pair = (mode != "no_tty").then(|| {
        portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: if success { 40 } else { 0 },
                cols: if success { 132 } else { 0 },
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap()
    });
    let arguments = [
        "--exact",
        "client::geometry_tests::driver::driver_worker",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ];
    let environment = [
        ("HOME", root.path().join("home")),
        ("XDG_CONFIG_HOME", root.path().join("config")),
        ("XDG_STATE_HOME", root.path().join("state")),
        ("XDG_DATA_HOME", root.path().join("data")),
        (
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            socket,
        ),
    ];
    for (_, directory) in environment.iter().take(4) {
        std::fs::create_dir_all(directory).unwrap();
    }
    let (mut child, mut reader): (
        Box<dyn portable_pty::Child + Send + Sync>,
        Box<dyn io::Read + Send>,
    ) = if matches!(mode, "deadline" | "interrupt") {
        let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
        command.args(arguments);
        command.cwd(root.path());
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| key.starts_with("FLOCK_")) {
                command.env_remove(key);
            }
        }
        for (key, value) in &environment {
            command.env(key, value);
        }
        command.env("FLOCK_TEST_DRIVER_MODE", mode);
        command.env("TERM", "xterm-256color");
        let pair = pair.as_ref().unwrap();
        (
            pair.slave.spawn_command(command).unwrap(),
            pair.master.try_clone_reader().unwrap(),
        )
    } else {
        let mut command = crate::platform::terminal_geometry::tests::detached_command();
        command
            .args(arguments)
            .current_dir(root.path())
            .stderr(Stdio::piped());
        if has_server {
            command.stdin(crate::platform::terminal_geometry::tests::pty_input(
                pair.as_ref().unwrap().master.as_ref(),
            ));
            command.stdout(std::fs::File::create(root.path().join("stdout")).unwrap());
        } else {
            command.stdin(Stdio::null()).stdout(Stdio::null());
        }
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| key.starts_with("FLOCK_")) {
                command.env_remove(key);
            }
        }
        for (key, value) in &environment {
            command.env(key, value);
        }
        command.env("FLOCK_TEST_DRIVER_MODE", mode);
        let mut child =
            crate::process::TracedCommand::from_command(command, "geometry-real-driver")
                .spawn_traced()
                .unwrap();
        let stderr = child.stderr.take().unwrap();
        (Box::new(child), Box::new(stderr))
    };
    let (sent, received) = std::sync::mpsc::channel();
    let reading = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut announced = false;
        while output.len() <= 64 * 1024 {
            let mut buffer = [0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
            }
            let text = String::from_utf8_lossy(&output);
            if !announced
                && text.contains("client.geometry_query_failed")
                && text.contains("waiting_geometry")
            {
                announced = true;
                let _ = sent.send(());
            }
        }
        output
    });
    let ready = if mode == "interrupt" {
        received.recv_timeout(DRIVER_FINISH_CAP).is_ok()
    } else {
        true
    };
    let interrupt_error = if mode == "interrupt" && ready {
        interrupt(child.as_mut()).err()
    } else {
        None
    };
    let status = finish_pty_child_with_timeout(
        child,
        !ready || interrupt_error.is_some(),
        if mode == "interrupt" {
            INTERRUPT_FINISH_CAP
        } else {
            DRIVER_FINISH_CAP
        },
    );
    // Cancel a pending accept as soon as the owned client has exited. An
    // accepted peer sees EOF when that client closes its socket.
    let _ = stop_server.send(());
    drop(pair);
    let mut output = String::from_utf8_lossy(&reading.join().unwrap()).into_owned();
    assert!(
        ready,
        "client never entered the real geometry wait: {output}"
    );
    if let Some(listener) = idle_listener {
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "geometry startup occupied a handshake slot: {output}"
        );
    }
    if let Some(server) = server {
        let result = server.join().expect("fixture server must return its error");
        if let Err(error) = result {
            output.push_str(&format!("\nfixture server: {error}\n"));
            assert!(
                !status.success(),
                "successful client left a server error: {output}"
            );
        }
    }
    if let Some(error) = interrupt_error {
        panic!("failed to interrupt geometry client: {error}\n{output}");
    }
    (status, output)
}

#[test]
fn interrupt_failure_includes_the_real_client_output() {
    let failure = std::panic::catch_unwind(|| {
        probe_with_interrupt("interrupt", |_| {
            Err(io::Error::other("geometry client exited before SIGINT"))
        });
    })
    .expect_err("an interrupt failure must fail the probe");
    let diagnostic = failure.downcast_ref::<String>().unwrap();
    assert!(
        diagnostic.contains("geometry client exited before SIGINT"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("client.geometry_query_failed"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("waiting_geometry"), "{diagnostic}");
}

#[test]
fn exited_client_cancels_a_pending_server_accept() {
    let root = geometry_fixture_dir();
    let listener = std::os::unix::net::UnixListener::bind(root.path().join("s")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || done.send(serve(listener, stopped)).unwrap());
    stop.send(()).unwrap();
    let error = finished
        .recv_timeout(DRIVER_FINISH_CAP.saturating_add(DRIVER_CLEANUP_MARGIN))
        .expect("cancelled fixture server must return within its fail ceiling")
        .expect_err("client exit must cancel accept");
    server.join().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
}

#[test]
#[ignore = "exec-only fixture calls the real driver with isolated stdio/config/socket"]
fn driver_worker() {
    let mode = std::env::var("FLOCK_TEST_DRIVER_MODE").unwrap();
    if mode == "interrupt" {
        // Observe the real geometry event, after the production handler has been
        // installed. This subscriber changes only where diagnostics are written.
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(io::stderr)
            .try_init()
            .unwrap();
    }
    // Error exits belong to run_client_with_mode. No fixture-owned error
    // formatting or process exit may stand in for that production behavior.
    run_terminal_attach("fixture-terminal".into(), false).unwrap();
}
