//! Name refusals through the real CLI, with no live server or operator state.
mod support;

#[tokio::test]
async fn security_agent_name_cli_rejects_before_connecting() {
    let root = support::socket_dir::SocketDir::reserve("name317");
    let names = [
        ("csi", "agent\u{1b}[2J".to_owned()),
        ("osc", "agent\u{1b}]52;c;Zml4dHVyZQ==\u{7}".to_owned()),
        ("newline", "agent\nnext".to_owned()),
        ("c1", "agent\u{9b}2J".to_owned()),
        ("bidi", "agent\u{202e}other".to_owned()),
        ("overlong", "界".repeat(65)),
    ];
    for verb in ["rename", "start"] {
        for (case, name) in &names {
            let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
            command
                .env_clear()
                .envs(support::environment::isolated_env(root.path(), root.path()));
            command.args(["--format", "json", "agent", verb]);
            if verb == "rename" {
                command.arg("fixture-target");
            }
            command.arg(name);
            if verb == "start" {
                command.args(["--", "fixture-program"]);
            }
            let mut command = tokio::process::Command::from(command.into_std());
            command.kill_on_drop(true);
            let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
                .await
                .expect("CLI must finish without a server")
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{verb}: {case}");
            let response: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("typed JSON refusal");
            assert_eq!(
                response["error"]["code"], "invalid_agent_name",
                "{verb}: {case}"
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains(name),
                "name leaked: {case}"
            );
        }
    }
}
