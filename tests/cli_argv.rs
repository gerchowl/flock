//! Invalid native argv must be a usage refusal, never a Rust panic.
#![allow(clippy::disallowed_methods)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

#[test]
fn non_utf8_argument_is_a_typed_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .env_remove("FLOCK_SOCKET_PATH")
        .env_remove("FLOCK_PANE_ID")
        .env_remove("FLOCK_AGENT_ID")
        .arg("--session")
        .arg(OsString::from_vec(b"invalid-\xff".to_vec()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("invalid_params"), "{stderr}");
    assert!(stderr.contains("argument 2"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn non_utf8_program_name_is_a_typed_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .env_remove("FLOCK_SOCKET_PATH")
        .env_remove("FLOCK_PANE_ID")
        .env_remove("FLOCK_AGENT_ID")
        .arg0(OsString::from_vec(b"flk-\xff".to_vec()))
        .arg("--version")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("invalid_params: argument 0"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn handoff_import_after_global_session_flags_reaches_the_importer() {
    for flags in [
        vec!["--session", "handoff-fixture"],
        vec!["--session=handoff-fixture"],
    ] {
        for socket in [
            OsString::from("socket"),
            OsString::from_vec(b"socket-\xff".to_vec()),
        ] {
            let sandbox = std::env::temp_dir().join(format!("flk-2407-{}", std::process::id()));
            std::fs::create_dir(&sandbox).unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_flk"))
                .env_clear()
                .env("HOME", &sandbox)
                .env("XDG_CONFIG_HOME", &sandbox)
                .args(&flags)
                .args(["server", "--handoff-import"])
                .arg(&socket)
                .output()
                .unwrap();
            std::fs::remove_dir_all(&sandbox).unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                output.status.code(),
                Some(1),
                "{flags:?} {socket:?}: {stderr}"
            );
            assert!(
                stderr.contains("missing handoff token"),
                "{flags:?} {socket:?}: {stderr}"
            );
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}
