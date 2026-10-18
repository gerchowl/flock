//! Public structured admission through a real isolated server, CLI and MCP child.

#[test]
#[allow(clippy::disallowed_methods)] // Owns a bounded fixture script and its server tree.
fn isolated_public_structured_cli_and_mcp_dogfood() {
    let python = std::env::split_paths(&std::env::var_os("PATH").expect("fixture PATH"))
        .map(|path| path.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 is required for the structured dogfood fixture");
    let output = std::process::Command::new(python)
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/probes/structured_public.py"
        ))
        .args(["--flk", env!("CARGO_BIN_EXE_flk")])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("fixture PATH"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bounded structured dogfood fixture");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["status"], "passed");
    assert_eq!(proof["real_mcp"], true);
    assert_eq!(proof["framed_opening_turn"], true);
}

mod support;

/// A legacy peer sees exactly one ping, never a selector it could silently ignore.
#[tokio::test]
async fn public_cli_refuses_a_legacy_or_malformed_structured_capability() {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for capability in [None, Some(json!(false)), Some(json!("true"))] {
        let root = support::socket_dir::SocketDir::reserve("sc");
        let socket = root.path().join("a");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut command = support::environment::Command::new(env!("CARGO_BIN_EXE_flk"));
        command
            .args([
                "--json",
                "agent",
                "start",
                "fixture",
                "--runtime",
                "structured",
                "--cwd",
            ])
            .arg(root.path())
            .args(["--", "claude"])
            .env("FLOCK_SOCKET_PATH", &socket);
        let mut command = tokio::process::Command::from(command.into_std());
        command.kill_on_drop(true);
        let seen = std::cell::Cell::new(false);
        let peer = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "ping");
            seen.set(true);
            let mut reply = json!({"id":request["id"], "result":{"type":"pong",
                "version":"fixture", "protocol":support::PROTOCOL_VERSION,"capabilities":{}}});
            if let Some(capability) = capability {
                reply["result"]["capabilities"]["structured_agents"] = capability;
            }
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            let (extra, _) = listener.accept().await.unwrap();
            let mut extra = BufReader::new(extra);
            let mut line = String::new();
            assert!(extra.read_line(&mut line).await.unwrap() > 0);
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                output = command.output() => output,
                unexpected = peer => panic!("legacy peer received an effect request: {unexpected}"),
            }
        })
        .await
        .expect("legacy peer and owned CLI must finish within the fail ceiling");
        let output = output.unwrap();
        assert!(
            seen.get(),
            "structured negotiation must probe the actual peer"
        );
        assert_eq!(
            output.status.code(),
            Some(exit::PROTOCOL_GAP),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let refusal: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(refusal["error"]["code"], "server_version_gap");
        assert_eq!(refusal["error"]["retry"], "after_handoff");
        let listener = listener.into_std().unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[allow(dead_code)] // Numeric registry shared with this bin-only integration test.
#[path = "../src/cli/exit/constants.rs"]
mod exit;

/// Real headless reload must change subsequent admission, never existing leases.
#[test]
#[allow(clippy::disallowed_methods)] // Owns the bounded isolated server/probe tree.
fn isolated_public_structured_policy_live_reload() {
    let output = std::process::Command::new("python3")
        .args([
            "-I",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/probes/structured_public.py"
            ),
            "--flk",
            env!("CARGO_BIN_EXE_flk"),
            "--scenario",
            "reload",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("fixture PATH"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bounded isolated reload probe");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
