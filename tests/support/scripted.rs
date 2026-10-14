//! A scripted socket server for driving the real `flk` binary through a verb
//! without a flock server behind it (#1025, #1024).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Output;
use std::thread;
use std::time::{Duration, Instant};

use super::{environment::Command, socket_dir::SocketDir};

/// Owns the socket reservation for a scripted peer. Dropping this guard removes
/// its directory even if the test unwinds. The peer thread remains detached, as
/// before, with its existing ten-second accept window and request handling.
/// Drop does not wait for peer I/O or join the thread.
pub struct ScriptedServer {
    _directory: SocketDir,
    _thread: thread::JoinHandle<()>,
}

/// A socket that answers every request with `answer(request)`, and the protocol
/// check a state-changing verb sends first (#868) with this build's protocol
/// unless `answer` returns a pong of its own.
pub fn scripted_server(
    name: &str,
    answer: fn(&serde_json::Value) -> serde_json::Value,
) -> (PathBuf, ScriptedServer) {
    let directory = SocketDir::reserve("flk-scripted");
    let socket = directory.path().join("s.sock");
    let listener = UnixListener::bind(&socket)
        .unwrap_or_else(|error| panic!("bind scripted peer {name}: {error}"));
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let Ok((mut stream, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap_or_default();
            // `answer` may script the ping too (an older server's protocol);
            // anything but a pong there means this build's protocol.
            let answered = answer(&request);
            let mut reply = if method == "ping"
                && answered.pointer("/result/type") != Some(&serde_json::json!("pong"))
            {
                serde_json::json!({"result":{
                    "type":"pong","version":"fixture","protocol":super::PROTOCOL_VERSION
                }})
            } else {
                answered
            };
            reply["id"] = request["id"].clone();
            let _ = writeln!(stream, "{reply}");
        }
    });
    (
        socket,
        ScriptedServer {
            _directory: directory,
            _thread: handle,
        },
    )
}

pub fn flk(socket: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flk"))
        .args(args)
        .env("FLOCK_SOCKET_PATH", socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("flk should run")
}
