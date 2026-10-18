//! Real crash recovery, including the copied probe binary, without live profiles.
//! Producer stages and failure ceilings follow ADR-0037 §1.
#![allow(clippy::disallowed_methods)]
mod support;

use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};
use support::process_lifetime::{self, ExitWatch};
use support::sandboxes::{self, ServerRecord};
use support::socket_dir::SocketDir;

struct Probe {
    root: Option<SocketDir>,
    owner: Child,
    stdout: ChildStdout,
    base: PathBuf,
    record: Option<ServerRecord>,
}

impl Probe {
    fn start() -> Self {
        let root = SocketDir::reserve("529");
        let driver = root.path().join("driver.sh");
        fs::write(&driver, "printf 'PROBE_READY\\n'\nread -r finish\n").unwrap();
        let mut command = Command::new("python3");
        command
            .args([
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/probes/run.py"),
                "--bin",
                env!("CARGO_BIN_EXE_flk"),
                "--root",
            ])
            .arg(root.path())
            .arg("--runbook")
            .arg(&driver)
            .arg("--driver")
            .arg("/bin/sh")
            .arg(&driver)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("FLOCK_"))
        {
            command.env_remove(key);
        }
        let mut owner = command.spawn().unwrap();
        let stdout = owner.stdout.take().unwrap();
        let mut probe = Self {
            root: Some(root),
            owner,
            stdout,
            base: PathBuf::new(),
            record: None,
        };
        let death = ExitWatch::new(probe.owner.id()).unwrap();
        let first = probe.line(&death);
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        probe.base = PathBuf::from(value["sandbox"].as_str().unwrap());
        assert_eq!(probe.line(&death), "PROBE_READY\n");
        let records: Vec<_> = fs::read_dir(&probe.base)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
            .collect();
        assert_eq!(records.len(), 1);
        probe.record = Some(serde_json::from_slice(&fs::read(records[0].path()).unwrap()).unwrap());
        probe.record.as_mut().unwrap().start_time =
            Some(process_lifetime::birth(probe.record().pid).unwrap());
        assert!(probe.record().matches(&probe.base));
        assert!(probe.record().binary.starts_with(&probe.base));
        probe
    }

    fn line(&mut self, death: &ExitWatch) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut line = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("probe stage ceiling");
            let mut events = [
                libc::pollfd {
                    fd: self.stdout.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: death.raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe {
                libc::poll(
                    events.as_mut_ptr(),
                    2,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            assert_ne!(
                result, 0,
                "probe stayed alive and silent until the stage ceiling"
            );
            assert!(
                result > 0,
                "probe poll failed: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                events[1].revents, 0,
                "probe owner died before its expected stage"
            );
            let mut byte = [0];
            assert_eq!(
                self.stdout.read(&mut byte).unwrap(),
                1,
                "probe output ended before its stage"
            );
            line.push(byte[0]);
            assert!(line.len() <= 65536, "unexpected probe output");
            if byte[0] == b'\n' {
                return String::from_utf8(line).unwrap();
            }
        }
    }

    fn record(&self) -> &ServerRecord {
        self.record.as_ref().unwrap()
    }

    fn orphan(&mut self) {
        self.owner.kill().unwrap();
        self.owner.wait().unwrap();
        assert!(!process_lifetime::pid_alive(self.owner.id()));
        assert!(
            self.record().matches(&self.base),
            "fixture server must outlive its fake owner"
        );
    }
}

impl Drop for Probe {
    #[expect(
        clippy::print_stderr,
        reason = "failed fixture cleanup must retain and report its private sandbox"
    )]
    fn drop(&mut self) {
        // Constructor failures also recover this fixture's durable record.
        if self.base.as_os_str().is_empty() {
            let prefix = format!("flock-autodetect-test-{}-", self.owner.id());
            if let Ok(entries) = fs::read_dir(self.root.as_ref().unwrap().path()) {
                if let Some(entry) = entries
                    .flatten()
                    .find(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                {
                    self.base = entry.path();
                }
            }
        }
        if self.record.is_none() {
            if let Ok(entries) = fs::read_dir(&self.base) {
                self.record = entries
                    .flatten()
                    .find_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok());
            }
        }
        if let Some(record) = &self.record {
            if process_lifetime::pid_alive(record.pid) {
                // This fixture owns the spawned server. Its captured birth also
                // permits cleanup when a negative mutation already removed files.
                if let Err(error) = process_lifetime::stop(record.pid, || {
                    record
                        .start_time
                        .is_some_and(|birth| process_lifetime::birth(record.pid) == Some(birth))
                        || record.matches(&self.base)
                }) {
                    eprintln!("fixture retained {}: {error}", self.base.display());
                    if let Some(root) = self.root.take() {
                        std::mem::forget(root);
                    }
                }
            }
        }
        if self.record.is_none() {
            eprintln!(
                "fixture retained {}: no proven server record",
                self.base.display()
            );
            if let Some(root) = self.root.take() {
                std::mem::forget(root);
            }
        }
        self.owner.stdin.take();
        let _ = self.owner.kill();
        let _ = self.owner.wait();
    }
}

#[test]
fn dead_owner_sweep_stops_the_real_recorded_server_and_removes_probe_copy() {
    let mut probe = Probe::start();
    let exit = ExitWatch::new(probe.record().pid).unwrap();
    let binary = probe.record().binary.clone();
    probe.orphan();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(!probe.base.exists(), "dead owner's sandbox was not swept");
    assert!(!binary.exists(), "copied unfixed binary leaked");
    exit.wait(Duration::from_secs(5)).unwrap();
}

#[test]
fn live_owner_and_its_real_server_are_kept() {
    let probe = Probe::start();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(probe.base.exists());
    assert!(probe.record().matches(&probe.base));
    assert!(process_lifetime::pid_alive(probe.owner.id()));
}

#[test]
fn recorded_pid_now_belonging_to_an_unrelated_process_is_not_signalled() {
    let mut probe = Probe::start();
    let mut sleeper = Command::new("/bin/sh")
        .args(["-c", "read -r finish"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut wrong = probe.record().clone();
    wrong.pid = sleeper.id();
    wrong.start_time = process_lifetime::birth(sleeper.id());
    fs::write(
        probe
            .base
            .join(format!(".server-{}.json", probe.record().pid)),
        serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();
    probe.orphan();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    let survived = sleeper.try_wait().unwrap().is_none();
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert!(survived, "unrelated fixture process was signalled");
    assert!(probe.base.exists());
    assert!(
        probe.record().matches(&probe.base),
        "validation must precede every signal"
    );
}

#[test]
fn changed_birth_or_unreadable_identity_retains_the_sandbox() {
    let mut probe = Probe::start();
    probe.orphan();
    let mut record = probe.record().clone();
    record.start_time = Some(process_lifetime::birth(record.pid).unwrap().wrapping_add(1));
    record.write(&probe.base).unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(probe.base.exists());
    assert!(probe.record().matches(&probe.base));
    fs::write(
        probe.base.join(format!(".server-{}.json", record.pid)),
        b"not-json",
    )
    .unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(probe.base.exists());
    assert!(probe.record().matches(&probe.base));
}

#[test]
fn recorded_binary_home_and_state_must_match_the_running_server() {
    let probe = Probe::start();
    let mut changed = probe.record().clone();
    changed.home = probe.base.join("other-home");
    fs::create_dir(&changed.home).unwrap();
    assert!(!changed.matches(&probe.base));
    changed = probe.record().clone();
    changed.state = probe.base.join("other-state");
    fs::create_dir(&changed.state).unwrap();
    assert!(!changed.matches(&probe.base));
    changed = probe.record().clone();
    let other = probe.base.join("other-bin");
    fs::create_dir(&other).unwrap();
    changed.binary = other.join("flk");
    fs::copy(&probe.record().binary, &changed.binary).unwrap();
    assert!(!changed.matches(&probe.base));
    assert!(probe.record().matches(&probe.base));
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
fn server_argument_in_a_client_command_never_proves_server_identity() {
    assert!(sandboxes::server_argv(&["flk".into(), "server".into()]));
    assert!(!sandboxes::server_argv(&[
        "flk".into(),
        "msg".into(),
        "server".into(),
        "hello".into()
    ]));
    assert!(!sandboxes::server_argv(&["server".into()]));
}
