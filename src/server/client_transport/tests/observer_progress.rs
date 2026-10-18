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
    write_with_progress(b"framed", Instant::now, |bytes, _| {
        output.push(bytes[0]);
        Ok(1)
    })
    .unwrap();
    assert_eq!(output, b"framed");
    assert_eq!(
        write_with_progress(b"x", Instant::now, |_, _| Ok(0))
            .unwrap_err()
            .kind(),
        io::ErrorKind::WriteZero
    );
}
