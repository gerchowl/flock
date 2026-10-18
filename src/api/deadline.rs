//! One monotonic deadline for a CLI socket exchange (ADR-0029 §2, #921).
use super::client::RequestClass;
use super::schema::{Method, Request, MSG_WAIT_REPLY_MAX_MS};
use std::{
    fmt,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Ordinary request cap. Partial bytes never restart this deadline.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Transport/dispatch allowance beyond an operation's own timeout.
pub(crate) const OPERATION_MARGIN: Duration = Duration::from_secs(5);
/// Finite cap for a wait or event stream whose caller supplied no timeout.
pub(crate) const MAX_WAIT: Duration = Duration::from_millis(MSG_WAIT_REPLY_MAX_MS);

/// Reject durations that the monotonic clock cannot represent.
pub(crate) fn end_after(started: Instant, timeout: Duration) -> io::Result<Instant> {
    started
        .checked_add(timeout)
        .ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

/// Select the operation's finite wait, plus transport margin, without truncating
/// a requested long poll. Invalid parameters are left for server validation.
pub(crate) fn request_timeout(request: &Request, ordinary: Duration) -> Duration {
    let wait = match &request.method {
        Method::MsgWaitReply(p) => Some(Duration::from_millis(
            p.timeout_ms
                .unwrap_or(MSG_WAIT_REPLY_MAX_MS)
                .min(MSG_WAIT_REPLY_MAX_MS),
        )),
        Method::PaneWaitForOutput(p) => {
            Some(p.timeout_ms.map(Duration::from_millis).unwrap_or(MAX_WAIT))
        }
        Method::EventsWait(p) => Some(p.timeout_ms.map(Duration::from_millis).unwrap_or(MAX_WAIT)),
        Method::EventsRead(p) => Some(Duration::from_millis(p.wait_ms.unwrap_or(0))),
        _ => serde_json::to_value(request)
            .ok()
            .and_then(|v| v["params"]["write_timeout_ms"].as_u64())
            .map(Duration::from_millis),
    };
    wait.map_or(ordinary, |wait| {
        ordinary.max(wait.saturating_add(OPERATION_MARGIN))
    })
}

/// Safely retryable transport expiry: no dispatch, or a declared read/idempotent call.
#[derive(Debug)]
pub(crate) struct SocketTimeout {
    pub(crate) socket: PathBuf,
    pub(crate) elapsed: Duration,
}
impl fmt::Display for SocketTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "socket {} timed out after {:.3}s",
            self.socket.display(),
            self.elapsed.as_secs_f64()
        )
    }
}
impl std::error::Error for SocketTimeout {}

/// A mutation accepted bytes but supplied no reply before its deadline.
/// Never retry automatically: inspect server state first (ADR-0029 §2).
#[derive(Debug)]
pub(crate) struct RequestOutcomeUnknown(pub(crate) SocketTimeout);
impl fmt::Display for RequestOutcomeUnknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}; operation may have been applied; inspect before retrying",
            self.0
        )
    }
}
impl std::error::Error for RequestOutcomeUnknown {}

/// A socket whose connect, writes and reads consume one deadline. Retrying a
/// read preserves partial JSON bytes and never resends a mutation. EAGAIN,
/// EINTR and kernel timeout reports retry only while time remains.
pub(crate) struct DeadlineStream {
    stream: UnixStream,
    socket: PathBuf,
    started: Instant,
    deadline: Instant,
    class: RequestClass,
    dispatched: bool,
}
impl DeadlineStream {
    pub(crate) fn connect(
        socket: &Path,
        timeout: Duration,
        class: RequestClass,
    ) -> io::Result<Self> {
        let started = Instant::now();
        let deadline = end_after(started, timeout)?;
        let stream = crate::platform::socket_deadline::connect(socket, deadline).map_err(|e| {
            if e.kind() == io::ErrorKind::TimedOut {
                Self::timeout_at(socket, started)
            } else {
                e
            }
        })?;
        Ok(Self {
            stream,
            socket: socket.to_owned(),
            started,
            deadline,
            class,
            dispatched: false,
        })
    }
    pub(crate) fn timeout_at(socket: &Path, started: Instant) -> io::Error {
        Self::expiry(socket, started, false)
    }
    fn expiry(socket: &Path, started: Instant, unknown: bool) -> io::Error {
        let timeout = SocketTimeout {
            socket: socket.to_owned(),
            elapsed: started.elapsed(),
        };
        if unknown {
            io::Error::other(RequestOutcomeUnknown(timeout))
        } else {
            io::Error::new(io::ErrorKind::TimedOut, timeout)
        }
    }
    fn remaining(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(Self::expiry(
                &self.socket,
                self.started,
                self.dispatched && self.class == RequestClass::Mutating,
            ))
        } else {
            Ok(left)
        }
    }
    fn transfer(
        &mut self,
        mut operation: impl FnMut(&mut UnixStream) -> io::Result<usize>,
        read: bool,
    ) -> io::Result<usize> {
        loop {
            let left = self.remaining()?;
            if read {
                self.stream.set_read_timeout(Some(left))?;
            } else {
                self.stream.set_write_timeout(Some(left))?;
            }
            match operation(&mut self.stream) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                result => {
                    if !read && matches!(result, Ok(n) if n > 0) {
                        self.dispatched = true;
                    }
                    return result;
                }
            }
        }
    }
}
impl Read for DeadlineStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.transfer(|s| s.read(out), true)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.transfer(|s| s.write(bytes), false)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
