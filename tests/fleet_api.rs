//! Regression coverage for the socket path used by the fleet API helpers.
#[path = "support/json_api.rs"]
mod json_api;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn response_survives_peer_close_before_read() {
    let socket = Socket::new();
    let path = &socket.0;
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut api = json_api::JsonApi::connect(path, TIMEOUT).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server.set_nonblocking(false).unwrap();
    server.set_read_timeout(Some(TIMEOUT)).unwrap();
    server.set_write_timeout(Some(TIMEOUT)).unwrap();
    api.send(r#"{"id":"test","method":"ping"}"#).unwrap();
    let mut request = String::new();
    BufReader::new(&server).read_line(&mut request).unwrap();
    assert_eq!(request, "{\"id\":\"test\",\"method\":\"ping\"}\n");
    server.write_all(b"{\"result\":\"pong\"}\n").unwrap();
    // Closing synchronously puts the peer shutdown before the client's read.
    // macOS rejects SO_RCVTIMEO here if setup was deferred until after send.
    drop(server);
    assert_eq!(api.read().unwrap(), "{\"result\":\"pong\"}\n");
}

#[test]
fn both_timeouts_are_configured_before_any_request() {
    let socket = Socket::new();
    let listener = UnixListener::bind(&socket.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    let api = json_api::JsonApi::connect(&socket.0, TIMEOUT).unwrap();
    let (_server, _) = listener.accept().unwrap();
    assert_eq!(api.stream.read_timeout().unwrap(), Some(TIMEOUT));
    assert_eq!(api.stream.write_timeout().unwrap(), Some(TIMEOUT));
}

#[test]
fn silent_live_peer_fails_read_and_write_within_configured_timeout() {
    let socket = Socket::new();
    let listener = UnixListener::bind(&socket.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    let timeout = Duration::from_millis(50);
    let api = json_api::JsonApi::connect(&socket.0, timeout).unwrap();
    let (_silent_server, _) = listener.accept().unwrap();
    // These assertions fail before blocking I/O if timeout setup regresses.
    assert_eq!(api.stream.read_timeout().unwrap(), Some(timeout));
    assert_eq!(api.stream.write_timeout().unwrap(), Some(timeout));
    let mut writer = api.stream.try_clone().unwrap();
    // Fix the fixture capacity rather than depending on host socket defaults.
    use std::os::fd::AsRawFd;
    let capacity: libc::c_int = 1024;
    let result = unsafe {
        libc::setsockopt(
            writer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&capacity as *const libc::c_int).cast(),
            std::mem::size_of_val(&capacity) as libc::socklen_t,
        )
    };
    assert_eq!(
        result,
        0,
        "set fixture send capacity: {}",
        std::io::Error::last_os_error()
    );
    let error = writer.write_all(&vec![b'x'; 8 * 1024 * 1024]).unwrap_err();
    assert_timeout(error);
    assert_timeout(api.read().unwrap_err());
}

fn assert_timeout(error: std::io::Error) {
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "expected timeout, got {error}"
    );
}

#[test]
fn closed_peer_without_response_is_eof() {
    let socket = Socket::new();
    let listener = UnixListener::bind(&socket.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut api = json_api::JsonApi::connect(&socket.0, TIMEOUT).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nonblocking(false).unwrap();
    server.set_read_timeout(Some(TIMEOUT)).unwrap();
    api.send("ping\n").unwrap();
    let mut request = String::new();
    BufReader::new(&server).read_line(&mut request).unwrap();
    assert_eq!(request, "ping\n");
    drop(server);
    assert_eq!(api.read().unwrap(), "");
}

#[test]
fn complete_exchange_preserves_request_and_response_lines() {
    let socket = Socket::new();
    let listener = UnixListener::bind(&socket.0).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + TIMEOUT;
        let mut peer = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "client never connected"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        peer.set_write_timeout(Some(TIMEOUT)).unwrap();
        let mut line = String::new();
        BufReader::new(&peer).read_line(&mut line).unwrap();
        assert_eq!(line, "ping\n");
        peer.write_all(b"pong\n").unwrap();
    });
    assert_eq!(
        json_api::request(&socket.0, "ping\n", TIMEOUT).unwrap(),
        "pong\n"
    );
    server.join().unwrap();
}

struct Socket(std::path::PathBuf);

impl Socket {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("api-{}-{unique}", std::process::id())))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
