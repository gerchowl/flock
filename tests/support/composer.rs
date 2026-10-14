//! Failure-bounded condition waits for the guarded-submit fixture (ADR-0037 §1).
//! A single deadline covers startup and all stages. EOF, malformed output and
//! silence are errors, including when the producer remains alive.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

/// One shared failure ceiling for the fixture startup and readiness protocol.
pub struct Stages {
    deadline: Instant,
}

impl Stages {
    /// Start a failure-only deadline, independent of the paste timing assertion.
    pub fn new(ceiling: Duration) -> Self {
        Self {
            deadline: Instant::now() + ceiling,
        }
    }

    fn readable(&self, fd: RawFd) -> io::Result<()> {
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "composer stage stayed silent",
                ));
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            // The descriptor belongs to the caller and stays open through poll.
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    /// Accept a fixture connection or fail at the shared silence ceiling.
    pub fn accept(&self, listener: &UnixListener) -> io::Result<UnixStream> {
        self.readable(listener.as_raw_fd())?;
        listener.accept().map(|(stream, _)| stream)
    }

    /// Read a bounded UTF-8 stage line, rejecting EOF, oversized lines and silence.
    pub fn line(&self, stream: &mut UnixStream) -> io::Result<String> {
        let mut bytes = Vec::new();
        loop {
            self.readable(stream.as_raw_fd())?;
            let mut byte = [0];
            if stream.read(&mut byte)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "composer stage producer exited",
                ));
            }
            if byte[0] == b'\n' {
                return String::from_utf8(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
            }
            bytes.push(byte[0]);
            if bytes.len() > 65536 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "composer stage line too long",
                ));
            }
        }
    }

    /// Require the next stage exactly, rejecting out-of-order fixture output.
    pub fn expect(&self, stream: &mut UnixStream, expected: &str) -> io::Result<()> {
        let actual = self.line(stream)?;
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected {expected}, got {actual}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn producer_exit_fails() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        drop(writer);
        assert_eq!(
            Stages::new(Duration::from_secs(1))
                .expect(&mut reader, "boot")
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn unexpected_stage_fails() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"wrong\n").unwrap();
        assert_eq!(
            Stages::new(Duration::from_secs(1))
                .expect(&mut reader, "boot")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn live_silent_producer_fails() {
        let (mut reader, _live_writer) = UnixStream::pair().unwrap();
        assert_eq!(
            Stages::new(Duration::from_millis(10))
                .expect(&mut reader, "boot")
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn stage_arrival_succeeds() {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"boot\n").unwrap();
        Stages::new(Duration::from_secs(1))
            .expect(&mut reader, "boot")
            .unwrap();
    }
}
