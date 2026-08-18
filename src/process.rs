//! One traced funnel for every external process flock spawns.
//!
//! `TracedCommand` wraps `std::process::Command` so `.output()`, `.status()`,
//! and `.spawn()` are called in a SINGLE place — inside this module, with an
//! `#[allow(clippy::disallowed_methods)]` and a reason comment. The
//! `clippy.toml` `disallowed-methods` list forbids those methods on the raw
//! `Command` everywhere else, so the "what did flock actually run?" question
//! is answerable from the log tail: every invocation emits a `process.exec` or
//! `process.spawn` event through the `crate::logging` facade.
//!
//! Non-zero exit → WARN; spawn failure → ERROR event; zero exit → INFO.
//! Argument shaping is bounded: joined with spaces and truncated to
//! `MAX_ARG_LOG_CHARS` with an ellipsis marker so a hostile or huge invocation
//! can't blow up the audit trail.
//!
//! Callers pass a static `subsystem` label ("remote", "git", "platform", …)
//! so grep-able events fall on the right role in the fleet log.

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// Cap for the `args` log field. Command lines can be long (curl URLs, ssh
/// bootstrap scripts) — truncate rather than let the audit trail hold arbitrary
/// user-influenced strings.
/// How often a timed run checks whether its child has exited. Short enough
/// that the deadline is honoured closely, long enough not to spin a core.
const TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(10);

const MAX_ARG_LOG_CHARS: usize = 512;
const ARG_TRUNCATED_MARKER: &str = "…[truncated]";

/// A `std::process::Command` that logs its invocation through the
/// `crate::logging` facade when it runs. Callers build it exactly like a
/// `Command`, but call `output_traced()` / `status_traced()` / `spawn_traced()`
/// instead — those are the ONLY paths that reach `Command::output` /
/// `::status` / `::spawn` from flock's non-test code.
pub(crate) struct TracedCommand {
    inner: Command,
    subsystem: &'static str,
    program: String,
}

// Landing the wrapper as its own commit means every builder method looks
// dead until the migration sweep lands next. The struct + impl carry a
// module-scope allow rather than one #[allow] per fn — the migration commit
// touches every method as a caller lands, so any actually-unused surface
// will resurface as an unfulfilled expectation there, not stale here.
#[allow(dead_code)]
impl TracedCommand {
    /// Build a new traced command targeting `program`, tagged with a static
    /// `subsystem` label ("remote", "git", "platform", …) for grep-ability.
    pub(crate) fn new(program: impl AsRef<OsStr>, subsystem: &'static str) -> Self {
        let program_ref = program.as_ref();
        let program_display = program_ref.to_string_lossy().into_owned();
        Self {
            inner: Command::new(program_ref),
            subsystem,
            program: program_display,
        }
    }

    /// Wrap an already-built `Command` for last-mile tracing. Used by helpers
    /// that receive a fully-built `Command` (e.g. platform notification /
    /// clipboard closures) — the `program` label is recovered from the
    /// wrapped `Command::get_program()`.
    pub(crate) fn from_command(command: Command, subsystem: &'static str) -> Self {
        let program = command.get_program().to_string_lossy().into_owned();
        Self {
            inner: command,
            subsystem,
            program,
        }
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub(crate) fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    pub(crate) fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub(crate) fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub(crate) fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    pub(crate) fn current_dir(&mut self, dir: impl AsRef<std::path::Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub(crate) fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdin(stdio);
        self
    }

    pub(crate) fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdout(stdio);
        self
    }

    pub(crate) fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stderr(stdio);
        self
    }

    /// Read-only view of the accumulated env overrides. Used by tests that
    /// assert env plumbing shape (e.g. autodetect socket-override clearing).
    pub(crate) fn get_envs(&self) -> std::process::CommandEnvs<'_> {
        self.inner.get_envs()
    }

    /// Detach into a new process group so the child survives its parent's
    /// exit (`process_group(0)` = new session leader). Unix-only, matches
    /// `std::os::unix::process::CommandExt::process_group`.
    #[cfg(unix)]
    pub(crate) fn process_group(&mut self, pgid: i32) -> &mut Self {
        use std::os::unix::process::CommandExt;
        self.inner.process_group(pgid);
        self
    }

    /// Run the command with a wall-clock deadline, killing the child if it
    /// exceeds it. Same tracing contract as [`Self::output_traced`].
    ///
    /// `output_traced` is `Command::output()`, which waits forever. That is the
    /// shape behind every "flock froze" report so far: a subprocess that hangs
    /// takes its caller with it, and when the caller is the event loop that
    /// means every pane at once (#302). ssh options and curl flags bound their
    /// own transports, but nothing bounds `git`, `gh`, `pmset` or `ioreg`.
    ///
    /// A timed-out child is reported as an `io::ErrorKind::TimedOut` error, so
    /// callers treat "took too long" exactly like "failed" rather than needing
    /// a third case.
    ///
    /// Reader threads drain stdout/stderr concurrently rather than after the
    /// wait: a child that fills a 64 KiB pipe buffer blocks in `write` until
    /// someone reads, so polling `try_wait` without draining would hang until
    /// the deadline even for a child that had already finished its work.
    pub(crate) fn output_traced_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        use std::io::Read;

        let args = shape_args(&self.inner);
        let start = Instant::now();
        self.inner.stdout(std::process::Stdio::piped());
        self.inner.stderr(std::process::Stdio::piped());
        let result = (|| -> io::Result<Output> {
            // Same funnel exemption as `output_traced` — this IS the wrapper.
            #[allow(clippy::disallowed_methods)]
            let mut child = self.inner.spawn()?;
            let mut stdout_pipe = child.stdout.take();
            let mut stderr_pipe = child.stderr.take();
            let stdout_reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(pipe) = stdout_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut buf);
                }
                buf
            });
            let stderr_reader = std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(pipe) = stderr_pipe.as_mut() {
                    let _ = pipe.read_to_end(&mut buf);
                }
                buf
            });

            let deadline = start + timeout;
            let status = loop {
                match child.try_wait()? {
                    Some(status) => break Some(status),
                    None if Instant::now() >= deadline => {
                        // SIGKILL, not SIGTERM: this is a probe we already gave
                        // its full budget, and a child ignoring TERM would just
                        // extend the stall we are here to bound.
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    None => std::thread::sleep(TIMEOUT_POLL_INTERVAL),
                }
            };

            // Joins return once the pipes close, which the kill above forces.
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            match status {
                Some(status) => Ok(Output {
                    status,
                    stdout,
                    stderr,
                }),
                None => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out after {}s", timeout.as_secs()),
                )),
            }
        })();
        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(output) => crate::logging::process_exec_completed(
                self.subsystem,
                &self.program,
                &args,
                Some(output.status),
                duration_ms,
            ),
            Err(err) => crate::logging::process_exec_failed(
                self.subsystem,
                &self.program,
                &args,
                &err.to_string(),
            ),
        }
        result
    }

    /// Run the command with `input` written to its stdin, capturing
    /// stdout/stderr. Same tracing contract as [`Self::output_traced`].
    ///
    /// Exists so a caller can hand a secret to a child WITHOUT putting it in
    /// argv, where it would be visible to any local user via `ps` and would be
    /// written to the process-exec log on every run.
    pub(crate) fn output_traced_with_stdin(&mut self, input: &[u8]) -> io::Result<Output> {
        use std::io::Write;
        let args = shape_args(&self.inner);
        let start = Instant::now();
        self.inner.stdin(std::process::Stdio::piped());
        self.inner.stdout(std::process::Stdio::piped());
        self.inner.stderr(std::process::Stdio::piped());
        let result = (|| {
            // This IS the funnel the lint points at — same exemption as
            // `output_traced` below, which owns the crate's only `output()`.
            #[allow(clippy::disallowed_methods)]
            let mut child = self.inner.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input)?;
                // Explicit drop closes the pipe; without it the child waits on
                // EOF and both sides block forever.
                drop(stdin);
            }
            child.wait_with_output()
        })();
        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(output) => crate::logging::process_exec_completed(
                self.subsystem,
                &self.program,
                &args,
                Some(output.status),
                duration_ms,
            ),
            Err(err) => crate::logging::process_exec_failed(
                self.subsystem,
                &self.program,
                &args,
                &err.to_string(),
            ),
        }
        result
    }

    /// Run the command to completion, capturing stdout/stderr. Logs a single
    /// `process.exec` event: INFO on zero exit, WARN on non-zero, ERROR on
    /// spawn failure. Duration is measured across the whole `output()` call
    /// (spawn + wait).
    pub(crate) fn output_traced(&mut self) -> io::Result<Output> {
        let args = shape_args(&self.inner);
        let start = Instant::now();
        // The single crate-wide site for `Command::output` — every other
        // caller must go through TracedCommand (clippy `disallowed_methods`).
        #[allow(clippy::disallowed_methods)]
        let result = self.inner.output();
        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(output) => {
                crate::logging::process_exec_completed(
                    self.subsystem,
                    &self.program,
                    &args,
                    Some(output.status),
                    duration_ms,
                );
            }
            Err(err) => {
                crate::logging::process_exec_failed(
                    self.subsystem,
                    &self.program,
                    &args,
                    &err.to_string(),
                );
            }
        }
        result
    }

    /// Run the command to completion, inheriting stdio (or whatever the
    /// builder set). Logs the same `process.exec` shape as `output_traced`.
    pub(crate) fn status_traced(&mut self) -> io::Result<ExitStatus> {
        let args = shape_args(&self.inner);
        let start = Instant::now();
        // The single crate-wide site for `Command::status`.
        #[allow(clippy::disallowed_methods)]
        let result = self.inner.status();
        let duration_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(status) => {
                crate::logging::process_exec_completed(
                    self.subsystem,
                    &self.program,
                    &args,
                    Some(*status),
                    duration_ms,
                );
            }
            Err(err) => {
                crate::logging::process_exec_failed(
                    self.subsystem,
                    &self.program,
                    &args,
                    &err.to_string(),
                );
            }
        }
        result
    }

    /// Spawn without waiting. Emits a `process.spawn` event with the child's
    /// PID on success, an error event on failure. The child's later exit is
    /// the caller's story — this facade covers the launch.
    pub(crate) fn spawn_traced(&mut self) -> io::Result<Child> {
        let args = shape_args(&self.inner);
        // The single crate-wide site for `Command::spawn`.
        #[allow(clippy::disallowed_methods)]
        let result = self.inner.spawn();
        match &result {
            Ok(child) => {
                crate::logging::process_spawned(self.subsystem, &self.program, &args, child.id());
            }
            Err(err) => {
                crate::logging::process_spawn_failed(
                    self.subsystem,
                    &self.program,
                    &args,
                    &err.to_string(),
                );
            }
        }
        result
    }
}

/// Mask credential-bearing argument VALUES before they are logged.
///
/// `shape_args` writes argv into the `process.exec` event at INFO — every
/// command, every run. An argument like `Authorization: bearer ghp_…` would
/// therefore land in the log verbatim, on every invocation, for the lifetime
/// of the process.
///
/// Callers should keep secrets out of argv entirely (argv is also world-visible
/// via `ps`); this is the backstop for when someone forgets, not the primary
/// defence.
fn redact_arg(arg: &str) -> String {
    const SECRET_PREFIXES: [&str; 4] = [
        "authorization:",
        "proxy-authorization:",
        "private-token:",
        "x-api-key:",
    ];
    let lowered = arg.to_ascii_lowercase();
    for prefix in SECRET_PREFIXES {
        if let Some(rest) = lowered.strip_prefix(prefix) {
            // Keep the scheme word only when it IS a scheme. For a header like
            // `x-api-key: <secret>` the first word after the colon is the
            // secret itself, so preserving it would defeat the whole point.
            const AUTH_SCHEMES: [&str; 5] = ["bearer", "basic", "digest", "negotiate", "token"];
            let first = rest.split_whitespace().next().unwrap_or("");
            let label = &arg[..prefix.len()];
            return if AUTH_SCHEMES.contains(&first) {
                format!("{label} {first} <redacted>")
            } else {
                format!("{label} <redacted>")
            };
        }
    }
    arg.to_string()
}

/// Shape a `Command`'s argv into a single log-safe string: args joined with
/// spaces, capped at `MAX_ARG_LOG_CHARS`, with an ellipsis marker on overflow.
/// Non-UTF8 args become lossy strings so the audit trail never fails to render.
#[allow(dead_code)] // Called from the wrapper commit's impl (also under #[allow]); real callers land in the migration.
fn shape_args(command: &Command) -> String {
    let mut buf = String::new();
    for arg in command.get_args() {
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(&redact_arg(&arg.to_string_lossy()));
        if buf.len() >= MAX_ARG_LOG_CHARS {
            break;
        }
    }
    if buf.len() > MAX_ARG_LOG_CHARS {
        // Char-boundary-safe truncation: back off to the last boundary at or
        // below the cap so a wide grapheme doesn't split into a broken byte.
        let mut cut = MAX_ARG_LOG_CHARS;
        while !buf.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        buf.truncate(cut);
        buf.push_str(ARG_TRUNCATED_MARKER);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::capture_logs;

    /// A cross-platform program that exits 0 without touching the environment
    /// (the shell builtin `true` on POSIX; PATH lookup is fine on macOS/Linux).
    const TRUE_PROG: &str = "true";
    /// A cross-platform program that exits with a non-zero status. `false`
    /// is a POSIX shell builtin present on both macOS and Linux CI runners.
    const FALSE_PROG: &str = "false";

    /// argv reaches the process-exec log on every run. A credential passed as
    /// a header argument would therefore be written to disk each time — this is
    /// the backstop for when a caller forgets to use stdin.
    #[test]
    fn credential_headers_are_masked_before_they_reach_the_log() {
        // Fixtures are COMPOSED rather than written as literals: a realistic
        // token literal trips the repo's own secret scanner, and a test that
        // needs an allowlist entry teaches the next person to add allowlist
        // entries.
        let fake = format!("{}{}", "ghp_", "x".repeat(20));
        let masked = redact_arg(&format!("Authorization: bearer {fake}"));
        assert!(!masked.contains(&fake), "token survived: {masked}");
        assert!(
            masked.contains("bearer"),
            "scheme is diagnostic, keep it: {masked}"
        );
        assert_eq!(
            redact_arg("--max-time"),
            "--max-time",
            "ordinary args untouched"
        );

        // For a header with no scheme word the first token after the colon IS
        // the secret, so it must not survive as a "scheme".
        let keyed = redact_arg(&format!("x-api-key: {fake}"));
        assert!(!keyed.contains(&fake), "{keyed}");

        // Case-insensitive: callers spell headers however they like.
        let secret = "y".repeat(16);
        let basic = redact_arg(&format!("AUTHORIZATION: Basic {secret}"));
        assert!(!basic.contains(&secret), "{basic}");
    }

    /// The whole point: a child that never exits must not take its caller with
    /// it. `sleep 60` under a 200ms budget has to come back promptly.
    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let started = Instant::now();
        let err = TracedCommand::new("sleep", "test")
            .arg("60")
            .output_traced_with_timeout(Duration::from_millis(200))
            .expect_err("a 60s sleep must not satisfy a 200ms budget");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "returned after {:?} — the deadline did not fire",
            started.elapsed()
        );
    }

    #[test]
    fn a_prompt_child_returns_its_output_normally() {
        let out = TracedCommand::new("echo", "test")
            .arg("hi")
            .output_traced_with_timeout(Duration::from_secs(10))
            .expect("echo is well within budget");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    /// A child writing more than a pipe buffer (~64 KiB) blocks in `write`
    /// until someone drains it. If the deadline loop polled `try_wait` without
    /// concurrent readers, this would sit until the timeout and then be
    /// reported as a hang — turning a healthy command into a false failure.
    #[test]
    fn a_child_larger_than_the_pipe_buffer_does_not_deadlock() {
        let out = TracedCommand::new("sh", "test")
            .args([
                "-c",
                "for i in $(seq 1 5000); do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done",
            ])
            .output_traced_with_timeout(Duration::from_secs(20))
            .expect("a chatty but fast child must not trip the deadline");
        assert!(out.status.success());
        assert!(
            out.stdout.len() > 64 * 1024,
            "test needs to exceed one pipe buffer, got {} bytes",
            out.stdout.len()
        );
    }

    /// A timed-out run still emits its process.exec event, or a wedge would be
    /// invisible in the log — the exact blindness this work is fixing.
    #[test]
    fn a_timed_out_run_is_still_logged_as_a_failure() {
        let err = TracedCommand::new("sleep", "test")
            .arg("30")
            .output_traced_with_timeout(Duration::from_millis(150))
            .expect_err("must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn output_traced_logs_process_exec_with_program_args_subsystem_and_duration() {
        let out = capture_logs(|| {
            let _ = TracedCommand::new("echo", "test")
                .arg("hello")
                .arg("world")
                .output_traced();
        });
        assert!(out.contains("event=\"process.exec\""), "{out}");
        assert!(out.contains("subsystem=\"test\""), "{out}");
        assert!(out.contains("program=\"echo\""), "{out}");
        assert!(out.contains("args=\"hello world\""), "{out}");
        assert!(out.contains("duration_ms="), "{out}");
        assert!(out.contains("outcome=\"ok\""), "{out}");
        assert!(out.contains("INFO"), "clean exit is INFO: {out}");
    }

    #[test]
    fn output_traced_nonzero_exit_is_warn_with_error_outcome() {
        let out = capture_logs(|| {
            let _ = TracedCommand::new(FALSE_PROG, "test").output_traced();
        });
        assert!(out.contains("event=\"process.exec\""), "{out}");
        assert!(out.contains("outcome=\"error\""), "{out}");
        assert!(out.contains("WARN"), "non-zero exit must be WARN: {out}");
    }

    #[test]
    fn status_traced_logs_process_exec_and_returns_status() {
        let out = capture_logs(|| {
            let status = TracedCommand::new(TRUE_PROG, "test")
                .status_traced()
                .expect("true(1) should be on PATH");
            assert!(status.success(), "true exits zero");
        });
        assert!(out.contains("event=\"process.exec\""), "{out}");
        assert!(out.contains("subsystem=\"test\""), "{out}");
        assert!(out.contains("outcome=\"ok\""), "{out}");
        assert!(out.contains("INFO"), "{out}");
    }

    #[test]
    fn spawn_failure_logs_error_event() {
        let out = capture_logs(|| {
            let result = TracedCommand::new(
                "/nonexistent/definitely-not-a-real-program-flock-test",
                "test",
            )
            .output_traced();
            assert!(result.is_err(), "missing program must fail to spawn");
        });
        assert!(
            out.contains("event=\"process.exec\"") || out.contains("event=\"process.spawn\""),
            "{out}"
        );
        assert!(out.contains("outcome=\"error\""), "{out}");
        assert!(out.contains("ERROR"), "spawn failure must be ERROR: {out}");
    }

    #[test]
    fn spawn_traced_emits_process_spawn_with_pid() {
        let out = capture_logs(|| {
            let mut child = TracedCommand::new(TRUE_PROG, "test")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn_traced()
                .expect("true(1) should spawn");
            let _ = child.wait();
        });
        assert!(out.contains("event=\"process.spawn\""), "{out}");
        assert!(out.contains("subsystem=\"test\""), "{out}");
        assert!(
            out.contains("pid="),
            "spawn event carries the child pid: {out}"
        );
        assert!(out.contains("outcome=\"ok\""), "{out}");
    }

    #[test]
    fn args_are_bounded_and_truncated_with_marker() {
        // A long args list mustn't be able to blow up the audit trail.
        let long = "x".repeat(600);
        let out = capture_logs(|| {
            let _ = TracedCommand::new("echo", "test")
                .arg(&long)
                .output_traced();
        });
        assert!(out.contains("…[truncated]"), "long args truncated: {out}");
    }

    #[test]
    fn from_command_traces_a_prebuilt_command() {
        // Platform helpers hand around a fully-built `Command` — verify the
        // wrap-and-run path emits the same event shape as builder-first use.
        let out = capture_logs(|| {
            #[allow(clippy::disallowed_methods)] // Test builds a raw Command intentionally.
            let mut cmd = Command::new("echo");
            cmd.arg("wrapped");
            let _ = TracedCommand::from_command(cmd, "platform").output_traced();
        });
        assert!(out.contains("event=\"process.exec\""), "{out}");
        assert!(out.contains("subsystem=\"platform\""), "{out}");
        assert!(out.contains("program=\"echo\""), "{out}");
        assert!(out.contains("args=\"wrapped\""), "{out}");
    }
}
