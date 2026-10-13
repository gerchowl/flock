//! Startup synchronization for harness-owned headless servers.
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
/// until the test runner cancels it. There is no startup duration contract.
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
        stage = match (stage, line) {
            (_, READY) => 1,
            (1, line) if line == expected_api => 2,
            (2, line) if line.starts_with("client socket: ") => return Ok(()),
            _ => 0,
        };
    }
}
