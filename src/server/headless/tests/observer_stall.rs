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
    let control_frame = HeadlessServer::frame_server_message(&ServerMessage::Notify {
        kind: protocol::NotifyKind::Toast,
        message: "backpressure".into(),
        body: Some("x".repeat(MAX_FRAME_SIZE / 2)),
    })
    .unwrap();
    stalled.control.send(control_frame).unwrap();
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

fn add_control_observer(
    server: &mut HeadlessServer,
    client_id: u64,
    socket: &UnixStream,
    write: impl FnMut(&mut UnixStream, &[u8]) -> bool + Send + 'static,
) -> impl FnOnce() + Send {
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (control, control_rx) = crate::server::client_transport::ObserverControl::channel(
        Some(socket.try_clone().unwrap()),
        client_id,
    );
    let (render, render_rx) = std::sync::mpsc::sync_channel(1);
    server.clients.insert(
        client_id,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            client_id,
            RenderEncoding::TerminalAnsi,
            Some(crate::server::client_transport::ClientWriter { control, render }),
        ),
    );
    let socket = socket.try_clone().unwrap();
    let events = server.server_event_tx.clone();
    move || client_writer_loop_with(socket, client_id, control_rx, render_rx, events, write)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_byte_overflow_fanout_preserves_other_observers_and_live_pane() {
    use std::io::Read;
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("control-overflow");
    let pane = workspace.focused_pane_id().unwrap();
    let terminal = workspace.terminal_id(pane).unwrap().clone();
    let runtime = crate::terminal::TerminalRuntime::spawn_argv_command(
        pane,
        24,
        80,
        server.client_socket_path.parent().unwrap().to_owned(),
        &[
            "/bin/sh".into(),
            "-c".into(),
            "printf 'READY\\n'; while IFS= read -r line; do printf 'PANE:%s\\n' \"$line\"; done"
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
    server.app.state.mode = crate::app::Mode::Terminal;
    pane_contains(&server, &terminal, "READY").await;
    let (stalled, mut stalled_peer) = UnixStream::pair().unwrap();
    stalled_peer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _stalled_writer = add_control_observer(&mut server, 1, &stalled, |socket, data| {
        socket.write_all(data).is_ok()
    });
    let mut peers = Vec::new();
    let mut writers = Vec::new();
    for client_id in [2, 3] {
        let (socket, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let task = add_control_observer(&mut server, client_id, &socket, |socket, data| {
            socket.write_all(data).is_ok()
        });
        writers.push(std::thread::spawn(task));
        peers.push(peer);
    }
    server.foreground_client_id = Some(2);
    // Fewer than eight frames must overflow by bytes, while healthy queues drain.
    for index in 0..5 {
        server.send_to_all_clients(ServerMessage::Notify {
            kind: protocol::NotifyKind::Toast,
            message: format!("burst-{index}"),
            body: Some("x".repeat(1024 * 1024)),
        });
        for peer in &mut peers {
            assert!(matches!(
                protocol::read_message::<_, ServerMessage>(peer, MAX_GRAPHICS_FRAME_SIZE).unwrap(),
                ServerMessage::Notify { .. }
            ));
        }
    }
    let overflowed = !server.clients.contains_key(&1);
    if overflowed {
        assert_eq!(stalled_peer.read(&mut [0]).unwrap(), 0);
    }
    assert!(server.clients.contains_key(&2) && server.clients.contains_key(&3));
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
    for peer in &mut peers {
        let screen = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        match protocol::read_message::<_, ServerMessage>(peer, MAX_GRAPHICS_FRAME_SIZE).unwrap() {
            ServerMessage::Terminal(frame) => {
                screen.test_process_pty_bytes(&frame.bytes);
                assert!(screen.visible_text().contains("PANE:still-running"));
            }
            other => panic!("healthy observer lost pane output: {other:?}"),
        }
    }
    server.clients.clear();
    for writer in writers {
        writer.join().unwrap();
    }
    server
        .app
        .terminal_runtimes
        .remove(&terminal)
        .unwrap()
        .shutdown();
    assert!(
        overflowed,
        "the byte budget must evict the overflowing observer before eight frames"
    );
}

#[test]
fn progressing_observer_keeps_latest_title_during_large_render_fanout() {
    use std::io::Read;
    let mut server = test_headless_server();
    let (socket, mut peer) = UnixStream::pair().unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let cancel = socket.try_clone().unwrap();
    let mut frame = Vec::new();
    protocol::write_message(
        &mut frame,
        &ServerMessage::Terminal(protocol::TerminalFrame {
            seq: 1,
            width: 80,
            height: 24,
            full: true,
            bytes: vec![b'x'; 2 * 1024 * 1024],
        }),
    )
    .unwrap();
    let frame_len = frame.len();
    let (progress, progressed) = std::sync::mpsc::channel();
    let (advance, advances) = std::sync::mpsc::channel();
    let mut first = true;
    let task = add_control_observer(&mut server, 1, &socket, move |socket, data| {
        if !first {
            return socket.write_all(data).is_ok();
        }
        first = false;
        for chunk in data[..12 * 1024].chunks(1024) {
            if socket.write_all(chunk).is_err() {
                return false;
            }
            if progress.send(()).is_err() || advances.recv_timeout(Duration::from_secs(5)).is_err()
            {
                return false;
            }
        }
        socket.write_all(&data[12 * 1024..]).is_ok()
    });
    server.clients[&1]
        .writer
        .as_ref()
        .unwrap()
        .render
        .send(frame)
        .unwrap();
    let writer = std::thread::spawn(task);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for index in 0..12 {
            progressed
                .recv_timeout(Duration::from_secs(5))
                .expect("writer must make actual socket progress");
            peer.read_exact(&mut [0; 1024]).unwrap();
            server.send_to_all_clients(ServerMessage::SetWindowTitle {
                title: format!("title-{index}"),
            });
            assert!(
                server.clients.contains_key(&1),
                "title burst evicted an observer that is making socket progress"
            );
            advance.send(()).unwrap();
        }
        peer.read_exact(&mut vec![0; frame_len - 12 * 1024])
            .unwrap();
        assert!(
            server.clients.contains_key(&1),
            "small title updates must not evict a progressing observer"
        );
        match protocol::read_message::<_, ServerMessage>(&mut peer, MAX_GRAPHICS_FRAME_SIZE)
            .unwrap()
        {
            ServerMessage::SetWindowTitle { title } => assert_eq!(title, "title-11"),
            other => panic!("expected latest title, got {other:?}"),
        }
    }));
    server.clients.clear();
    drop(advance);
    let _ = cancel.shutdown(std::net::Shutdown::Both);
    writer.join().unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
