//! S10f public TUI acceptance using only an isolated, scripted model producer.
#[allow(clippy::disallowed_methods)] // Owns a bounded Python driver and its isolated process tree.
fn acceptance(permission: bool) {
    let python = std::env::split_paths(&std::env::var_os("PATH").expect("fixture PATH"))
        .map(|p| p.join("python3"))
        .find(|p| p.is_file())
        .expect("python3 for the owned TUI driver");
    let mut command = std::process::Command::new(python);
    command
        .args([
            "-I",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/probes/structured_tui.py"
            ),
            "--flk",
            env!("CARGO_BIN_EXE_flk"),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("fixture PATH"));
    if permission {
        command.arg("--permission");
    }
    let output = command.output().expect("bounded owned TUI fixture");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["status"], "passed");
    for field in [
        "real_tui",
        "composer",
        "shell_cli",
        "pipes_not_pty",
        "descendants_gone",
        "temporary_removed",
        "mcp_ignored_eof",
    ] {
        assert_eq!(proof[field], true, "{field}");
    }
    assert_eq!(proof["permission_allow"], permission);
    assert_eq!(proof["permission_deny"], permission);
}
#[test]
fn structured_mvp_public_tui_composer_shell_and_stop() {
    acceptance(false);
}
#[test]
fn structured_mvp_public_tui_permission_decision() {
    acceptance(true);
}
