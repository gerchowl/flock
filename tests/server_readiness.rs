mod support;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;

const BANNER: &str = "flk server running; you can use any flk CLI command in another terminal.\r\n";

#[test]
fn readiness_rejects_mismatched_lines_without_another_read() {
    struct NoFurtherRead;
    impl Read for NoFurtherRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("mismatched announcement must fail before another read")
        }
    }
    for output in [
        format!("{BANNER}api socket: wrong.sock\n"),
        format!("{BANNER}unexpected line\n"),
        format!("{BANNER}api socket: fixture.sock\nlogs: wrong-order\n"),
        format!("{BANNER}{BANNER}"),
    ] {
        let err = support::readiness::read_announcement(
            io::Cursor::new(output.as_bytes()).chain(NoFurtherRead),
            Path::new("fixture.sock"),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(&output));
    }
}

#[test]
fn readiness_accepts_canonical_and_symlink_socket_paths() {
    let base = std::env::temp_dir().join(format!("ready-alias-{}", std::process::id()));
    std::fs::create_dir_all(base.join("real")).unwrap();
    std::os::unix::fs::symlink(base.join("real"), base.join("alias")).unwrap();
    std::fs::write(base.join("real/socket"), "").unwrap();
    let expected = base.join("real/socket").canonicalize().unwrap();
    let output = format!(
        "{BANNER}api socket: {}\nclient socket: client.sock\n",
        base.join("alias/socket").display()
    );
    let result = support::readiness::read_announcement(output.as_bytes(), &expected);
    std::fs::remove_dir_all(base).unwrap();
    result.unwrap();
}

#[test]
fn readiness_wording_and_order_match_print_ready_message() {
    let source = include_str!("../src/server/headless.rs");
    let body = source.split("fn print_ready_message(").nth(1).unwrap();
    let formats: Vec<_> = body
        .lines()
        .filter_map(|line| line.trim().strip_prefix("eprintln!(\""))
        .filter_map(|line| line.split('"').next())
        .take(3)
        .collect();
    assert_eq!(
        formats,
        [
            BANNER.trim_end_matches(['\r', '\n']),
            "api socket: {}",
            "client socket: {}",
        ],
        "pin the first three emitted formats and their order"
    );
    let output = formats
        .iter()
        .map(|line| line.replace("{}", "fixture.sock"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    support::readiness::read_announcement(output.as_bytes(), Path::new("fixture.sock"))
        .expect("server wording/order must agree with the harness reader");
}

#[test]
fn readiness_binaries_have_a_runner_hang_ceiling() {
    let config: toml::Value = toml::from_str(include_str!("../.config/nextest.toml")).unwrap();
    let overrides = config["profile"]["default"]["overrides"]
        .as_array()
        .unwrap();
    for binary in [
        "delegate",
        "agent_settled",
        "api_ping",
        "cli_wrapper",
        "server_readiness",
    ] {
        let entry = overrides
            .iter()
            .find(|entry| {
                entry["filter"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("binary(={binary})"))
                    && entry
                        .get("slow-timeout")
                        .is_some_and(|slow| slow.get("terminate-after").is_some())
            })
            .expect("every readiness reader needs a runner hang ceiling");
        assert_eq!(entry["slow-timeout"]["period"].as_str(), Some("60s"));
        assert_eq!(
            entry["slow-timeout"]["terminate-after"].as_integer(),
            Some(3)
        );
    }
}

#[test]
fn readiness_waits_for_the_complete_announcement_without_a_clock() {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    // The reader reaches this gate only after consuming the banner and API
    // line. The writer controls readiness, regardless of scheduling speed.
    struct GatedReader {
        stream: UnixStream,
        remaining: usize,
        blocked: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Read for GatedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                self.blocked.send(()).unwrap();
                self.release.recv().unwrap();
                self.remaining = usize::MAX;
            }
            let len = buf.len().min(self.remaining);
            let n = self.stream.read(&mut buf[..len])?;
            self.remaining -= n;
            Ok(n)
        }
    }
    let prefix = format!("{BANNER}api socket: fixture.sock\r\n");
    writer.write_all(prefix.as_bytes()).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = support::readiness::read_announcement(
            GatedReader {
                stream: reader,
                remaining: prefix.len(),
                blocked: blocked_tx,
                release: release_rx,
            },
            Path::new("fixture.sock"),
        );
        done_tx.send(result).unwrap();
    });
    blocked_rx.recv().unwrap();
    assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    writer.write_all(b"client socket: client.sock\r\n").unwrap();
    release_tx.send(()).unwrap();
    done_rx.recv().unwrap().unwrap();
    worker.join().unwrap();
}

#[test]
fn readiness_rejects_error_paths_and_other_sandboxes() {
    for output in [
        "error: flk server is already running\napi socket: fixture.sock\n".to_string(),
        format!("{BANNER}api socket: fixture.sock\n"),
    ] {
        let err =
            support::readiness::read_announcement(output.as_bytes(), Path::new("fixture.sock"))
                .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains(&output));
    }
}

#[test]
fn readiness_reports_a_failed_child_without_hanging_on_the_parent_slave() {
    let pair = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .unwrap();
    let base = std::env::temp_dir().join(format!("flk-ready-{}", std::process::id()));
    let mut cmd = portable_pty::CommandBuilder::new("/bin/sh");
    for (key, value) in
        support::environment::isolated_env(&base.join("config"), &base.join("runtime"))
    {
        cmd.env(key, value);
    }
    cmd.env_remove("FLOCK_SOCKET_PATH");
    cmd.env_remove("FLOCK_CLIENT_SOCKET_PATH");
    cmd.env_remove("FLOCK_PANE_ID");
    cmd.env_remove("FLOCK_AGENT_ID");
    cmd.args(["-c", "printf 'startup failed\\n' >&2; exit 7"]);
    let mut child = support::environment::spawn_pty(pair.slave.as_ref(), cmd).unwrap();
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().unwrap();
    let err = support::readiness::read_announcement(reader, Path::new("fixture.sock")).unwrap_err();
    assert!(err.to_string().contains("startup failed"), "{err}");
    drop(pair.master);
    assert!(!child.wait().unwrap().success());
    std::fs::remove_dir_all(base).unwrap();
}
