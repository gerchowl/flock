//! The complete protocol-fault trace, with a real authenticated socket and close.
use super::*;
#[test]
fn protocol_script_checks_welcome_fault_provenance_and_actual_close() {
    runtime(async {
        let server = Server::with_setup(
            |clock| Arc::new(clock),
            |core| {
                Arc::get_mut(&mut core.shared).unwrap().epoch =
                    "00000000000000000000000000000001".into()
            },
        )
        .await;
        let path = server._root.path().join("client-tokens/one.token");
        let mut token: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        token["principal"] = json!("0123456789abcdef0123456789abcdef");
        fs::write(path, token.to_string()).unwrap();
        let control = std::env::var("S7_PROTOCOL_CONTROL").unwrap_or_default(); // guardrails-ok(hermetic): explicit test-only causal control
        let output = within(
            tokio::process::Command::new("bun")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/web/client/tests/wire/endpoint-protocol.ts"
                ))
                .arg(format!("ws://{}/v1/ws", server.bind))
                .arg(control)
                .arg(env!("CARGO_PKG_VERSION"))
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", server._root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap();
        let facts: Vec<Value> = if output.status.success() {
            serde_json::from_slice(&output.stdout).unwrap()
        } else {
            server.finish().await;
            panic!("{}", String::from_utf8_lossy(&output.stderr));
        };
        let lines = crate::structured_wire::fixture::FixtureLine::parse_script(include_str!(
            "../../../../web/client/test/fixtures/wire/unknown-subscription.jsonl"
        ))
        .unwrap();
        assert_eq!(facts.len(), lines.len());
        for (index, (fact, line)) in facts.iter().zip(lines).enumerate() {
            assert_eq!(fact["index"], index);
            assert_eq!(fact["checked"], true);
            assert_eq!(fact["step"], serde_json::to_value(line).unwrap()["step"]);
            assert_eq!(
                fact["source"],
                match index {
                    0 => "setup",
                    1 | 3 => "client",
                    5 => "server",
                    6 => "client_fault",
                    10 => "clock",
                    _ => "observation",
                }
            );
        }
        server.finish().await;
    });
}
