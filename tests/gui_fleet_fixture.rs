//! Invalid script and model input must fail promptly with useful diagnostics.
mod support;

#[test]
fn scripted_model_resume_continues_after_the_completed_reply() {
    use serde_json::json;
    let root = support::socket_dir::SocketDir::reserve("gfr");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gui-fleet/claude.py");
    assert!(fixture.is_file(), "GUI fleet fake model is missing");
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 fixture interpreter");
    let session = "00000000-0000-4000-8000-000000003093";
    let script = json!([
        {"expect":"message","text":"first"}, {"emit":"reply","text":"completed"},
        {"expect":"message","text":"second"}, {"emit":"reply","text":"continued"}
    ]);
    std::fs::write(root.path().join("gui-fleet.json"), script.to_string()).unwrap();
    for (flag, text, reply, code) in [
        ("--session-id", "first", "completed", 1),
        ("--resume", "second", "continued", 0),
    ] {
        let mut command = support::environment::Command::new(&python);
        command
            .arg("-I")
            .arg(&fixture)
            .args([flag, session])
            .env_clear()
            .env("HOME", root.path())
            .env("CLAUDE_CONFIG_DIR", root.path());
        let input = json!({"type":"user","uuid":session,"message":{"role":"user","content":[{"type":"text","text":text}]}});
        let output = support::command::output(
            command,
            Some(format!("{input}\n").into_bytes()),
            support::command::CLI_LIMIT,
        )
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if code == 1 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected EOF"));
        }
        let records: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(records.iter().all(|record| record["session_id"] == session));
        assert_eq!(records.last().unwrap()["result"], reply);
    }
}

#[test]
fn scripted_model_rejects_malformed_input_and_unknown_steps() {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gui-fleet/claude.py");
    assert!(fixture.is_file(), "GUI fleet fake model is missing");
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 fixture interpreter");
    for (script, input, diagnostic) in [
        (r#"[{"emit":"unknown"}]"#, "", "unknown script step"),
        ("{}", "", "script must be a list"),
        ("[", "", "invalid JSON"),
        (
            r#"[{"expect":"message","text":"hello"}]"#,
            "bad json\n",
            "invalid JSON",
        ),
        (
            r#"[{"expect":"message","text":"hello"}]"#,
            "{}\n",
            "expected user message",
        ),
        (
            r#"[{"expect":"message","text":"hello"}]"#,
            "",
            "unexpected EOF",
        ),
        (r#"[{"emit":"stream","chunks":[1]}]"#, "", "stream chunks"),
        (
            r#"[{"expect":"decision","behavior":"maybe"}]"#,
            "",
            "decision behavior",
        ),
    ] {
        let root = support::socket_dir::SocketDir::reserve("gff");
        std::fs::write(root.path().join("gui-fleet.json"), script).unwrap();
        let mut command = support::environment::Command::new(&python);
        command
            .args(["-I"])
            .arg(&fixture)
            .args(["--session-id", "00000000-0000-4000-8000-000000003093"])
            .env_clear()
            .env("HOME", root.path())
            .env("CLAUDE_CONFIG_DIR", root.path());
        let output = support::command::output(
            command,
            Some(input.as_bytes().to_vec()),
            support::command::CLI_LIMIT,
        )
        .expect("malformed input must fail within the ceiling");
        assert_eq!(output.status.code(), Some(1), "{script}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
