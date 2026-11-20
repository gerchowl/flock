use super::*;
use std::cell::Cell;
use std::io::Cursor;

fn welcome_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    protocol::write_message(
        &mut bytes,
        &ServerMessage::Welcome {
            version: protocol::PROTOCOL_VERSION,
            encoding: protocol::RenderEncoding::SemanticFrame,
            error: None,
        },
    )
    .unwrap();
    bytes
}

#[test]
fn trickled_prefix_and_payload_each_consume_the_same_five_seconds() {
    for prefix in [true, false] {
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut bytes = Cursor::new(welcome_bytes());
        let mut calls = 0;
        let error = read_with(
            start + TIMEOUT,
            || clock.get(),
            |buffer, remaining| {
                calls += 1;
                assert!(calls <= 8, "partial progress reset the deadline");
                let in_prefix = bytes.position() < 4;
                if prefix || !in_prefix {
                    clock.set(clock.get() + Duration::from_secs(2).min(remaining));
                }
                let count = if !prefix && in_prefix { 4 } else { 1 };
                bytes.read(&mut buffer[..count])
            },
        )
        .unwrap_err();
        assert!(matches!(error, ClientError::AttachTimeout));
        assert_eq!(clock.get() - start, TIMEOUT);
    }
}

#[test]
fn early_timeout_would_block_and_interruption_repoll_the_remaining_budget() {
    let start = Instant::now();
    let clock = Cell::new(start);
    let mut bytes = Cursor::new(welcome_bytes());
    let mut errors = [
        io::ErrorKind::TimedOut,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::Interrupted,
    ]
    .into_iter();
    let mut budgets = Vec::new();
    let result = read_with(
        start + TIMEOUT,
        || clock.get(),
        |buffer, remaining| {
            budgets.push(remaining);
            if let Some(error) = errors.next() {
                clock.set(clock.get() + Duration::from_secs(1));
                Err(error.into())
            } else {
                bytes.read(buffer)
            }
        },
    );
    assert!(matches!(result, Ok(ServerMessage::Welcome { .. })));
    assert_eq!(
        &budgets[..4],
        &[
            TIMEOUT,
            Duration::from_secs(4),
            Duration::from_secs(3),
            Duration::from_secs(2),
        ]
    );
}

#[test]
fn expiry_beats_readable_bytes_and_interruptions_at_the_deadline() {
    let start = Instant::now();
    for before_read in [true, false] {
        let clock = Cell::new(if before_read { start + TIMEOUT } else { start });
        let error = read_with(
            start + TIMEOUT,
            || clock.get(),
            |_, _| {
                assert!(!before_read, "expired reader must not touch the socket");
                clock.set(start + TIMEOUT);
                Err(io::ErrorKind::Interrupted.into())
            },
        )
        .unwrap_err();
        assert!(matches!(error, ClientError::AttachTimeout));
    }
}

#[test]
fn successful_bytes_arriving_at_the_deadline_still_expire() {
    let start = Instant::now();
    let clock = Cell::new(start);
    let mut bytes = Cursor::new(welcome_bytes());
    let error = read_with(
        start + TIMEOUT,
        || clock.get(),
        |buffer, _| {
            clock.set(start + TIMEOUT);
            bytes.read(buffer)
        },
    )
    .unwrap_err();
    assert!(matches!(error, ClientError::AttachTimeout));
}

#[test]
fn eof_bad_encoding_oversize_and_other_io_errors_remain_protocol_failures() {
    let start = Instant::now();
    for (case, bytes) in [
        vec![],
        vec![1, 0, 0, 0, 255],
        ((MAX_FRAME_SIZE + 1) as u32).to_le_bytes().to_vec(),
    ]
    .into_iter()
    .enumerate()
    {
        let mut bytes = Cursor::new(bytes);
        let error =
            read_with(start + TIMEOUT, || start, |buffer, _| bytes.read(buffer)).unwrap_err();
        match (case, error) {
            (0, ClientError::Protocol(protocol::FramingError::UnexpectedEof))
            | (1, ClientError::Protocol(protocol::FramingError::Bincode(_)))
            | (2, ClientError::Protocol(protocol::FramingError::Oversized { .. })) => {}
            other => panic!("distinct framing error lost: {other:?}"),
        }
    }
    let error = read_with(
        start + TIMEOUT,
        || start,
        |_, _| Err(io::ErrorKind::ConnectionReset.into()),
    )
    .unwrap_err();
    assert!(
        matches!(error, ClientError::Protocol(protocol::FramingError::Io(ref io))
        if io.kind() == io::ErrorKind::ConnectionReset)
    );
}

#[test]
fn timeout_is_registered_and_cannot_trigger_a_handoff_or_redial() {
    let code = crate::api::error_code::ErrorCode::AttachTimeout;
    assert_eq!(code.as_str(), "attach_timeout");
    assert_eq!(code.exit(), crate::cli::exit::TIMEOUT);
    assert_eq!(code.retry(), crate::cli::exit::Retry::Yes);
    assert_eq!(
        ClientError::AttachTimeout.to_string(),
        "server did not answer attach within 5s (server busy?)"
    );
    let error = super::super::AttachAttemptError::Handshake(ClientError::AttachTimeout);
    assert!(!error.is_live_handoff_refusal());
    assert!(!error.is_redial_failure());
    assert!(!super::super::HandoffRetry::default().should_retry(&error, Duration::ZERO));
}
