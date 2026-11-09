//! Real crash recovery, including the copied probe binary, without live profiles.
//! Producer stages and failure ceilings follow ADR-0037 §1.
#![allow(clippy::disallowed_methods)]
mod support;

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Stdio};
use std::time::{Duration, Instant};
use support::environment::Command;
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
        assert_eq!(
            probe.record().start_time,
            process_lifetime::birth(probe.record().pid)
        );
        assert!(probe.record().start_time.is_some());
        assert!(probe.record().matches(&probe.base));
        assert!(probe.record().binary.starts_with(&probe.base));
        probe
    }

    fn line(&mut self, death: &ExitWatch) -> String {
        stage_line(&mut self.stdout, death)
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

fn stage_line(stdout: &mut ChildStdout, death: &ExitWatch) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut line = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("probe stage ceiling");
        let mut events = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
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
            stdout.read(&mut byte).unwrap(),
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

#[test]
fn dead_owner_server_is_recovered_after_its_binary_is_replaced() {
    let mut probe = Probe::start();
    let exit = ExitWatch::new(probe.record().pid).unwrap();
    probe.orphan();
    let old = probe.base.join("old-flk");
    fs::rename(&probe.record().binary, &old).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_flk"), &probe.record().binary).unwrap();
    fs::remove_file(old).unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(!probe.base.exists(), "rebuild stranded the recorded server");
    exit.wait(Duration::from_secs(5)).unwrap();
}

#[test]
fn missing_birth_proof_retains_the_sandbox_and_its_server() {
    let mut probe = Probe::start();
    probe.orphan();
    let mut incomplete = probe.record().clone();
    incomplete.start_time = None;
    incomplete.write(&probe.base).unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(
        probe.base.exists(),
        "missing birth proof authorized cleanup"
    );
    assert!(probe.record().matches(&probe.base));
}

#[test]
fn runtime_recording_excludes_the_client_candidate() {
    let probe = Probe::start();
    let root = SocketDir::reserve("2162");
    let base = root.path().join(format!(
        "flock-autodetect-test-{}-client",
        std::process::id()
    ));
    let config = base.join("config");
    let runtime = base.join("runtime");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir(&runtime).unwrap();
    sandboxes::record_runtime_candidates(
        &config,
        &runtime,
        Some(probe.record().pid),
        [probe.record().pid],
    );
    assert!(
        !base
            .join(format!(".server-{}.json", probe.record().pid))
            .exists(),
        "client candidate became a durable server record"
    );
}

struct SignalChild(Child);
impl SignalChild {
    fn start(ignore: bool) -> Self {
        let code = format!("import os,signal\nsignal.signal(signal.SIGTERM, lambda *_: (os.write(1,b'TERM\\n'), {}))\nos.write(1,b'READY\\n')\nwhile True: signal.pause()\n", if ignore { "None" } else { "os._exit(0)" });
        let mut fixture = Self(
            Command::new("python3")
                .args(["-c", &code])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let death = ExitWatch::new(fixture.0.id()).unwrap();
        assert_eq!(
            stage_line(fixture.0.stdout.as_mut().unwrap(), &death),
            "READY\n"
        );
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
    use std::os::unix::process::ExitStatusExt;
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

#[test]
fn outside_sandbox_server_with_matching_argv_and_birth_is_not_signalled() {
    let mut probe = Probe::start();
    let outside = Probe::start();
    probe.orphan();
    let mut wrong = outside.record().clone();
    wrong.home = probe.record().home.clone();
    wrong.state = probe.record().state.clone();
    fs::write(
        probe
            .base
            .join(format!(".server-{}.json", probe.record().pid)),
        serde_json::to_vec(&wrong).unwrap(),
    )
    .unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(probe.base.exists());
    assert!(outside.record().matches(&outside.base));
    assert!(probe.record().matches(&probe.base));
}

#[test]
fn missing_exact_argv_proof_retains_the_server() {
    let mut probe = Probe::start();
    probe.orphan();
    let mut incomplete = probe.record().clone();
    incomplete.argv.clear();
    incomplete.write(&probe.base).unwrap();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    assert!(probe.base.exists());
    assert!(probe.record().matches(&probe.base));
}

#[test]
fn direct_spawn_records_known_launch_argv_before_exec_is_visible() {
    let fixture = SignalChild::start(true);
    let root = SocketDir::reserve("2162");
    let base = root.path().join(format!(
        "flock-autodetect-test-{}-preexec",
        std::process::id()
    ));
    let config = base.join("config");
    let runtime = base.join("runtime");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir(&runtime).unwrap();
    // A forked child can still expose inherited argv before exec. This owned
    // producer supplies different argv deterministically; no signal uses this record.
    let copied = base.join("bin/flk");
    fs::create_dir(copied.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_flk"), &copied).unwrap();
    let alias = base.join("flk");
    std::os::unix::fs::symlink(&copied, &alias).unwrap();
    let binary = sandboxes::prepare_launch(&alias, &runtime).unwrap();
    fs::remove_file(alias).unwrap();
    fs::remove_file(copied).unwrap();
    sandboxes::record_spawn(fixture.0.id(), &config, &runtime);
    let record: ServerRecord = serde_json::from_slice(
        &fs::read(base.join(format!(".server-{}.json", fixture.0.id()))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record.argv,
        vec![binary.to_string_lossy().into_owned(), "server".into()]
    );
    assert_eq!(record.binary, binary);
    assert_eq!(record.start_time, process_lifetime::birth(fixture.0.id()));
}

fn relative_launch_owner(root: &std::path::Path) {
    let base = root.join(format!(
        "flock-autodetect-test-{}-relative",
        std::process::id()
    ));
    let config = base.join("config");
    let runtime = base.join("runtime");
    fs::create_dir_all(config.join(support::paths::app_dir_name())).unwrap();
    fs::create_dir(&runtime).unwrap();
    fs::write(
        config
            .join(support::paths::app_dir_name())
            .join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();
    let alias = base.join("flk");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_flk"), &alias).unwrap();
    sandboxes::prepare_launch(&alias, &runtime).unwrap();
    let api = root.join("api.sock");
    // Preserve legacy argv spelling while capturing the executable before spawn.
    let server = SignalChild(
        Command::new(&alias)
            .arg0("./flk")
            .arg("server")
            .current_dir(&base)
            .envs(support::environment::isolated_env(&config, &runtime))
            .env("FLOCK_SOCKET_PATH", &api)
            .env("FLOCK_CLIENT_SOCKET_PATH", root.join("client.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    support::wait_for_socket(&api, Duration::from_secs(10));
    sandboxes::record_runtime_candidates(&config, &runtime, None, [server.0.id()]);
    io::stdout().write_all(b"RELATIVE_READY\n").unwrap();
    io::stdout().flush().unwrap();
    io::stdin().read_exact(&mut [0]).unwrap();
}

#[test]
fn relative_argv_server_is_recorded_matched_and_swept() {
    if let Some(root) = std::env::var_os("FLOCK_TEST_RELATIVE_ROOT") {
        relative_launch_owner(std::path::Path::new(&root));
        return;
    }
    let root = SocketDir::reserve("2175");
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "relative_argv_server_is_recorded_matched_and_swept",
            "--nocapture",
            "--format=terse",
            "--color=never",
        ])
        .env("FLOCK_TEST_RELATIVE_ROOT", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let base = root
        .path()
        .join(format!("flock-autodetect-test-{}-relative", owner.id()));
    let stdout = owner.stdout.take().unwrap();
    let mut probe = Probe {
        root: Some(root),
        owner,
        stdout,
        base,
        record: None,
    };
    let death = ExitWatch::new(probe.owner.id()).unwrap();
    assert_eq!(probe.line(&death), "\n");
    assert_eq!(probe.line(&death), "running 1 test\n");
    assert_eq!(probe.line(&death), "RELATIVE_READY\n");
    let entries: Vec<_> = fs::read_dir(&probe.base)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(".server-")
                && entry.file_name().to_string_lossy().ends_with(".json")
        })
        .collect();
    assert_eq!(entries.len(), 1);
    let recorded: ServerRecord =
        serde_json::from_slice(&fs::read(entries[0].path()).unwrap()).unwrap();
    let canonical: PathBuf =
        serde_json::from_slice(&fs::read(probe.base.join(".launch-binary.json")).unwrap()).unwrap();
    // Cleanup uses the captured launch fact even when the regression writes a
    // relative binary. Assertions inspect the unmodified durable record.
    probe.record = Some(ServerRecord {
        binary: canonical.clone(),
        ..recorded.clone()
    });
    assert_eq!(recorded.binary, canonical);
    assert_eq!(recorded.argv[0], "./flk");
    assert_eq!(
        recorded.argv,
        support::process_table::process_info(recorded.pid)
            .unwrap()
            .argv
    );
    assert!(
        recorded.matches(&probe.base),
        "relative invocation must remain provable"
    );
    let marker = probe.base.join(".launch-binary.json");
    let saved = probe.base.join(".saved-launch.json");
    fs::rename(&marker, &saved).unwrap();
    assert!(
        !recorded.matches(&probe.base),
        "relative argv needs its captured executable proof"
    );
    fs::rename(saved, marker).unwrap();
    let mut wrong = recorded.clone();
    wrong.argv[0] = "./other-flk".into();
    assert!(
        !wrong.matches(&probe.base),
        "exact live argv must still match"
    );
    let exit = ExitWatch::new(recorded.pid).unwrap();
    probe.orphan();
    sandboxes::sweep(probe.root.as_ref().unwrap().path());
    exit.wait(Duration::from_secs(5)).unwrap();
    assert!(
        !probe.base.exists(),
        "relative argv stranded the dead-owner sandbox"
    );
}
