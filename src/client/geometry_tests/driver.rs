//! Exec-isolated probes of the real client driver's diagnostics and exit status.
use super::*;
use std::process::Stdio;

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

fn probe(mode: &str) -> (portable_pty::ExitStatus, String) {
    let root = crate::structured::launch::SessionConfigDir::create(Path::new("/tmp")).unwrap();
    let socket = root.path().join("client.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let success = mode == "stdin_tty";
    let (server, idle_listener) = if success {
        (
            Some(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let mut peer = runtime
                    .block_on(async {
                        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                        tokio::time::timeout(Duration::from_secs(5), listener.accept())
                            .await
                            .expect("client never reached the private listener")
                            .unwrap()
                            .0
                    })
                    .into_std()
                    .unwrap();
                peer.set_nonblocking(false).unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let hello: ClientMessage =
                    protocol::read_message(&mut peer, MAX_FRAME_SIZE).unwrap();
                assert!(matches!(
                    hello,
                    ClientMessage::Hello {
                        cols: 132,
                        rows: 40,
                        ..
                    }
                ));
                protocol::write_message(
                    &mut peer,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::TerminalAnsi,
                        error: None,
                    },
                )
                .unwrap();
                protocol::write_message(
                    &mut peer,
                    &ServerMessage::ServerShutdown {
                        reason: Some("detached".into()),
                    },
                )
                .unwrap();
                let mut buffer = [0; 4096];
                while io::Read::read(&mut peer, &mut buffer).unwrap() != 0 {}
            })),
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
    let (child, mut reader): (
        Box<dyn portable_pty::Child + Send + Sync>,
        Box<dyn io::Read + Send>,
    ) = if mode == "deadline" {
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
        if std::env::var_os("FLOCK_TEST_DRIVER_SILENT").is_some() {
            command.env("FLOCK_TEST_DRIVER_SILENT", "1");
        }
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
        if success {
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
        if std::env::var_os("FLOCK_TEST_DRIVER_SILENT").is_some() {
            command.env("FLOCK_TEST_DRIVER_SILENT", "1");
        }
        let mut child =
            crate::process::TracedCommand::from_command(command, "geometry-real-driver")
                .spawn_traced()
                .unwrap();
        let stderr = child.stderr.take().unwrap();
        (Box::new(child), Box::new(stderr))
    };
    let reading = std::thread::spawn(move || {
        let mut output = Vec::new();
        while output.len() <= 64 * 1024 {
            let mut buffer = [0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
            }
        }
        output
    });
    let status = finish_pty_child(child, false);
    drop(pair);
    let output = String::from_utf8_lossy(&reading.join().unwrap()).into_owned();
    if let Some(listener) = idle_listener {
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "geometry startup occupied a handshake slot: {output}"
        );
    }
    if let Some(server) = server {
        server.join().unwrap();
    }
    (status, output)
}

#[test]
#[ignore = "exec-only fixture calls the real driver with isolated stdio/config/socket"]
fn driver_worker() {
    assert!(std::env::var_os("FLOCK_TEST_DRIVER_MODE").is_some());
    if std::env::var_os("FLOCK_TEST_DRIVER_SILENT").is_some() {
        loop {
            std::thread::park();
        }
    }
    // Error exits belong to run_client_with_mode. No fixture-owned error
    // formatting or process exit may stand in for that production behavior.
    run_terminal_attach("fixture-terminal".into(), false).unwrap();
}
