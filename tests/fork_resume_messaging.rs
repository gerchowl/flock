//! Real server and fake harness acceptance for inbox identity after fork/resume.

// This harness owns its subprocesses; TracedCommand polices production code.
#![allow(clippy::disallowed_methods)]

fn probe(mode: &str) {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/probes/fork_resume_messaging.py"
        ))
        .args(["--bin", env!("CARGO_BIN_EXE_flk"), "--mode", mode])
        .output()
        .expect("run bounded isolated messaging probe");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fork_uses_attested_inbox_despite_inherited_pane_claim() {
    probe("fork");
}

#[test]
fn resume_uses_attested_inbox_despite_inherited_pane_claim() {
    probe("resume");
}
