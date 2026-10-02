//! E2E (#427): `flk report bug --body-only` puts the issue body on stdout and
//! nothing else, so the report survives the pipe it was always missing.
//!
//! Driven through the compiled binary, because the thing being protected is a
//! STREAM CONTRACT rather than a function: `compose` returning a body proves
//! only that some string exists. What a future refactor breaks silently is the
//! split — the day a header, a URL or the "nothing has been sent" footer goes
//! back onto stdout, `--body-only` still exits 0, still prints something that
//! reads like a report, and now poisons whatever a person pipes it into. A unit
//! test of `Composed` could not see that, so these assert on the two streams
//! separately.
//!
//! The preview is asserted on stderr for the same reason from the other side:
//! ADR-0010 decision 5 requires the composed block to be previewed before
//! anything can be sent, and on this route that promise is only kept if the
//! preview moves off stdout with the body. A test that accepted stdout-only
//! would let it go quiet without anyone noticing.

// Integration tests drive the compiled binary through raw Command; the
// TracedCommand funnel polices flock's own subprocesses, not the harness's.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// A filled-in `flk report template bug`, written by hand rather than by the
/// command that emits it — a fixture that depends on the code under test can
/// agree with a regression instead of catching it.
const BUG_REPORT: &str = "\
## current-behavior
the pane never returns
## expected-behavior
it returns the recent buffer
## reproduction
1. run flk
2. open an agent pane
3. read the pane with --source recent
it never comes back
## impact
an agent cannot read its own pane
";

/// A value that cannot occur incidentally anywhere in the harness, so finding
/// it proves the log tail travelled rather than that a substring matched.
const LOG_MARKER: &str = "zzdiagnosticzz";

/// A private root for one test.
///
/// Under `/tmp` rather than `std::env::temp_dir()` because this harness hands
/// the child a socket path: a macOS `TMPDIR` is a `/var/folders/…` path long
/// enough to run past `SUN_LEN` before a single byte is written, and the
/// failure that produces is a socket error rather than anything about reports.
/// The socket here is never bound — it only has to be a path that can be
/// *named*, so the error is what keeps the length honest.
fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/flock-report-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

/// `flock`/`flock-dev` under the profile that built this test binary, mirroring
/// `app_dir_name()` — the log tail is read out of the config dir, so a test
/// that guessed the wrong one would silently assert that no diagnostics exist.
fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}

struct Sandbox {
    base: PathBuf,
    home: PathBuf,
    config_home: PathBuf,
    bug_file: PathBuf,
    socket: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let base = unique_dir(tag);
        let home = base.join("home");
        let config_home = base.join("config");
        fs::create_dir_all(&home).expect("sandbox home");
        fs::create_dir_all(&config_home).expect("sandbox config home");
        let bug_file = base.join("bug.md");
        fs::write(&bug_file, BUG_REPORT).expect("write the filled-in form");
        let socket = base.join("absent.sock");
        Self {
            base,
            home,
            config_home,
            bug_file,
            socket,
        }
    }

    /// One WARN record, in the shape `report::redact` reads. `program` is
    /// allowlisted, so this is exactly the path a real record takes — the test
    /// does not need the logging spine to have run.
    fn seed_a_log_record(&self) {
        let dir = self.config_home.join(app_dir_name());
        fs::create_dir_all(&dir).expect("log dir");
        fs::write(
            dir.join("flock-server.log"),
            format!(
                "{{\"timestamp\":\"2026-09-29T11:06:13.873997Z\",\"level\":\"WARN\",\
                 \"message\":\"process exec exited non-zero\",\"event\":\"process.exec\",\
                 \"subsystem\":\"peers\",\"outcome\":\"error\",\"program\":\"{LOG_MARKER}\",\
                 \"target\":\"flk::logging\"}}\n"
            ),
        )
        .expect("seed log");
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("FLOCK_SOCKET_PATH", &self.socket)
            .env_remove("FLOCK_CLIENT_SOCKET_PATH")
            .env_remove("FLOCK_ENV")
            .env_remove("FLOCK_PANE_ID")
            .output()
            .expect("run flk")
    }

    fn report(&self, extra: &[&str]) -> std::process::Output {
        let mut args = vec![
            "report",
            "bug",
            "--file",
            self.bug_file.to_str().expect("utf-8 path"),
            "--repo",
            "gerchowl/flock",
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn body_only_puts_the_body_on_stdout_and_nothing_else() {
    let sandbox = Sandbox::new("streams");
    let output = sandbox.report(&["--body-only", "--no-diagnostics"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );

    let body = stdout_of(&output);

    // The report itself, whole and unabridged — the fields the reporter wrote
    // and the environment block flock filled in without being asked.
    assert!(body.starts_with("## current-behavior\n"), "{body}");
    for section in [
        "## current-behavior",
        "## expected-behavior",
        "## reproduction",
        "## impact",
        "## environment",
    ] {
        assert!(
            body.contains(section),
            "{section} missing from body:\n{body}"
        );
    }
    assert!(body.ends_with('\n'), "a body written to a file wants one");

    // And none of the preview's furniture, which is the entire point of the
    // flag. Each of these was the thing that made `--print` need an `awk`
    // script before `gh issue create` would take it.
    for furniture in [
        "destination:",
        "kind:",
        "github.com",
        "issues/new",
        "nothing has been sent",
        "## diagnostics",
    ] {
        assert!(
            !body.contains(furniture),
            "{furniture:?} leaked into the body:\n{body}"
        );
    }
}

#[test]
fn the_preview_and_the_next_command_land_on_stderr() {
    let sandbox = Sandbox::new("stderr");
    let output = sandbox.report(&["--body-only", "--no-diagnostics"]);
    let stderr = stderr_of(&output);

    // ADR-0010 decision 5: the composed block is previewed before anything can
    // be sent. On this route "previewed" means "on stderr", because the body
    // owns stdout and a pipe can only carry one of them.
    assert!(
        stderr.contains("destination: gerchowl/flock"),
        "the preview must still name the destination:\n{stderr}"
    );
    assert!(
        stderr.contains("## reproduction"),
        "the preview must still show the composed sections:\n{stderr}"
    );
    assert!(
        stderr.contains("nothing has been sent"),
        "the route that sends nothing must say so:\n{stderr}"
    );

    // The hand-off the issue asked for: one exact command, so the moment of
    // context is freshest is not also the moment the reporter has to
    // reconstruct what they meant to type.
    assert!(
        stderr.contains("gh issue create --repo gerchowl/flock --title '<one line>' --body-file -"),
        "the ready-to-run command is the deliverable:\n{stderr}"
    );
}

#[test]
fn diagnostics_ride_inline_rather_than_going_to_a_clipboard() {
    let sandbox = Sandbox::new("diagnostics");
    sandbox.seed_a_log_record();
    let output = sandbox.report(&["--body-only"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );

    // The URL is capped at 7 500 characters and the body is not, so on this
    // route the log tail belongs in the body — a headless report carries
    // strictly more evidence than the browser route can, not less.
    assert!(
        stdout_of(&output).contains(LOG_MARKER),
        "the log tail must reach the body:\n{}",
        stdout_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("the redacted log tail is in the body"),
        "putting logs in the body has to be said out loud:\n{stderr}"
    );
    assert!(
        !stderr.contains("clipboard"),
        "nothing is copied anywhere when stdout is the destination:\n{stderr}"
    );
}

#[test]
fn body_only_and_open_are_refused_together() {
    let sandbox = Sandbox::new("conflict");
    let output = sandbox.report(&["--body-only", "--open"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        stderr_of(&output)
    );
    let stderr = stderr_of(&output);
    assert!(stderr.contains("--body-only"), "{stderr}");
    assert!(stderr.contains("--open"), "{stderr}");
    // A browser must not have been launched behind a body someone piped
    // somewhere, so there is no preview to check for and no body to leak.
    assert!(stdout_of(&output).is_empty(), "{}", stdout_of(&output));
}

#[test]
fn help_documents_the_flag() {
    let sandbox = Sandbox::new("help");
    // Both spellings of the question, because #365 fixed `flk report bug
    // --help` specifically and the group-level form answers from the same
    // place.
    for args in [vec!["report", "bug", "--help"], vec!["report", "--help"]] {
        let output = sandbox.run(&args);
        assert_eq!(output.status.code(), Some(0), "flk {}", args.join(" "));
        let text = format!("{}{}", stdout_of(&output), stderr_of(&output));
        assert!(
            text.contains("--body-only"),
            "flk {} does not document --body-only:\n{text}",
            args.join(" ")
        );
    }
}

/// The flag is only useful if it survives the shell, so it goes through the
/// template the issue's own reproduction describes.
#[test]
fn a_body_pipes_straight_into_gh_without_hand_surgery() {
    let sandbox = Sandbox::new("pipe");
    let output = sandbox.report(&["--body-only", "--no-diagnostics"]);
    let body = stdout_of(&output);

    // What `gh issue create --body-file -` does with it: a byte-for-byte
    // document. Asserted as the absence of the shapes that needed stripping
    // before, because "we printed a report" is true of the broken version too.
    assert!(!body.contains("\r"), "a body must not carry CR");
    assert!(!body.trim().is_empty(), "an empty body is not a report");
    assert_eq!(
        body.matches("## ").count(),
        5,
        "five sections, no header line that would read as a sixth:\n{body}"
    );
    assert!(Path::new(&sandbox.bug_file).exists(), "fixture untouched");
}
