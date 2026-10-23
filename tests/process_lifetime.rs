//! Integration tests for process lifetime and exit notification helpers.

mod support;

use std::io::{self, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Stdio};
use std::time::Duration;
use support::environment::Command;
use support::process_lifetime::{self, ExitWatch};

struct SignalChild(Child);
impl SignalChild {
    fn start(ignore: bool) -> Self {
        let code = format!(
            "import os,signal\nsignal.signal(signal.SIGTERM, lambda *_: (os.write(1,b'TERM\\n'), {}))\nos.write(1,b'READY\\n')\nwhile True: signal.pause()\n",
            if ignore { "None" } else { "os._exit(0)" }
        );
        let mut fixture = Self(
            Command::new("python3")
                .args(["-c", &code])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let death = ExitWatch::new(fixture.0.id()).unwrap();
        let mut buf = [0u8; 6];
        fixture
            .0
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut buf)
            .unwrap();
        assert_eq!(&buf, b"READY\n");
        let _ = death;
        fixture
    }
}
impl Drop for SignalChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn pid_liveness_rejects_special_pids_and_tracks_an_owned_child() {
    assert!(process_lifetime::pid_alive(std::process::id()));
    assert!(!process_lifetime::pid_alive(0));
    assert!(!process_lifetime::pid_alive(u32::MAX));
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read -r finish"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(process_lifetime::pid_alive(child.id()));
    let exit = ExitWatch::new(child.id()).unwrap();
    assert_eq!(
        exit.wait(Duration::ZERO).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "a silent producer must fail its wait ceiling"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    exit.wait(Duration::from_secs(5)).unwrap();
    assert!(!process_lifetime::pid_alive(child.id()));
}

#[test]
fn stop_sends_term_and_allows_graceful_cleanup() {
    let mut fixture = SignalChild::start(false);
    let pid = fixture.0.id();
    let birth = process_lifetime::birth(pid).unwrap();
    process_lifetime::stop(pid, || process_lifetime::birth(pid) == Some(birth)).unwrap();
    assert!(
        fixture.0.wait().unwrap().success(),
        "server never received graceful TERM"
    );
    let mut output = String::new();
    fixture
        .0
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "TERM\n");
}

#[test]
fn stop_escalates_after_term_and_rechecks_identity() {
    let mut fixture = SignalChild::start(true);
    let mut checks = 0;
    let pid = fixture.0.id();
    let birth = process_lifetime::birth(pid).unwrap();
    process_lifetime::stop(pid, || {
        checks += 1;
        process_lifetime::birth(pid) == Some(birth)
    })
    .unwrap();
    assert_eq!(checks, 2, "escalation must recheck identity");
    assert_eq!(fixture.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    let mut output = String::new();
    fixture
        .0
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "TERM\n");
}

#[test]
fn changed_identity_before_escalation_is_not_signalled() {
    let mut fixture = SignalChild::start(true);
    let mut checks = 0;
    let result = process_lifetime::stop(fixture.0.id(), || {
        checks += 1;
        checks == 1
    });
    assert!(result.is_err(), "identity change did not refuse escalation");
    assert!(fixture.0.try_wait().unwrap().is_none());
    assert_eq!(checks, 2);
}
