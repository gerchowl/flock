use super::super::*;
use std::cell::Cell;

#[test]
fn wedged_observer_expires_at_exact_no_progress_boundary() {
    let (mut socket, _observer) = UnixStream::pair().unwrap();
    socket.set_nonblocking(true).unwrap();
    let now = Cell::new(Instant::now());
    let mut blocked = 0;
    let result = write_with_progress(
        &vec![42; 8 * 1024 * 1024],
        OBSERVER_WRITE_STALL_TIMEOUT,
        || now.get(),
        |bytes, remaining| match socket.write(bytes) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                blocked += 1;
                assert!(
                    blocked <= 2,
                    "deadline failed to terminate a silent live observer"
                );
                if blocked == 1 {
                    assert_eq!(remaining, Duration::from_secs(30));
                    now.set(now.get() + Duration::from_secs(29));
                } else {
                    assert_eq!(remaining, Duration::from_secs(1));
                    now.set(now.get() + Duration::from_secs(1));
                }
                Err(err)
            }
            other => other,
        },
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert_eq!(blocked, 2);
}

#[test]
fn slow_socket_progress_restarts_deadline_within_one_frame() {
    use std::io::Read;
    let (mut socket, mut observer) = UnixStream::pair().unwrap();
    observer
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let started = Instant::now();
    let now = Cell::new(started);
    let mut received = Vec::new();
    write_with_progress(
        b"slow",
        OBSERVER_WRITE_STALL_TIMEOUT,
        || now.get(),
        |bytes, _remaining| {
            now.set(now.get() + Duration::from_secs(29));
            // A real accepted byte is progress, regardless of the frame's age.
            let written = socket.write(&bytes[..1])?;
            let mut byte = [0];
            observer.read_exact(&mut byte)?;
            received.extend_from_slice(&byte);
            Ok(written)
        },
    )
    .unwrap();
    assert_eq!(received, b"slow");
    assert_eq!(now.get().duration_since(started), Duration::from_secs(116));
}

#[test]
fn partial_writes_preserve_frame_and_zero_write_fails() {
    let mut output = Vec::new();
    write_with_progress(
        b"framed",
        OBSERVER_WRITE_STALL_TIMEOUT,
        Instant::now,
        |bytes, _| {
            output.push(bytes[0]);
            Ok(1)
        },
    )
    .unwrap();
    assert_eq!(output, b"framed");
    assert_eq!(
        write_with_progress(b"x", OBSERVER_WRITE_STALL_TIMEOUT, Instant::now, |_, _| Ok(
            0
        ))
        .unwrap_err()
        .kind(),
        io::ErrorKind::WriteZero
    );
}

/// Exercise SO_SNDTIMEO itself, including cleanup if the deadline regresses.
#[test]
fn real_writer_deadline_disconnects_nonreading_observer_and_reaps_threads() {
    use std::net::Shutdown;
    use std::sync::mpsc::{channel, RecvTimeoutError};
    const TEST_STALL_TIMEOUT: Duration = Duration::from_millis(20);
    const FAIL_CEILING: Duration = Duration::from_secs(5);
    let (socket, _observer) = UnixStream::pair().unwrap();
    crate::platform::socket_test_support::bound_send_buffer(&socket).unwrap();
    let cancel = socket.try_clone().unwrap();
    let reader_socket = socket.try_clone().unwrap();
    let (control, control_rx) = channel();
    let (_render, render_rx) = std::sync::mpsc::sync_channel(1);
    let (events, mut received_events) = mpsc::channel(64);
    let (exited, exits) = channel();
    let mut frame = Vec::new();
    protocol::write_message(
        &mut frame,
        &ServerMessage::Terminal(protocol::TerminalFrame {
            seq: 1,
            width: 80,
            height: 24,
            full: true,
            bytes: vec![b'x'; 512 * 1024],
        }),
    )
    .unwrap();
    // Each real protocol frame is larger than the bounded kernel send buffer.
    // Keep the channels and peer alive so only a transport failure can end it.
    for _ in 0..4 {
        control.send(frame.clone()).unwrap();
    }
    let reader_events = events.clone();
    let reader_exit = exited.clone();
    let reader = std::thread::spawn(move || {
        let result = client_read_loop(
            reader_socket,
            91,
            &reader_events,
            &Arc::new(AtomicBool::new(false)),
        );
        reader_exit.send(("reader", result.is_ok())).unwrap();
    });
    let writer = std::thread::spawn(move || {
        client_writer_loop_timed(
            socket,
            91,
            control_rx,
            render_rx,
            events,
            TEST_STALL_TIMEOUT,
        );
        exited.send(("writer", true)).unwrap();
    });
    let deadline = Instant::now() + FAIL_CEILING;
    let first = exits.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let second = if first.is_ok() {
        exits.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    } else {
        Err(RecvTimeoutError::Timeout)
    };
    let ended_without_cancellation = first.is_ok() && second.is_ok();
    if !ended_without_cancellation {
        // Own descriptor only. Wake a live, silent regressed writer before
        // failing, so removing SO_SNDTIMEO cannot hang the test or its threads.
        cancel.shutdown(Shutdown::Both).unwrap();
        for _ in usize::from(first.is_ok())..2 {
            exits
                .recv_timeout(FAIL_CEILING)
                .expect("cancelled transport thread must exit");
        }
    }
    reader.join().unwrap();
    writer.join().unwrap();
    assert!(
        ended_without_cancellation,
        "production socket deadline failed: writer/reader needed explicit cancellation"
    );
    let mut ended = [first.unwrap(), second.unwrap()];
    ended.sort();
    assert_eq!(ended, [("reader", true), ("writer", true)]);
    assert!(matches!(
        received_events.try_recv().unwrap(),
        ServerEvent::ClientDisconnected { client_id: 91 }
    ));
}

#[test]
fn overflowing_control_queue_disconnects_only_its_observer() {
    use std::io::Read;
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (control, queue) = ObserverControl::channel(Some(socket), 91);
    let (healthy_socket, mut healthy_peer) = UnixStream::pair().unwrap();
    healthy_peer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (healthy, healthy_queue) = ObserverControl::channel(Some(healthy_socket), 92);
    for _ in 0..OBSERVER_CONTROL_CAPACITY {
        control.send(vec![1]).unwrap();
    }
    assert!(
        control.send(vec![2]).is_err(),
        "overflow must disconnect without waiting for the writer"
    );
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert_eq!(queue.try_iter().count(), OBSERVER_CONTROL_CAPACITY);
    healthy.send(vec![3]).unwrap();
    assert_eq!(healthy_queue.try_recv().unwrap(), vec![3]);
    healthy
        .socket
        .as_ref()
        .unwrap()
        .as_ref()
        .write_all(b"ok")
        .unwrap();
    let mut received = [0; 2];
    healthy_peer.read_exact(&mut received).unwrap();
    assert_eq!(&received, b"ok");
}

#[test]
fn oversized_control_frame_disconnects_without_queueing() {
    use std::io::Read;
    let (socket, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let (control, queue) = ObserverControl::channel(Some(socket), 91);
    assert!(control
        .send(vec![0; OBSERVER_CONTROL_FRAME_BYTES + 1])
        .is_err());
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert!(queue.try_recv().is_err());
}
