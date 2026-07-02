//! Integration coverage for remote-launch observability (logging redesign PR-1).
//!
//! The origin story: a failed remote connect left NOTHING in the logs at any
//! FLOCK_LOG level — the ssh probes, the resolved remote binary path, and the
//! bridge command were all invisible. These tests drive `flk --remote` against
//! a stub `ssh` on PATH and assert the launcher's flock.log now tells the story
//! at the DEFAULT level.

#![cfg(unix)]
// This harness drives the compiled flk binary directly (Command::new(CARGO_BIN_EXE_flk))
// — TracedCommand (logging redesign PR-3) is a source-code lint, not a test-scaffolding one.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/flock-remote-log-test-{}-{nanos}",
        std::process::id()
    ))
}

/// A stub `ssh` that answers the launcher's preflight probes like a real host
/// with NO flk installed: uname succeeds (linux/x86_64), `command -v flk`
/// fails, every `test -x` fails. Argv of every invocation is appended to
/// `ssh-calls.log` for debugging.
const STUB_SSH: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
printf '%s\n' "$*" >> "$dir/ssh-calls.log"
case "$*" in
  *"/bin/sh -s"*)
    script="$(cat)"
    case "$script" in
      *"uname -s"*)
        printf 'Linux\nx86_64\n'
        exit 0
        ;;
      *)
        # binary probes / installs: nothing there
        exit 1
        ;;
    esac
    ;;
  *"command -v flk"*)
    exit 1
    ;;
esac
exit 1
"#;

fn write_stub_ssh(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("ssh");
    fs::write(&path, STUB_SSH).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for_log_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut content = String::new();
    while Instant::now() < deadline {
        content = fs::read_to_string(path).unwrap_or_default();
        if content.contains(needle) {
            return content;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "log at {} never contained {needle:?}; content:\n{content}",
        path.display()
    );
}

#[test]
fn failed_remote_connect_story_is_in_the_log_at_default_level() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let home = base.join("home");
    let stub_dir = base.join("stub");
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&home).unwrap();
    write_stub_ssh(&stub_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let status = Command::new(env!("CARGO_BIN_EXE_flk"))
        .arg("--remote")
        .arg("stub-target")
        .env("PATH", format!("{}:{orig_path}", stub_dir.display()))
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env_remove("FLOCK_LOG") // the point: DEFAULT level must carry the story
        .env_remove("FLOCK_REMOTE_BINARY")
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null()) // install prompt reads EOF → declines
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run flk --remote");

    assert!(
        !status.success(),
        "stub host has no flk and install is declined — launch must fail"
    );

    let log = config_home.join(app_dir_name()).join("flock.log");
    // The platform probe answered linux/x86_64 — that MUST be in the log.
    let content = wait_for_log_contains(
        &log,
        "\"event\":\"remote.probe.result\"",
        Duration::from_secs(5),
    );
    assert!(content.contains("\"os\":\"linux\""), "{content}");
    assert!(content.contains("\"arch\":\"x86_64\""), "{content}");
    assert!(
        content.contains("\"target\":\"stub-target\""),
        "every remote event carries the target: {content}"
    );
    // And the story must include at least one remote.* non-ok outcome telling
    // us WHY the launch went nowhere (declined install / failed probe).
    assert!(
        content.contains("\"subsystem\":\"remote\""),
        "remote events must be attributed: {content}"
    );

    let ssh_calls = fs::read_to_string(stub_dir.join("ssh-calls.log")).unwrap_or_default();
    assert!(
        ssh_calls.contains("stub-target"),
        "stub ssh should have been invoked: {ssh_calls}"
    );

    let _ = fs::remove_dir_all(&base);
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "flock-dev"
    } else {
        "flock"
    }
}
