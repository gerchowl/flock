//! Exec-isolated checks of the real server-message loop and terminal writer.
use super::*;
use std::process::{Command, Stdio};

#[allow(clippy::disallowed_methods)] // Exec fixture owns and bounds its child process.
fn output_for(mode: &str) -> Vec<u8> {
    let root = crate::test_support::TempDir::socket();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[title]\nenabled = {}\n[slots]\nenabled = false\n",
            mode == "allowed"
        ),
    )
    .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    command
        .args([
            "--exact",
            "client::title_tests::title_output_worker",
            "--ignored",
            "--nocapture",
        ])
        .env(crate::config::CONFIG_PATH_ENV_VAR, config)
        .env("FLOCK_TEST_TITLE_MODE", mode)
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env_remove("TMUX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut errors = Vec::new();
        std::io::Read::read_to_end(&mut { stdout }, &mut output).unwrap();
        std::io::Read::read_to_end(&mut { stderr }, &mut errors).unwrap();
        tx.send((output, errors)).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(10));
    if result.is_err() {
        child.kill().unwrap();
    }
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let (output, errors) = result.expect("title worker must finish within its bounded ceiling");
    assert!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&errors)
    );
    output
}

#[test]
fn local_veto_blocks_server_title_and_all_restore_bytes() {
    let output = output_for("veto");
    assert!(output
        .windows(b"render-still-arrives".len())
        .any(|bytes| bytes == b"render-still-arrives"));
    assert!(
        !output.windows(2).any(|bytes| bytes == b"\x1b]"),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(!output
        .windows(crate::terminal_notify::POP_WINDOW_TITLE.len())
        .any(|bytes| bytes == crate::terminal_notify::POP_WINDOW_TITLE));
}

#[test]
fn allowed_client_writes_server_title_through_real_loop() {
    let output = output_for("allowed");
    assert!(output
        .windows(b"\x1b]2;server-title".len())
        .any(|bytes| bytes == b"\x1b]2;server-title"));
}

#[test]
fn local_reload_veto_blocks_even_an_owned_title_restore() {
    let output = output_for("reload");
    assert!(!output.windows(2).any(|bytes| bytes == b"\x1b]"));
}

#[test]
#[ignore = "exec-isolated terminal writer fixture"]
fn title_output_worker() {
    let mode = std::env::var("FLOCK_TEST_TITLE_MODE").unwrap();
    if mode == "reload" {
        HOST_WINDOW_TITLE_ALLOWED.store(true, Ordering::Release);
        HOST_WINDOW_TITLE_OWNED.store(true, Ordering::Release);
        reload_local_client_config(&mut crate::config::SoundConfig::default(), &mut false);
        assert!(!HOST_WINDOW_TITLE_ALLOWED.load(Ordering::Acquire));
    } else {
        let (client, mut server) = UnixStream::pair().unwrap();
        protocol::write_message(
            &mut server,
            &ServerMessage::SetWindowTitle {
                title: "server-title".into(),
            },
        )
        .unwrap();
        protocol::write_message(
            &mut server,
            &ServerMessage::Terminal(protocol::TerminalFrame {
                seq: 1,
                width: 80,
                height: 24,
                full: true,
                bytes: b"render-still-arrives".to_vec(),
            }),
        )
        .unwrap();
        protocol::write_message(
            &mut server,
            &ServerMessage::ServerShutdown {
                reason: Some("detached".into()),
            },
        )
        .unwrap();
        let quit = Arc::new(AtomicBool::new(false));
        let (_stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel(1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(run_client_loop(
            client,
            (80, 24, 0, 0),
            Box::new(|_| Ok((80, 24, 0, 0))),
            quit.clone(),
            crate::config::SoundConfig::default(),
            3,
            false,
            false,
            false,
            RenderEncoding::TerminalAnsi,
            Some(AttachEscapeState::default()),
            None,
            &mut stdin_rx,
            Instant::now(),
        ));
        quit.store(true, Ordering::Release);
        assert!(
            matches!(result, Err(ClientError::ServerShutdown { .. })),
            "{result:?}"
        );
        // Even inherited ownership must never bypass a local veto on exit.
        HOST_WINDOW_TITLE_OWNED.store(true, Ordering::Release);
    }
    restore_host_window_title();
    restore_host_window_title();
}
