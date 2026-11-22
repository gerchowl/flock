//! One monotonic budget for the complete Welcome frame, including partial reads.
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::{protocol, ClientError, ServerMessage, MAX_FRAME_SIZE};

pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn read(stream: &UnixStream, deadline: Instant) -> Result<ServerMessage, ClientError> {
    read_with(deadline, Instant::now, |buffer, remaining| {
        crate::platform::socket::read_ready(stream, buffer, remaining)
    })
}

fn read_with(
    deadline: Instant,
    now: impl FnMut() -> Instant,
    receive: impl FnMut(&mut [u8], Duration) -> io::Result<usize>,
) -> Result<ServerMessage, ClientError> {
    let mut reader = WelcomeReader {
        deadline,
        now,
        receive,
        expired: false,
    };
    let result = protocol::read_message(&mut reader, MAX_FRAME_SIZE);
    if reader.expired {
        Err(ClientError::AttachTimeout)
    } else {
        result.map_err(ClientError::Protocol)
    }
}

struct WelcomeReader<N, R> {
    deadline: Instant,
    now: N,
    receive: R,
    expired: bool,
}

impl<N, R> Read for WelcomeReader<N, R>
where
    N: FnMut() -> Instant,
    R: FnMut(&mut [u8], Duration) -> io::Result<usize>,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let remaining = self.deadline.saturating_duration_since((self.now)());
            if remaining.is_zero() {
                self.expired = true;
                return Err(io::ErrorKind::TimedOut.into());
            }
            let result = (self.receive)(buffer, remaining);
            if (self.now)() >= self.deadline {
                self.expired = true;
                return Err(io::ErrorKind::TimedOut.into());
            }
            match result {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) => {}
                other => return other,
            }
        }
    }
}

pub(super) fn report_timeout(error: &ClientError) {
    let code = crate::api::error_code::ErrorCode::AttachTimeout;
    let response = serde_json::json!({"id": null, "error": {
        "code": code.as_str(), "message": error.to_string(),
        "data": {"phase": "welcome", "timeout_ms": TIMEOUT.as_millis() as u64}
    }});
    crate::cli::output::error_as(&response, format!("flock: {error}"), code.exit());
    crate::cli::output::finish(code.exit());
}

#[cfg(test)]
mod tests;
