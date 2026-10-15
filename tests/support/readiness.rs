//! Startup synchronization for harness-owned headless servers and fixtures.
//!
//! `run_server` prints its ready message after both listeners and the app have
//! been initialized. Reading that message waits on the child, not host speed.
//! This is for normal starts, not handoffs or commands expected to fail.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

const READY: &str = "flk server running; you can use any flk CLI command in another terminal.";

/// Wait for the readiness announcement for this sandbox's API socket.
///
/// The caller must drop the parent's PTY slave first, so a failed child closes
/// the output stream. EOF/read errors include the startup output in the panic.
/// A child that remains alive but never announces readiness stays blocked here
/// until the nextest hang ceiling cancels it. There is no startup duration contract.
pub fn wait_for_server(master: &dyn portable_pty::MasterPty, api_socket: &Path) {
    let _timing = super::timing::wait("ready: server announcement");
    let reader = master
        .try_clone_reader()
        .expect("read server startup output");
    read_announcement(reader, api_socket)
        .unwrap_or_else(|err| panic!("server startup failed for {}: {err}", api_socket.display()));
}

/// Read the existing server banner and socket path in order, accepting PTY CRLF.
///
/// A socket path alone also appears in startup errors, so it is insufficient.
/// The client-socket line follows the API line in the successful announcement.
/// Every mismatch after the banner fails immediately. Existing socket endpoints
/// also compare by filesystem canonicalization: the server prints explicit
/// environment overrides verbatim, including paths through symlinks.
pub fn read_announcement(reader: impl Read, api_socket: &Path) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let expected_api = format!("api socket: {}", api_socket.display());
    let mut output = String::new();
    let mut stage = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("output closed before readiness\n{output}"),
                ));
            }
            Err(err) => {
                return Err(io::Error::new(err.kind(), format!("{err}\n{output}")));
            }
            Ok(_) => {}
        }
        output.push_str(&line);
        let line = line.trim_end_matches(['\r', '\n']);
        stage = match stage {
            0 => usize::from(line == READY),
            1 => {
                let matches = line.strip_prefix("api socket: ").is_some_and(|path| {
                    let path = Path::new(path);
                    path == api_socket
                        || matches!(
                            (path.canonicalize(), api_socket.canonicalize()),
                            (Ok(actual), Ok(expected)) if actual == expected
                        )
                });
                if !matches {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("expected {expected_api} after ready banner\n{output}"),
                    ));
                }
                2
            }
            _ => {
                if line
                    .strip_prefix("client socket: ")
                    .is_some_and(|path| !path.is_empty())
                {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected client socket after API socket\n{output}"),
                ));
            }
        };
    }
}

/// A shared failure-only ceiling for harness condition waits (ADR-0037 §1).
pub struct Stages {
    deadline: std::time::Instant,
}

impl Stages {
    /// Start one deadline shared by all protocol stages.
    pub fn new(ceiling: std::time::Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + ceiling,
        }
    }

    /// Remaining failure-only budget, shared by socket and channel waits.
    pub fn remaining(&self) -> std::time::Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    /// Wait for any descriptor, preferring earlier entries when several fire.
    /// EOF and descriptor errors wake poll too, so callers can report liveness.
    pub fn readable(&self, descriptors: &[std::os::fd::RawFd]) -> io::Result<usize> {
        loop {
            let remaining = self
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "composer stage stayed silent",
                ));
            }
            let mut polls: Vec<_> = descriptors
                .iter()
                .map(|&fd| libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            // The caller keeps all descriptors open through poll.
            let result =
                unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, millis) };
            if result > 0 {
                return Ok(polls
                    .iter()
                    .position(|p| p.revents != 0)
                    .expect("poll returned a ready descriptor"));
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    /// Read a line with this deadline, rejecting EOF and malformed output.
    pub fn line(&self, stream: &mut std::os::unix::net::UnixStream) -> io::Result<String> {
        read_socket_line(
            stream,
            |fd| self.readable(&[fd]).map(|_| ()),
            "stage producer closed",
        )
    }

    /// Require the next protocol stage exactly.
    pub fn expect(
        &self,
        stream: &mut std::os::unix::net::UnixStream,
        expected: &str,
    ) -> io::Result<()> {
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

/// Read a bounded UTF-8 line, checking readiness and liveness before each read.
/// The injected wait must bound silence, including a partial line.
/// Raw EOF uses the supplied reason; errors from the wait retain their source.
pub fn read_socket_line(
    stream: &mut std::os::unix::net::UnixStream,
    mut wait: impl FnMut(std::os::fd::RawFd) -> io::Result<()>,
    eof_reason: &str,
) -> io::Result<String> {
    use std::os::fd::AsRawFd;
    let mut bytes = Vec::new();
    loop {
        wait(stream.as_raw_fd())?;
        let mut byte = [0];
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, eof_reason));
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
