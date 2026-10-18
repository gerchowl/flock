//! Exec-isolated checks of the real server-message loop and terminal writer.
use super::{
    reload_local_client_config, restore_host_window_title, run_client_loop, AttachEscapeState,
    ClientError, HOST_WINDOW_TITLE_ALLOWED, HOST_WINDOW_TITLE_OWNED,
};
use crate::protocol::{self, RenderEncoding, ServerMessage};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// Only host-title operations, so unrelated OSC (links, clipboard) stays legal.
fn has_title_operation(output: &[u8]) -> bool {
    [
        b"\x1b]0;".as_slice(),
        b"\x1b]2;",
        b"\x1b[22;0t",
        b"\x1b[23;0t",
    ]
    .iter()
    .any(|pattern| output.windows(pattern.len()).any(|bytes| bytes == *pattern))
}

fn output_for(mode: &str) -> Vec<u8> {
    let root = crate::test_support::TempDir::socket();
    let config = root.path().join("config.toml");
    std::fs::write(&config, config_text(mode == "allowed")).unwrap();
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = portable_pty::CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "client::title_tests::title_output_worker",
        "--ignored",
        "--nocapture",
    ]);
    command.cwd(root.path());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("FLOCK_") {
            command.env_remove(key);
        }
    }
    command.env(crate::config::CONFIG_PATH_ENV_VAR, &config);
    command.env("FLOCK_TEST_TITLE_MODE", mode);
    command.env("HOME", root.path());
    command.env("XDG_CONFIG_HOME", root.path());
    command.env_remove("TMUX");
    command.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reenable = mode.starts_with("reenable");
    let reading = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut acknowledged = false;
        loop {
            let mut buffer = [0; 4096];
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => output.extend_from_slice(&buffer[..count]),
            }
            if reenable
                && !acknowledged
                && output
                    .windows(b"TITLE_CACHE_READY".len())
                    .any(|bytes| bytes == b"TITLE_CACHE_READY")
            {
                std::fs::write(&config, config_text(true)).unwrap();
                writer.write_all(b"continue\n").unwrap();
                acknowledged = true;
            }
            assert!(output.len() <= 64 * 1024, "unexpected fixture output flood");
        }
        tx.send(output).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(10));
    if result.is_err() {
        child.kill().unwrap();
    }
    let status = child.wait().unwrap();
    reading.join().unwrap();
    let output = result.expect("title worker must finish within its bounded ceiling");
    assert!(
        status.success(),
        "{status:?}: {}",
        String::from_utf8_lossy(&output)
    );
    output
}

fn config_text(enabled: bool) -> String {
    format!("[title]\nenabled = {enabled}\n[slots]\nenabled = false\n")
}

#[test]
fn title_detector_allows_unrelated_osc_and_recognizes_host_title_controls() {
    assert!(!has_title_operation(
        b"\x1b]8;;https://example.com\x1b\\link\x1b]52;c;YQ==\x07"
    ));
    for control in [
        b"\x1b]0;x\x07".as_slice(),
        b"\x1b]2;x\x1b\\",
        b"\x1b[22;0t",
        b"\x1b[23;0t",
    ] {
        assert!(has_title_operation(control));
    }
}

#[test]
fn local_veto_blocks_server_title_and_all_restore_bytes() {
    let output = output_for("veto");
    assert!(output
        .windows(b"render-still-arrives".len())
        .any(|bytes| bytes == b"render-still-arrives"));
    assert!(output
        .windows(b"\x1b]8;;https://example.com".len())
        .any(|bytes| bytes == b"\x1b]8;;https://example.com"));
    assert!(
        !has_title_operation(&output),
        "{:?}",
        String::from_utf8_lossy(&output)
    );
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
    assert!(!has_title_operation(&output_for("reload")));
}

#[test]
fn reenable_replays_cached_title_once_without_a_new_server_title() {
    let output = output_for("reenable");
    assert_eq!(
        output
            .windows(b"\x1b]2;server-title".len())
            .filter(|bytes| *bytes == b"\x1b]2;server-title")
            .count(),
        1,
        "{}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn reenable_without_a_received_title_emits_nothing() {
    assert!(!has_title_operation(&output_for("reenable_empty")));
}

#[test]
fn attach_uses_the_loaded_snapshot_even_if_the_file_changes_before_the_loop() {
    assert!(!has_title_operation(&output_for("snapshot")));
}

#[test]
#[ignore = "exec-isolated terminal writer fixture"]
fn title_output_worker() {
    let mode = std::env::var("FLOCK_TEST_TITLE_MODE").unwrap();
    let local_config = crate::config::Config::load().config;
    HOST_WINDOW_TITLE_ALLOWED.store(local_config.title.enabled, Ordering::Release);
    if mode == "snapshot" {
        std::fs::write(
            std::env::var(crate::config::CONFIG_PATH_ENV_VAR).unwrap(),
            config_text(true),
        )
        .unwrap();
    }
    if mode == "reload" {
        HOST_WINDOW_TITLE_ALLOWED.store(true, Ordering::Release);
        HOST_WINDOW_TITLE_OWNED.store(true, Ordering::Release);
        reload_local_client_config(&mut crate::config::SoundConfig::default(), &mut false, None);
        assert!(!HOST_WINDOW_TITLE_ALLOWED.load(Ordering::Acquire));
    } else {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_mode = mode.clone();
        let serving = std::thread::spawn(move || {
            if server_mode != "reenable_empty" {
                protocol::write_message(
                    &mut server,
                    &ServerMessage::SetWindowTitle {
                        title: "server-title".into(),
                    },
                )
                .unwrap();
            }
            protocol::write_message(
                &mut server,
                &ServerMessage::Terminal(protocol::TerminalFrame {
                    seq: 1,
                    width: 80,
                    height: 24,
                    full: true,
                    bytes: if server_mode.starts_with("reenable") {
                        b"TITLE_CACHE_READY".to_vec()
                    } else {
                        b"\x1b]8;;https://example.com\x1b\\render-still-arrives\x1b]8;;\x1b\\"
                            .to_vec()
                    },
                }),
            )
            .unwrap();
            if server_mode.starts_with("reenable") {
                let mut line = String::new();
                io::stdin().read_line(&mut line).unwrap();
                assert_eq!(line, "continue\n");
                for _ in 0..2 {
                    protocol::write_message(&mut server, &ServerMessage::ReloadSoundConfig)
                        .unwrap();
                }
            }
            protocol::write_message(
                &mut server,
                &ServerMessage::ServerShutdown {
                    reason: Some("detached".into()),
                },
            )
            .unwrap();
        });
        let quit = Arc::new(AtomicBool::new(false));
        let (_stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel(1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(run_client_loop(
            client,
            80,
            24,
            quit.clone(),
            local_config.ui.sound.clone(),
            3,
            false,
            false,
            false,
            RenderEncoding::TerminalAnsi,
            Some(AttachEscapeState::default()),
            None,
            &mut stdin_rx,
            Instant::now(),
            &local_config,
        ));
        quit.store(true, Ordering::Release);
        serving.join().unwrap();
        assert!(
            matches!(result, Err(ClientError::ServerShutdown { .. })),
            "{result:?}"
        );
        if matches!(mode.as_str(), "veto" | "snapshot") {
            // Even inherited ownership must never bypass the local veto.
            HOST_WINDOW_TITLE_OWNED.store(true, Ordering::Release);
        }
    }
    restore_host_window_title();
    restore_host_window_title();
}
