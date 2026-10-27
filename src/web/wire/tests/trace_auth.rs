//! Every authored step through the actual endpoint and the real client.
use super::*;
#[test]
fn auth_script_executes_every_step_and_accounts_for_pending_scripts() {
    runtime(async {
        let server = Server::new().await;
        let dropped = std::env::var("S7_TRACE_DROP_STEP").unwrap_or_default(); // guardrails-ok(hermetic): explicit test-only coverage fail-first control
        let output = within(
            tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-auth.ts"
                ))
                .arg(format!("ws://{}/v1/ws", server.bind))
                .arg(dropped)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").expect("fixture tools"))
                .env("HOME", server._root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let proof: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(proof.as_object().unwrap().len(), 1);
        let lines = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../web/client/test/fixtures/wire/token-refused.jsonl"
        ))
        .unwrap();
        let facts = proof["token-refused"].as_array().unwrap();
        assert_eq!(facts.len(), lines.len());
        for (index, (fact, line)) in facts.iter().zip(lines).enumerate() {
            assert_eq!(fact["index"], index);
            assert_eq!(fact["checked"], true);
            assert_eq!(fact["step"], serde_json::to_value(line).unwrap()["step"]);
        }
        server.finish().await;
    });
}
