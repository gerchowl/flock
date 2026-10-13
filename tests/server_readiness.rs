mod support;

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;

const BANNER: &str = "flk server running; you can use any flk CLI command in another terminal.\r\n";

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
        format!("{BANNER}api socket: other.sock\nclient socket: client.sock\n"),
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
