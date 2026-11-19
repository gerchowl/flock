//! Real CLI and PTY acceptance for the whole-Welcome deadline.

// The isolated Python driver owns and reaps each fake peer and client, with an 8s ceiling.
#![allow(clippy::disallowed_methods)]

#[test]
fn welcome_timeout_restores_pty_preserves_errors_and_never_replays_input() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/probes/attach_welcome.py"
        ))
        .args(["--bin", env!("CARGO_BIN_EXE_flk")])
        .output()
        .expect("run bounded isolated attach probe");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
