//! Exercise scripted child collection with real, owned test processes.
mod support;

use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;
use support::environment::Command;

fn fixture(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "scripted_child_fixture", "--nocapture"])
        .env("SCRIPTED_CHILD_MODE", mode);
    command
}

#[test]
fn scripted_child_fixture() {
    let Ok(mode) = std::env::var("SCRIPTED_CHILD_MODE") else {
        return;
    };
    match mode.as_str() {
        "pipes" => {
            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input, "fixture input\n");
            io::stdout().write_all(&vec![b'o'; 256 * 1024]).unwrap();
            io::stderr().write_all(&vec![b'e'; 256 * 1024]).unwrap();
            std::process::exit(7);
        }
        "silent" => {
            let mut stream = std::os::unix::net::UnixStream::connect(
                std::env::var_os("SCRIPTED_CHILD_SOCKET").unwrap(),
            )
            .unwrap();
            writeln!(stream, "{}", std::process::id()).unwrap();
            // Intentionally alive and silent after connecting. The owning
            // command runner must kill/reap this fixture on deadline expiry.
            loop {
                std::thread::park();
            }
        }
        other => panic!("unexpected fixture mode {other}"),
    }
}

#[test]
fn child_exit_and_full_output_pipes_are_collected_without_deadlock() {
    let output = support::command::output(
        fixture("pipes"),
        Some(b"fixture input\n".to_vec()),
        support::command::CLI_LIMIT,
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.ends_with(&vec![b'o'; 256 * 1024]));
    assert_eq!(output.stderr, vec![b'e'; 256 * 1024]);
}

#[test]
fn connected_silent_child_fails_and_is_reaped_within_its_command_bound() {
    use tokio::io::AsyncBufReadExt;
    let directory = support::socket_dir::SocketDir::reserve("scripted-bound");
    let socket = directory.path().join("child.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stage_tx, stage_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let stage = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(6), async {
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                tokio::io::BufReader::new(stream)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                line.trim()
                    .parse::<u32>()
                    .expect("fixture PID is the connected stage")
            })
            .await
        });
        let _ = stage_tx.send(stage);
    });
    let mut command = fixture("silent");
    command.env("SCRIPTED_CHILD_SOCKET", socket);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = support::command::output(command, None, Duration::from_secs(2));
        let _ = done_tx.send(result);
    });
    let pid = stage_rx
        .recv_timeout(Duration::from_secs(7))
        .expect("stage producer must resolve")
        .expect("child must connect before becoming silent");
    let result = match done_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(error) => {
            // Regression protection: this PID came from our own parked child.
            // If command collection loses its deadline, fail instead of hanging
            // and terminate that fixture before unwinding the test.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = done_rx.recv_timeout(Duration::from_secs(2));
            panic!("command deadline did not resolve a connected silent child: {error}");
        }
    };
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    // The runner owns reaping: a second wait must report no child, not pending.
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn registry_key_matches_fnv_vectors_without_path_canonicalization() {
    for (path, expected) in [
        ("", "cbf29ce484222325"),
        ("a", "af63dc4c8601ec8c"),
        ("foobar", "85944171f73967e8"),
    ] {
        assert_eq!(
            support::paths::server_key(std::path::Path::new(path)),
            expected
        );
    }
    assert_ne!(
        support::paths::server_key(std::path::Path::new("socket")),
        support::paths::server_key(std::path::Path::new("./socket"))
    );
}
