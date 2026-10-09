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

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use support::environment::Command;

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

/// Credential-shaped for `secret_token_re`, which is the whole point: a
/// placeholder like `ghp_TEST` is too short to match, so it would prove the
/// masking rules nothing.
const TOKEN: &str = "ghp_ABCdef0123456789XYZabc";

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
    ///
    /// `kind` and `message` are the two fields that can carry free text out
    /// (`ALLOWED_FIELDS` / `SCRUBBED_FIELDS`), so the secrets in
    /// [`seed_a_hostile_record`] travel exactly as far as a real one would.
    fn write_record(&self, record: &str) {
        let dir = self.config_home.join(app_dir_name());
        fs::create_dir_all(&dir).expect("log dir");
        fs::write(dir.join("flock-server.log"), format!("{record}\n")).expect("seed log");
    }

    fn seed_a_log_record(&self) {
        self.write_record(&format!(
            "{{\"timestamp\":\"2026-09-29T11:06:13.873997Z\",\"level\":\"WARN\",\
             \"message\":\"process exec exited non-zero\",\"event\":\"process.exec\",\
             \"subsystem\":\"peers\",\"outcome\":\"error\",\"program\":\"{LOG_MARKER}\",\
             \"target\":\"flk::logging\"}}"
        ));
    }

    /// A record shaped like the ones #232 was filed with: the sandbox `$HOME`
    /// in an allowlisted value, and a GitHub token in the free-text fields.
    ///
    /// The token shape is the one `secret_token_re` matches on (`ghp_` plus
    /// 16+ alphanumerics) — long enough to be a real credential to the regex
    /// and to anything reading the file.
    fn seed_a_hostile_record(&self) {
        self.write_record(&format!(
            "{{\"timestamp\":\"2026-09-29T11:06:14.873997Z\",\"level\":\"ERROR\",\
             \"message\":\"remote install failed at {home} with {token}\",\
             \"event\":\"remote.install\",\"subsystem\":\"remote\",\"outcome\":\"error\",\
             \"kind\":\"{home}/Projects/private-client\",\"err\":\"auth failed: \
             github_token={token}\",\"target\":\"flk::logging\"}}",
            home = self.home.display(),
            token = TOKEN,
        ));
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

/// Single-quote a path for `sh`, escaping any quote inside it. Without this a
/// sandbox path with an apostrophe would build a script that runs something
/// other than the test — the harness testing itself, and passing.
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
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

/// The promise this route makes, asserted on the bytes a person would paste
/// into a public issue.
///
/// The other diagnostics test seeds a benign record, which proves the log tail
/// *travels* and nothing about what it is allowed to carry. This one seeds what
/// #232 was actually made of — a `$HOME` path and a GitHub token — and reads
/// the body off **stdout**, because that is the stream whose whole job is to
/// become somebody else's public issue. `redact`'s own unit tests cover the
/// scrubber; this covers the route, i.e. that a body written to stdout is still
/// run through it on the way.
#[test]
fn the_body_on_stdout_is_masked() {
    let sandbox = Sandbox::new("masked");
    sandbox.seed_a_hostile_record();

    let output = sandbox.report(&["--body-only"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );

    let body = stdout_of(&output);
    assert!(
        !body.contains(TOKEN),
        "a GitHub token reached stdout:\n{body}"
    );
    assert!(
        !body.contains(&sandbox.home.display().to_string()),
        "the $HOME path reached stdout:\n{body}"
    );
    // Masked, not deleted: a body with the failure removed from it is worse
    // than useless, so the record must still be there in redacted form.
    assert!(
        body.contains("remote.install"),
        "the record should survive redaction:\n{body}"
    );
    assert!(
        body.contains("<redacted-token>") || body.contains("github_token=<redacted>"),
        "the token should be masked, not merely absent:\n{body}"
    );
    assert!(
        body.contains('~'),
        "the home path should be shortened:\n{body}"
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

/// The two matrix rows that were hand-checked and left unpinned: a `--file`
/// that is not there, and a diagnostics count. Both are places where a failure
/// could leave something half-written on a pipe, which is the only way this
/// route can hurt anybody.
#[test]
fn a_missing_file_fails_without_writing_a_partial_body() {
    let sandbox = Sandbox::new("missing-file");
    let missing = sandbox.base.join("not-there.md");
    let output = sandbox.run(&[
        "report",
        "bug",
        "--file",
        missing.to_str().expect("utf-8 path"),
        "--repo",
        "gerchowl/flock",
        "--body-only",
    ]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stdout_of(&output).is_empty(),
        "a failed compose must not put a partial body on a pipe: {}",
        stdout_of(&output)
    );
    assert!(
        stderr_of(&output).contains("could not read"),
        "stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn the_record_count_is_the_reporter_s_and_the_shortfall_is_said_out_loud() {
    let sandbox = Sandbox::new("last");
    sandbox.seed_a_log_record();

    // One seeded WARN record against a request for 5: the shortfall note is
    // the difference between "that is all there was" and "I asked for more",
    // and on this route it has to arrive on stderr or it arrives nowhere.
    let output = sandbox.report(&["--body-only", "--last", "5"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&output)
    );

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("fewer than the 5 requested"),
        "stderr: {stderr}"
    );
    assert!(
        stdout_of(&output).contains(LOG_MARKER),
        "--last changes how much rides along, not whether it does"
    );
}

/// The flag is only useful if it survives the shell, so it goes through the
/// template the issue's own reproduction describes.
#[test]
fn a_redirect_writes_exactly_the_body_and_nothing_else() {
    let sandbox = Sandbox::new("redirect");
    let piped = sandbox.report(&["--body-only", "--no-diagnostics"]);

    // A real `>` rather than a string comparison: the report lands on disk on
    // this route in a way `--open` never does, so the file a filing command
    // reads is itself worth proving is byte-for-byte the body and not the
    // preview. `sh` is the one interpreter POSIX guarantees.
    let destination = sandbox.base.join("redirected.md");
    let script = format!(
        "{} report bug --file {} --repo gerchowl/flock --body-only --no-diagnostics > {}",
        sh_quote(&PathBuf::from(env!("CARGO_BIN_EXE_flk"))),
        sh_quote(&sandbox.bug_file),
        sh_quote(&destination),
    );
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", &sandbox.home)
        .env("XDG_CONFIG_HOME", &sandbox.config_home)
        .env("FLOCK_SOCKET_PATH", &sandbox.socket)
        .env_remove("FLOCK_CLIENT_SOCKET_PATH")
        .env_remove("FLOCK_ENV")
        .output()
        .expect("run flk through a redirect");
    assert!(
        output.status.success(),
        "flk through sh: {script}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let written = fs::read_to_string(&destination).expect("the redirect created the file");
    assert_eq!(
        written,
        stdout_of(&piped),
        "the file and stdout must be the same bytes — otherwise the preview is \
         being redirected somewhere it was not meant to go"
    );
    assert!(written.starts_with("## current-behavior\n"), "{written}");
    assert!(!written.contains("destination:"), "{written}");
    assert!(Path::new(&sandbox.bug_file).exists(), "fixture untouched");
}
