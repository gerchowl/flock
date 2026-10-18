use super::*;
use crate::server::client_transport::{
    client_read_loop, client_writer_loop, client_writer_loop_with, finish_write,
    write_with_progress, OBSERVER_WRITE_STALL_TIMEOUT,
};
use std::cell::Cell;
use std::io::Write;
use std::os::unix::net::UnixStream;

/// Wait on parser publication rather than a guessed PTY scheduling delay.
async fn pane_contains(
    server: &HeadlessServer,
    terminal: &crate::terminal::TerminalId,
    text: &str,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // The real event loop clears this coalescing flag when it renders.
            server.app.render_dirty.store(false, Ordering::Release);
            let notified = server.app.render_notify.notified();
            let runtime = server
                .app
                .terminal_runtimes
                .get(terminal)
                .expect("live pane");
            assert!(crate::platform::process_exists(
                runtime.child_pid().expect("child")
            ));
            if runtime.visible_text().contains(text) {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "pane did not publish {text:?}: {:?}",
            server
                .app
                .terminal_runtimes
                .get(terminal)
                .unwrap()
                .visible_text()
        )
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_observer_disconnect_preserves_live_pane_and_healthy_observer() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("observer-progress");
    let pane = workspace.focused_pane_id().unwrap();
    let terminal = workspace.terminal_id(pane).unwrap().clone();
    let cwd = server.client_socket_path.parent().unwrap().to_owned();
    let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
        pane,
        24,
        80,
        cwd,
        &[
            "/bin/sh".into(),
            "-c".into(),
            "printf 'PANE_READY\n'; while IFS= read -r line; do printf 'PANE:%s\n' \"$line\"; done"
                .into(),
        ],
        1024 * 1024,
        crate::terminal_theme::TerminalTheme::default(),
        server.app.event_tx.clone(),
        server.app.render_notify.clone(),
        server.app.render_dirty.clone(),
    )
    .unwrap();
    server
        .app
        .terminal_runtimes
        .insert(terminal.clone(), runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    // Render through the same terminal registry the server owns.
    server.render_and_stream();
    pane_contains(&server, &terminal, "PANE_READY").await;

    let (healthy_socket, mut healthy_peer) = UnixStream::pair().unwrap();
    healthy_peer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (healthy, control, render) = test_client_writer();
    let event_tx = server.server_event_tx.clone();
    let healthy_thread = std::thread::spawn(move || {
        client_writer_loop(healthy_socket, 2, control, render, event_tx)
    });
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::TerminalAnsi,
            Some(healthy),
        ),
    );
    server.foreground_client_id = Some(2);
    server.render_and_stream();
    let observer_screen = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    match protocol::read_message::<_, ServerMessage>(&mut healthy_peer, MAX_GRAPHICS_FRAME_SIZE)
        .unwrap()
    {
        ServerMessage::Terminal(frame) => observer_screen.test_process_pty_bytes(&frame.bytes),
        other => panic!("healthy observer lost initial output: {other:?}"),
    }

    let (stalled_socket, _nonreading_peer) = UnixStream::pair().unwrap();
    let stalled_reader = stalled_socket.try_clone().unwrap();
    let (stalled, control, render) = test_client_writer();
    let tx = server.server_event_tx.clone();
    let quit = server.should_quit.clone();
    let reader = std::thread::spawn(move || client_read_loop(stalled_reader, 1, &tx, &quit));
    let tx = server.server_event_tx.clone();
    let (expired_tx, expired_rx) = tokio::sync::oneshot::channel();
    let writer = std::thread::spawn(move || {
        // A short syscall timeout exposes actual backpressure, then the supplied
        // clock advances the policy deadline without waiting 30 real seconds.
        let now = Cell::new(Instant::now());
        let mut observed_backpressure = false;
        client_writer_loop_with(stalled_socket, 1, control, render, tx, |socket, data| {
            socket
                .set_write_timeout(Some(Duration::from_millis(1)))
                .unwrap();
            let result = write_with_progress(
                data,
                OBSERVER_WRITE_STALL_TIMEOUT,
                || now.get(),
                |bytes, _remaining| {
                    assert!(
                        !observed_backpressure,
                        "silent observer must reach the deadline, never spin"
                    );
                    match socket.write(bytes) {
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            observed_backpressure = true;
                            now.set(now.get() + OBSERVER_WRITE_STALL_TIMEOUT);
                            Err(err)
                        }
                        other => other,
                    }
                },
            );
            assert_eq!(result.as_ref().unwrap_err().kind(), io::ErrorKind::TimedOut);
            finish_write(socket, 1, result)
        });
        expired_tx.send(observed_backpressure).unwrap();
    });
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(stalled.clone()),
        ),
    );
    stalled.control.send(vec![42; 8 * 1024 * 1024]).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), expired_rx)
        .await
        .unwrap()
        .unwrap());
    // Consume the real reader's disconnect, not a constructed lifecycle event.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = server
                .server_event_rx
                .recv()
                .await
                .expect("transport event");
            let disconnected = matches!(event, ServerEvent::ClientDisconnected { client_id: 1 });
            server.handle_server_event(event);
            if disconnected {
                break;
            }
        }
    })
    .await
    .expect("shutdown must wake the observer reader");
    assert!(!server.clients.contains_key(&1));
    assert!(server.clients.contains_key(&2));
    assert!(!server.should_quit.load(Ordering::Acquire));
    server
        .app
        .terminal_runtimes
        .get(&terminal)
        .unwrap()
        .try_send_bytes(bytes::Bytes::from_static(b"still-running\n"))
        .unwrap();
    pane_contains(&server, &terminal, "PANE:still-running").await;
    server.render_and_stream();
    match protocol::read_message::<_, ServerMessage>(&mut healthy_peer, MAX_GRAPHICS_FRAME_SIZE)
        .unwrap()
    {
        ServerMessage::Terminal(frame) => {
            observer_screen.test_process_pty_bytes(&frame.bytes);
            assert!(
                observer_screen
                    .visible_text()
                    .contains("PANE:still-running"),
                "{}",
                observer_screen.visible_text()
            );
        }
        other => panic!("healthy observer lost terminal output: {other:?}"),
    }
    writer.join().unwrap();
    reader.join().unwrap().unwrap();
    server.clients.clear();
    drop(stalled);
    healthy_thread.join().unwrap();
    server
        .app
        .terminal_runtimes
        .remove(&terminal)
        .unwrap()
        .shutdown();
}
