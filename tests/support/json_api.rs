//! Bounded, one-line JSON API exchanges for isolated test servers.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// A connected API socket. Callers choose the existing per-helper deadline.
/// Connect and I/O errors propagate. An orderly EOF yields an empty response.
/// The pathname connect itself is synchronous, as with the original helpers.
pub(crate) struct JsonApi {
    /// Configured transport, exposed for the regression tests to inspect bounds.
    pub(crate) stream: UnixStream,
}

impl JsonApi {
    /// Configure both I/O directions immediately after connecting, before send.
    pub(crate) fn connect(path: &Path, timeout: Duration) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        // macOS rejects socket options once the peer has closed. Configure
        // both directions before sending anything that can trigger a reply.
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self { stream })
    }

    /// Send exactly one newline-terminated request.
    pub(crate) fn send(&mut self, request: &str) -> io::Result<()> {
        self.stream.write_all(request.as_bytes())?;
        if !request.ends_with('\n') {
            self.stream.write_all(b"\n")?;
        }
        self.stream.flush()
    }

    /// Read a response line, including any buffered response from a closed peer.
    pub(crate) fn read(self) -> io::Result<String> {
        let mut line = String::new();
        BufReader::new(self.stream).read_line(&mut line)?;
        Ok(line)
    }
}

/// Send a request and read its response using the caller's unchanged timeout.
pub(crate) fn request(path: &Path, request: &str, timeout: Duration) -> io::Result<String> {
    let mut api = JsonApi::connect(path, timeout)?;
    api.send(request)?;
    api.read()
}
