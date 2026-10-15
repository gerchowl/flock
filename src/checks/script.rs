//! One-shot script check executor (#175 phase 4 commit 2).
//!
//! Exit semantics (§8.1 — THE core invariant of the mechanical checks
//! design):
//!
//! - `exit 0` → [`Outcome::Fire`] — the check tripped, the runner will
//!   consider it toward the debounce.
//! - any non-zero exit → [`Outcome::Pass`] — the check ran and reported OK.
//! - timeout / signal termination / spawn failure / stuck child →
//!   [`Outcome::Error`] — the check MECHANICS failed; the runner leaves the
//!   debounce counter alone and records the last-outcome as Error.
//!
//! The child inherits the caller's process-group but NOT its environment:
//! the executor calls `env_clear()`, injects only `FLOCK_CHECK_NAME` plus the
//! script's explicit allowlist, pins `cwd` to `$HOME`, and always clears
//! `RUST_LOG` (it wouldn't survive `env_clear()` but the intent is documented
//! for the day this grows a partial-clear path). stdout/stderr are captured
//! bounded to [`OUTPUT_CAP_BYTES`] each — a runaway `yes(1)` cannot bloat the
//! log tail.
//!
//! Timeout enforcement: a supervisor loop wakes every
//! [`WATCHDOG_POLL_INTERVAL`] and, past `timeout_secs`, sends `SIGTERM`; if the
//! child hasn't exited after [`SIGTERM_TO_SIGKILL_GRACE`], it sends `SIGKILL`.
//! Either path returns `Outcome::Error(reason)` — the runner sees no
//! ambiguity between "exit 0" and "was killed while running".

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::config::{accepted_program_path, ScriptCheck};

/// Cap on captured stdout/stderr per stream (per check run). Beyond this the
/// reader keeps draining but discards — the child never fills the pipe and
/// the audit trail stays bounded.
pub(crate) const OUTPUT_CAP_BYTES: usize = 4 * 1024;
/// Time between `try_wait` polls on the child. Small enough that the
/// timeout precision is sub-second, large enough that a well-behaved short
/// check adds ~one syscall on the way out.
pub(crate) const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// After `timeout_secs`, the executor SIGTERMs the child. If the child
/// hasn't exited within this grace, it escalates to SIGKILL.
pub(crate) const SIGTERM_TO_SIGKILL_GRACE: Duration = Duration::from_secs(5);

/// Outcome of one script run. See module doc for the exit-code contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `exit 0` — the check tripped this run.
    Fire,
    /// non-zero exit — the check ran cleanly and did not trip.
    Pass,
    /// Spawn or wait failure, timeout, or signal-termination. The `String` is a
    /// bounded, human-readable reason.
    Error(String),
}

/// Bounded stdout/stderr and capture diagnostics from a completed run.
/// Capture failures mark the affected stream incomplete; the runner warns on changes.
/// they never change exit classification, even when the caller discards output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
    /// Captured stdout bytes, bounded to `OUTPUT_CAP_BYTES`.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes, bounded to `OUTPUT_CAP_BYTES`.
    pub stderr: Vec<u8>,
    /// Stdout exceeded the byte cap or could not be collected completely.
    pub stdout_truncated: bool,
    /// Stderr exceeded the byte cap or could not be collected completely.
    pub stderr_truncated: bool,
    /// Why stdout capture failed; absence alone does not rule out byte truncation.
    pub stdout_note: Option<String>,
    /// Why stderr capture failed; absence alone does not rule out byte truncation.
    pub stderr_note: Option<String>,
}

/// Failure diagnostics for one capture run, independent of the script outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CaptureNotes {
    /// Why stdout capture failed, or `None` when capture completed.
    pub stdout: Option<String>,
    /// Why stderr capture failed, or `None` when capture completed.
    pub stderr: Option<String>,
}

/// Run one script check to completion (or timeout). See the module doc for
/// the full contract; the return alone is what the runner consumes.
pub(crate) fn run_script(check: &ScriptCheck) -> (Outcome, CapturedOutput) {
    run_script_with_executor(check, &Watchdog)
}

/// Execution and capture policy, separate from command setup and classification.
/// Production uses the watchdog deadlines. Other callers can wait on completion
/// with failure-only bounds without charging scheduling delay as a check timeout.
trait ScriptExecutor {
    /// Reap the child and return its status plus whether its deadline fired.
    fn wait(&self, child: &mut Child, timeout: Duration) -> Result<(ExitStatus, bool), String>;
    /// Collect bounded bytes or report a capture failure independently of exit status.
    fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String>;
}

/// Production child supervision and one-second capture collection policy.
struct Watchdog;

/// Share command setup and exit classification across execution policies.
fn run_script_with_executor(
    check: &ScriptCheck,
    executor: &impl ScriptExecutor,
) -> (Outcome, CapturedOutput) {
    let Some(program) = accepted_program_path(&check.program) else {
        return (
            Outcome::Error(format!(
                "program {:?} is not absolute and not under ~/.flock/checks/",
                check.program
            )),
            CapturedOutput::default(),
        );
    };
    // HOME cwd — refuse to run without one. The script contract says cwd is
    // `$HOME`; a fabricated `/tmp` would silently violate it (§8.1).
    let Some(home) = std::env::var_os("HOME") else {
        return (
            Outcome::Error("HOME is unset; cannot pin cwd for check".into()),
            CapturedOutput::default(),
        );
    };

    let mut command = Command::new(&program);
    command
        .args(&check.args)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        // Even though env_clear removes RUST_LOG, keep the explicit remove
        // as documentation: a future partial-clear path would need to know.
        .env_remove("RUST_LOG")
        .env("FLOCK_CHECK_NAME", &check.name);
    for (key, value) in &check.env {
        command.env(key, value);
    }

    let mut traced = crate::process::TracedCommand::from_command(command, "checks");
    let mut child = match traced.spawn_traced() {
        Ok(child) => child,
        Err(err) => {
            return (
                Outcome::Error(format!("spawn failed: {err}")),
                CapturedOutput::default(),
            );
        }
    };
    let stdout_rx = child.stdout.take().map(spawn_bounded_reader);
    let stderr_rx = child.stderr.take().map(spawn_bounded_reader);
    let timeout = Duration::from_secs(check.timeout_secs.max(1));
    let (status, timed_out) = match executor.wait(&mut child, timeout) {
        Ok(result) => result,
        Err(reason) => return (Outcome::Error(reason), CapturedOutput::default()),
    };

    // Attempt both collectors even if one fails, so a completed stream survives.
    let stdout = stdout_rx.map(|rx| executor.collect(rx)).transpose();
    let stderr = stderr_rx.map(|rx| executor.collect(rx)).transpose();
    let (stdout, stdout_truncated, stdout_note) = finish_capture(stdout);
    let (stderr, stderr_truncated, stderr_note) = finish_capture(stderr);
    let output = CapturedOutput {
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        stdout_note,
        stderr_note,
    };
    if timed_out {
        return (
            Outcome::Error(format!("timed out after {}s", timeout.as_secs())),
            output,
        );
    }

    // Signal-terminated → Error. On Unix, `ExitStatus::code()` returns None
    // when the child was killed by signal; `.signal()` names it. The classify
    // order matters: check signal BEFORE `.code()` so a SIGTERM/SIGSEGV never
    // gets classified as Pass.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (
                Outcome::Error(format!("terminated by signal {signal}")),
                output,
            );
        }
    }
    match status.code() {
        Some(0) => (Outcome::Fire, output),
        Some(_) => (Outcome::Pass, output),
        None => (
            Outcome::Error("no exit code and not signal-terminated".into()),
            output,
        ),
    }
}

/// Preserve a completed stream, or mark failed capture and retain its reason.
/// The runner reports changed capture diagnostics once per check. Neither path
/// classifies a check.
fn finish_capture(
    result: Result<Option<(Vec<u8>, bool)>, String>,
) -> (Vec<u8>, bool, Option<String>) {
    match result {
        Ok(Some((bytes, truncated))) => (bytes, truncated, None),
        Ok(None) => (Vec::new(), false, None),
        Err(reason) => (Vec::new(), true, Some(reason)),
    }
}

impl ScriptExecutor for Watchdog {
    fn wait(&self, child: &mut Child, timeout: Duration) -> Result<(ExitStatus, bool), String> {
        let pid = child.id();
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;

        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        send_signal(pid, libc::SIGTERM);
                        let escalate_at = Instant::now() + SIGTERM_TO_SIGKILL_GRACE;
                        while Instant::now() < escalate_at {
                            thread::sleep(WATCHDOG_POLL_INTERVAL);
                            if matches!(child.try_wait(), Ok(Some(_))) {
                                break;
                            }
                        }
                        if !matches!(child.try_wait(), Ok(Some(_))) {
                            send_signal(pid, libc::SIGKILL);
                        }
                        break;
                    }
                    thread::sleep(WATCHDOG_POLL_INTERVAL);
                }
                Err(err) => {
                    return Err(format!("wait failed: {err}"));
                }
            }
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(err) => {
                return Err(format!("wait failed: {err}"));
            }
        };

        Ok((status, timed_out))
    }

    fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
        collect_bounded(rx)
    }
}

fn send_signal(pid: u32, signal: libc::c_int) {
    if pid == 0 {
        return;
    }
    // SAFETY: `libc::kill` is signal-safe and thread-safe; we pass a
    // well-formed pid (positive u32 from Child::id) and a valid signal.
    unsafe {
        libc::kill(pid as libc::c_int, signal);
    }
}

/// One bounded reader result, or its explicit read-failure reason.
type CaptureReceiver = mpsc::Receiver<Result<(Vec<u8>, bool), String>>;

/// Spawn a reader thread for one bounded pipe. The thread reads up to
/// `OUTPUT_CAP_BYTES` into a Vec, then drain-drops the tail so the child
/// never blocks on a full pipe. EOF sends the bounded output; a read error
/// sends a failure instead of silently treating partial bytes as complete.
fn spawn_bounded_reader<R>(mut source: R) -> CaptureReceiver
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::with_capacity(OUTPUT_CAP_BYTES);
        let mut truncated = false;
        let mut discard = [0u8; 4096];
        loop {
            if buf.len() >= OUTPUT_CAP_BYTES {
                match source.read(&mut discard) {
                    Ok(0) => break,
                    Ok(_) => {
                        truncated = true;
                        continue;
                    }
                    Err(err) => {
                        let _ = tx.send(Err(format!("read failed: {err}")));
                        return;
                    }
                }
            }
            let remaining = OUTPUT_CAP_BYTES - buf.len();
            let mut chunk = vec![0u8; remaining.min(4096)];
            match source.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buf.extend_from_slice(&chunk[..read]),
                Err(err) => {
                    let _ = tx.send(Err(format!("read failed: {err}")));
                    return;
                }
            }
        }
        let _ = tx.send(Ok((buf, truncated)));
    });
    rx
}

/// Collect one stream with the unchanged one-second production bound. A late,
/// disconnected, or failed reader becomes an output note through
/// `finish_capture`, never a different `Outcome`. The runner reports changed
/// notes once per check. The other stream is retained.
/// Reader threads can outlive the run until pipe EOF; this bound does not
/// terminate descendants that inherited the pipe. Failed reads do not expose
/// partial bytes as complete output.
fn collect_bounded(rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
    rx.recv_timeout(Duration::from_secs(1))
        .map_err(|err| match err {
            mpsc::RecvTimeoutError::Timeout => "reader timed out".to_owned(),
            mpsc::RecvTimeoutError::Disconnected => "reader disconnected".to_owned(),
        })?
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests write fixture scripts to a temp dir with std::fs; the executor path itself uses TracedCommand.
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// ADR-0037 §1: completion decides success; this ceiling only fails missing
    /// evidence. Killing the owned child wakes wait and scope joins its reaper.
    struct Completion;
    const FAILURE_CEILING: Duration = Duration::from_secs(60);

    impl ScriptExecutor for Completion {
        fn wait(
            &self,
            child: &mut Child,
            _timeout: Duration,
        ) -> Result<(ExitStatus, bool), String> {
            let pid = child.id();
            thread::scope(|scope| {
                let (tx, rx) = mpsc::channel();
                scope.spawn(move || {
                    let _ = tx.send(child.wait());
                });
                match rx.recv_timeout(FAILURE_CEILING) {
                    Ok(result) => result
                        .map(|status| (status, false))
                        .map_err(|err| err.to_string()),
                    Err(err) => {
                        send_signal(pid, libc::SIGKILL);
                        Err(format!("completion producer failed: {err}"))
                    }
                }
            })
        }

        fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
            rx.recv_timeout(FAILURE_CEILING)
                .map_err(|err| format!("capture producer failed: {err}"))?
        }
    }

    /// Use completion evidence for the script and the real capture watchdog.
    /// A background descendant may hold a pipe open after its parent exits.
    #[derive(Default)]
    struct CompletionWithWatchdogCapture {
        waits: std::cell::Cell<usize>,
        collects: std::cell::Cell<usize>,
    }

    impl ScriptExecutor for CompletionWithWatchdogCapture {
        fn wait(&self, child: &mut Child, timeout: Duration) -> Result<(ExitStatus, bool), String> {
            self.waits.set(self.waits.get() + 1);
            Completion.wait(child, timeout)
        }

        fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
            self.collects.set(self.collects.get() + 1);
            Watchdog.collect(rx)
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "flock-checks-{label}-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn script(program: PathBuf, timeout_secs: u64) -> ScriptCheck {
        ScriptCheck {
            name: "test".into(),
            program,
            timeout_secs,
            ..ScriptCheck::default()
        }
    }

    /// A check whose script may call out to real binaries.
    ///
    /// `run_script` deliberately `env_clear()`s, so a script it runs has no
    /// `PATH`. On an FHS distro `/bin/sh` still finds `sleep` or `dd` through
    /// its compiled-in fallback (`/bin:/usr/bin`); on NixOS that fallback
    /// resolves to nothing and the script dies "command not found". Pass
    /// `PATH` the way a real check with an `env` block would.
    fn script_with_path(program: PathBuf, timeout_secs: u64) -> ScriptCheck {
        let mut check = script(program, timeout_secs);
        let (key, value) = crate::test_support::path_env_pair();
        check.env.insert(key, value);
        check
    }

    #[test]
    fn exit_zero_fires() {
        let dir = temp_dir("fires");
        let script_path = write_script(&dir, "fire.sh", "#!/bin/sh\nexit 0\n");
        let (outcome, _output) = run_script_with_executor(&script(script_path, 5), &Completion);
        assert_eq!(outcome, Outcome::Fire);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nonzero_exit_passes() {
        let dir = temp_dir("passes");
        let script_path = write_script(&dir, "pass.sh", "#!/bin/sh\nexit 7\n");
        let (outcome, _output) = run_script_with_executor(&script(script_path, 5), &Completion);
        assert_eq!(outcome, Outcome::Pass);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn silent_producer_times_out_and_is_reaped() {
        use std::cell::Cell;
        struct ObservedWatchdog(Cell<u32>);
        impl ScriptExecutor for ObservedWatchdog {
            fn wait(
                &self,
                child: &mut Child,
                timeout: Duration,
            ) -> Result<(ExitStatus, bool), String> {
                self.0.set(child.id());
                let result = Watchdog.wait(child, timeout)?;
                assert!(result.1, "silent child must reach the deadline");
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(result.0.signal(), Some(libc::SIGTERM));
                Ok(result)
            }
            fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
                Watchdog.collect(rx)
            }
        }
        let dir = temp_dir("timeout");
        let path = write_script(&dir, "silent.sh", "#!/bin/sh\nwhile :; do :; done\n");
        let observer = ObservedWatchdog(Cell::new(0));
        let (outcome, output) = run_script_with_executor(&script(path, 1), &observer);
        match outcome {
            Outcome::Error(reason) => assert!(reason.contains("timed out"), "{reason}"),
            other => panic!("expected timeout error, got {other:?}"),
        }
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let mut status = 0;
        // SAFETY: the recorded pid is our child, and status points to a valid int.
        assert_eq!(
            unsafe { libc::waitpid(observer.0.get() as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn signal_terminated_errors() {
        let dir = temp_dir("signal");
        // Kill self with SIGKILL — non-zero exit shape but *by signal*, not
        // exit code. Must classify as Error, never Pass (§8.1).
        let script_path = write_script(&dir, "kill.sh", "#!/bin/sh\nkill -9 $$\n");
        let (outcome, _output) = run_script_with_executor(&script(script_path, 5), &Completion);
        match outcome {
            Outcome::Error(reason) => assert!(
                reason.contains("signal"),
                "expected signal reason, got: {reason}"
            ),
            other => panic!("expected Error(signal), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stdout_capture_is_bounded() {
        let dir = temp_dir("bounded");
        // Emit far more than OUTPUT_CAP_BYTES; verify the captured payload
        // never exceeds the cap and the truncated flag flips.
        let script_path = write_script(
            &dir,
            "loud.sh",
            &format!(
                "#!/bin/sh\ndd if=/dev/zero bs=1024 count={} 2>/dev/null\nexit 0\n",
                (OUTPUT_CAP_BYTES / 1024) * 4
            ),
        );
        let (outcome, output) =
            run_script_with_executor(&script_with_path(script_path, 5), &Completion);
        assert_eq!(outcome, Outcome::Fire);
        assert!(output.stdout.len() <= OUTPUT_CAP_BYTES);
        assert!(
            output.stdout_truncated,
            "expected stdout truncation flag on a > cap payload"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn spawn_failure_returns_error() {
        // Absolute path that doesn't exist — spawn fails cleanly.
        let check = script(
            PathBuf::from("/nonexistent/definitely-not-a-real-check-flock"),
            5,
        );
        let (outcome, _output) = run_script(&check);
        match outcome {
            Outcome::Error(reason) => {
                assert!(reason.contains("spawn"), "reason: {reason}")
            }
            other => panic!("expected Error(spawn), got {other:?}"),
        }
    }

    #[test]
    fn env_is_scrubbed_to_the_allowlist_plus_flock_check_name() {
        let dir = temp_dir("env");
        // The child inherits ONLY what the executor injects: the explicit
        // allowlist + FLOCK_CHECK_NAME. Exit-0 gates on both being set to
        // exactly the expected values, and RUST_LOG being empty.
        let script_path = write_script(
            &dir,
            "env.sh",
            r#"#!/bin/sh
[ "$FLOCK_CHECK_NAME" = "envtest" ] || exit 11
[ "$LC_ALL" = "C" ] || exit 12
[ -z "$RUST_LOG" ] || exit 13
[ -z "$FLOCK_HOST_NAME" ] || exit 14
exit 0
"#,
        );
        // Set RUST_LOG and FLOCK_HOST_NAME in the parent to prove they
        // don't leak.
        std::env::set_var("RUST_LOG", "flock=debug");
        std::env::set_var("FLOCK_HOST_NAME", "parent");
        let mut check = script(script_path, 5);
        check.name = "envtest".into();
        check.env.insert("LC_ALL".into(), "C".into());
        let (outcome, _output) = run_script_with_executor(&check, &Completion);
        assert_eq!(outcome, Outcome::Fire, "env leak check must fire");
        std::env::remove_var("FLOCK_HOST_NAME");
        // RUST_LOG left set is fine — nextest owns the parent env.
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_rejects_dead_reader() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert!(collect_bounded(rx).unwrap_err().contains("disconnected"));
    }

    #[test]
    fn capture_rejects_live_silent_reader() {
        let (tx, rx) = mpsc::channel();
        assert!(collect_bounded(rx).unwrap_err().contains("timed out"));
        drop(tx);
    }

    #[test]
    fn capture_reports_read_errors_before_and_after_cap() {
        struct BrokenReader(usize);
        impl Read for BrokenReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::other("fixture read failure"));
                }
                let count = self.0.min(buf.len());
                buf[..count].fill(b'x');
                self.0 -= count;
                Ok(count)
            }
        }
        for length in [0, OUTPUT_CAP_BYTES] {
            let rx = spawn_bounded_reader(BrokenReader(length));
            assert_eq!(
                Completion.collect(rx),
                Err("read failed: fixture read failure".into())
            );
        }
    }

    #[test]
    fn capture_failure_preserves_exit_and_other_stream() {
        use std::cell::Cell;
        struct FailedCapture {
            call: Cell<usize>,
            fail_at: usize,
        }
        impl ScriptExecutor for FailedCapture {
            fn wait(
                &self,
                child: &mut Child,
                timeout: Duration,
            ) -> Result<(ExitStatus, bool), String> {
                Completion.wait(child, timeout)
            }
            fn collect(&self, rx: CaptureReceiver) -> Result<(Vec<u8>, bool), String> {
                let call = self.call.get();
                self.call.set(call + 1);
                let result = Completion.collect(rx)?;
                if call == self.fail_at {
                    Err("fixture capture failure".into())
                } else {
                    Ok(result)
                }
            }
        }
        let dir = temp_dir("capture-failure");
        let path = write_script(&dir, "output.sh", "#!/bin/sh\nprintf out\nprintf err >&2\n");
        for fail_at in [0, 1] {
            let (outcome, output) = run_script_with_executor(
                &script(path.clone(), 5),
                &FailedCapture {
                    call: Cell::new(0),
                    fail_at,
                },
            );
            let stream = if fail_at == 0 { "stdout" } else { "stderr" };
            assert_eq!(
                outcome,
                Outcome::Fire,
                "{stream} failure must not change exit classification"
            );
            if fail_at == 0 {
                assert!(output.stdout.is_empty());
                assert!(output.stdout_truncated);
                assert_eq!(
                    output.stdout_note.as_deref(),
                    Some("fixture capture failure")
                );
                assert_eq!(output.stderr, b"err");
                assert!(output.stderr_note.is_none());
            } else {
                assert_eq!(output.stdout, b"out");
                assert!(output.stdout_note.is_none());
                assert!(output.stderr.is_empty());
                assert!(output.stderr_truncated);
                assert_eq!(
                    output.stderr_note.as_deref(),
                    Some("fixture capture failure")
                );
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn background_descendant_capture_failure_preserves_exit_classification() {
        use std::os::unix::ffi::OsStrExt;
        let dir = temp_dir("background-capture");
        let release_path = dir.join("release");
        let ready_path = dir.join("ready");
        let pid_path = dir.join("pid");
        for path in [&release_path, &ready_path] {
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: name is a valid C path in the fixture's own directory.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }
        for (code, expected) in [(0, Outcome::Fire), (7, Outcome::Pass)] {
            // EOF releases the background reader if fixture setup panics.
            let release = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&release_path)
                .unwrap();
            let path = write_script(
                &dir,
                "background.sh",
                &format!(
                    r#"#!/bin/sh
(
exec 3<"$1"
printf 'ready\n' >"$2"
read -r token <&3
) 2>/dev/null &
printf '%s\n' "$!" >"$3"
read -r ready <"$2" || exit 81
[ "$ready" = ready ] || exit 82
exit {code}
"#
                ),
            );
            let mut check = script(path, 5);
            for path in [&release_path, &ready_path, &pid_path] {
                check.args.push(path.to_string_lossy().into_owned());
            }
            let executor = CompletionWithWatchdogCapture::default();
            let result = run_script_with_executor(&check, &executor);
            // The ready handshake proves this owned descendant opened its
            // release pipe before the script exited. The OS reaps this orphan
            // after termination, as for real background descendants.
            let pid = std::fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            send_signal(pid, libc::SIGKILL);
            drop(release);
            let (outcome, output) = result;
            assert_eq!(outcome, expected);
            assert!(output.stdout_truncated);
            assert!(output
                .stdout_note
                .as_deref()
                .is_some_and(|note| note.contains("timed out")));
            assert_eq!(executor.waits.get(), 1, "script must use completion wait");
            assert_eq!(
                executor.collects.get(),
                2,
                "both streams use watchdog collection"
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
