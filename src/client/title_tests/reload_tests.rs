//! Reload messages must update the snapshot used by the real slot-switch path.
use super::{config_text, output_for};
use crate::client::{run_client_loop, AttachEscapeState, ClientError};
use crate::config::Config;
use crate::protocol::{self, ClientKeybindings, ClientMessage, RenderEncoding, ServerMessage};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const FIXTURE_WAIT: Duration = Duration::from_secs(5);
const ACTIVE_PEER: &str = "peer.example";
const RELOAD_BARRIER: &str = "00000000-0000-4000-8000-000000000001";

pub(super) fn config(prefix: &str) -> String {
    config_text(false)
        // The active connection consumes the warm budget, keeping home cold
        // until the explicit switch, independently of timer scheduling.
        .replace(
            "[slots]\nenabled = false",
            "[slots]\nenabled = true\nmax = 1",
        )
        .replace("ctrl+a", prefix)
}

#[test]
fn successful_reload_refreshes_the_real_switch_hello_snapshot() {
    output_for("slot_reload_valid");
}

#[test]
fn failed_reload_preserves_the_last_successful_snapshot_in_the_switch_hello() {
    output_for("slot_reload_invalid");
}

pub(super) fn run(mode: &str, mut snapshot: Config) {
    let invalid = mode == "slot_reload_invalid";
    let prefix = if invalid { "ctrl+a" } else { "ctrl+b" };
    let mut expected = snapshot.clone();
    expected.keys.prefix = prefix.into();
    let expected = expected.local_keybindings_profile_toml().unwrap();
    let config_path =
        std::path::PathBuf::from(std::env::var(crate::config::CONFIG_PATH_ENV_VAR).unwrap());
    let socket = config_path.with_file_name("s");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    // The worker is exec-isolated and all inherited FLOCK_* values were removed.
    std::env::set_var(
        crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        &socket,
    );
    std::env::set_var(crate::remote::ACTIVE_SSH_TARGET_ENV_VAR, ACTIVE_PEER);
    std::env::set_var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR, "local");
    let (client, mut active) = UnixStream::pair().unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let serving = std::thread::spawn(move || {
        active.set_read_timeout(Some(FIXTURE_WAIT)).unwrap();
        assert!(matches!(read(&mut active), ClientMessage::Input {data} if data == b"attached"));
        // A successful reload establishes B, or establishes A from bootstrap C
        // before the invalid-file check. Both cases fail with a frozen snapshot.
        std::fs::write(&config_path, config(prefix)).unwrap();
        send(&mut active, &ServerMessage::ReloadSoundConfig);
        // Switching to the already-active peer returns FocusWorkspace only
        // after the preceding reload message has been handled by the loop.
        send(
            &mut active,
            &switch(ACTIVE_PEER, Some(RELOAD_BARRIER.into())),
        );
        assert!(
            matches!(read(&mut active), ClientMessage::FocusWorkspace {workspace_id} if workspace_id == RELOAD_BARRIER)
        );
        if invalid {
            std::fs::write(&config_path, "[").unwrap();
            send(&mut active, &ServerMessage::ReloadSoundConfig);
        } else {
            // No reload: a later dial must use B, never reread this C file.
            std::fs::write(&config_path, config("ctrl+c")).unwrap();
        }
        send(&mut active, &switch(protocol::HOME_SWITCH_TARGET, None));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut home = rt
            .block_on(async {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                tokio::time::timeout(FIXTURE_WAIT, listener.accept())
                    .await
                    .expect("switch must dial the private home listener")
                    .unwrap()
                    .0
            })
            .into_std()
            .unwrap();
        home.set_nonblocking(false).unwrap();
        home.set_read_timeout(Some(FIXTURE_WAIT)).unwrap();
        let hello = read(&mut home);
        send(
            &mut home,
            &ServerMessage::Welcome {
                version: protocol::PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            },
        );
        send(
            &mut home,
            &ServerMessage::ServerShutdown {
                reason: Some("detached".into()),
            },
        );
        // Keep both peers alive until the real client consumes home shutdown.
        released
            .recv_timeout(FIXTURE_WAIT)
            .expect("client loop must exit after switching");
        hello
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
        snapshot.ui.sound.clone(),
        3,
        false,
        false,
        false,
        RenderEncoding::TerminalAnsi,
        Some(AttachEscapeState::default()),
        Some(b"attached".to_vec()),
        &mut stdin_rx,
        Instant::now(),
        &mut snapshot,
    ));
    quit.store(true, Ordering::Release);
    let _ = release.send(());
    let hello = serving.join().unwrap();
    assert!(
        matches!(result, Err(ClientError::ServerShutdown {reason: Some(reason)}) if reason == "detached")
    );
    match hello {
        ClientMessage::Hello {
            keybindings: ClientKeybindings::Local { keys_toml },
            ..
        } => {
            assert_eq!(
                keys_toml, expected,
                "switch must use the last successful reload snapshot"
            );
        }
        _ => panic!("switch must advertise local keybindings"),
    }
    assert_eq!(
        snapshot.local_keybindings_profile_toml().unwrap(),
        expected,
        "the authoritative snapshot must survive beyond this loop"
    );
}

fn read(peer: &mut UnixStream) -> ClientMessage {
    protocol::read_message(peer, protocol::MAX_FRAME_SIZE).expect("expected bounded client message")
}

fn send(peer: &mut UnixStream, message: &ServerMessage) {
    protocol::write_message(peer, message).unwrap();
}

fn switch(target: &str, focus_workspace: Option<String>) -> ServerMessage {
    ServerMessage::SwitchServer {
        ssh_target: target.into(),
        fleet: None,
        focus_workspace,
        proxy_jump: None,
    }
}
