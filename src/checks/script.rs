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
use std::process::{Command, Stdio};
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
    /// spawn failure, timeout, or signal-termination. The `String` is a
    /// bounded, human-readable reason.
    Error(String),
}

/// Bounded stdout/stderr from a completed run. Kept for the executor's
/// log side; the runner never reads it (Outcome carries the decision).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CapturedOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Run one script check to completion (or timeout). See the module doc for
/// the full contract; the return alone is what the runner consumes.
pub(crate) fn run_script(check: &ScriptCheck) -> (Outcome, CapturedOutput) {
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
    let pid = child.id();

    let stdout_rx = child.stdout.take().map(spawn_bounded_reader);
    let stderr_rx = child.stderr.take().map(spawn_bounded_reader);

    let timeout = Duration::from_secs(check.timeout_secs.max(1));
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
                return (
                    Outcome::Error(format!("wait failed: {err}")),
                    CapturedOutput::default(),
                );
            }
        }
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(err) => {
            return (
                Outcome::Error(format!("wait failed: {err}")),
                CapturedOutput::default(),
            );
        }
    };

    let stdout = stdout_rx.map(collect_bounded).unwrap_or_default();
    let stderr = stderr_rx.map(collect_bounded).unwrap_or_default();
    let output = CapturedOutput {
        stdout: stdout.0,
        stdout_truncated: stdout.1,
        stderr: stderr.0,
        stderr_truncated: stderr.1,
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

/// Spawn a reader thread for one bounded pipe. The thread reads up to
/// `OUTPUT_CAP_BYTES` into a Vec, then drain-drops the tail so the child
/// never blocks on a full pipe. On EOF or read error the channel sender is
/// dropped; the collector picks up whatever arrived.
fn spawn_bounded_reader<R>(mut source: R) -> mpsc::Receiver<Result<(Vec<u8>, bool), String>>
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
                    Err(_) => break,
                }
            }
            let remaining = OUTPUT_CAP_BYTES - buf.len();
            let mut chunk = vec![0u8; remaining.min(4096)];
            match source.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buf.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
        let _ = tx.send(Ok((buf, truncated)));
    });
    rx
}

fn collect_bounded(rx: mpsc::Receiver<Result<(Vec<u8>, bool), String>>) -> (Vec<u8>, bool) {
    match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok((buf, truncated))) => (buf, truncated),
        _ => (Vec::new(), false),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Tests write fixture scripts to a temp dir with std::fs; the executor path itself uses TracedCommand.
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

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
        let (outcome, _output) = run_script(&script(script_path, 5));
        assert_eq!(outcome, Outcome::Fire);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nonzero_exit_passes() {
        let dir = temp_dir("passes");
        let script_path = write_script(&dir, "pass.sh", "#!/bin/sh\nexit 7\n");
        let (outcome, _output) = run_script(&script(script_path, 5));
        assert_eq!(outcome, Outcome::Pass);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sleep_past_timeout_errors() {
        let dir = temp_dir("timeout");
        // sleep well past a 1s timeout — the watchdog SIGTERMs, then
        // (well within the 5s grace) the child dies and try_wait returns.
        let script_path = write_script(&dir, "sleep.sh", "#!/bin/sh\nsleep 10\n");
        let (outcome, _output) = run_script(&script_with_path(script_path, 1));
        match outcome {
            Outcome::Error(reason) => assert!(
                reason.contains("timed out"),
                "expected timed-out reason, got: {reason}"
            ),
            other => panic!("expected Error(timed out), got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn signal_terminated_errors() {
        let dir = temp_dir("signal");
        // Kill self with SIGKILL — non-zero exit shape but *by signal*, not
        // exit code. Must classify as Error, never Pass (§8.1).
        let script_path = write_script(&dir, "kill.sh", "#!/bin/sh\nkill -9 $$\n");
        let (outcome, _output) = run_script(&script(script_path, 5));
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
        let (outcome, output) = run_script(&script_with_path(script_path, 5));
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
        let (outcome, _output) = run_script(&check);
        assert_eq!(outcome, Outcome::Fire, "env leak check must fire");
        std::env::remove_var("FLOCK_HOST_NAME");
        // RUST_LOG left set is fine — nextest owns the parent env.
        let _ = std::fs::remove_dir_all(dir);
    }
}
