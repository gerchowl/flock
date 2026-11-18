mod support;

use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Stdio},
    time::{Duration, Instant},
};
use support::{environment::Command, json_api, socket_dir::SocketDir};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(root: &Path, socket: &Path) -> Server {
    let mut command = Command::new(env!("CARGO_BIN_EXE_flk"));
    command
        .arg("server")
        .envs(support::environment::isolated_env(
            root,
            &root.join("runtime"),
        ))
        .env("FLOCK_SOCKET_PATH", socket)
        .env_remove("FLOCK_PANE_ID")
        .env_remove("FLOCK_AGENT_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(root.join("server.stderr")).unwrap(),
        ));
    let mut server = Server(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "isolated server exited before readiness"
        );
        if let Ok(response) = json_api::request(
            socket,
            r#"{"id":"ready","method":"ping","params":{}}"#,
            Duration::from_secs(1),
        ) {
            if serde_json::from_str::<Value>(&response).is_ok() {
                return server;
            }
        }
        assert!(
            Instant::now() < deadline,
            "isolated server readiness exceeded fail ceiling"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn socket_status_restores_operator_bindings_and_true_switch_without_activation() {
    let dir = SocketDir::reserve("pb-restore");
    let root = dir.path().join("c");
    let state = root.join(support::paths::app_dir_name());
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), "onboarding = false\n").unwrap();
    let raw = json!({"schema":1,"policy_revision":7,"execution_switch":true,"unknown":[],
        "bindings":[{"binding_id":"binding-fixture","target":{"repo_id":"repo-fixture","node_ids":["node-fixture"],
        "agent_ids":["agent-fixture"]},"account_alias":"fixture-account","credential_ref":"fixture-reference",
        "credential_generation":1,"bundle_digests":["fixture-digest"],"grants":[],"revision":7,"revoked":null}]});
    let path = state.join("profile-bindings.json");
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    let socket = dir.path().join("s.sock");
    for generation in 0..3 {
        let mut expected = raw.clone();
        if generation == 2 {
            expected["policy_revision"] = json!(6);
            expected["bindings"][0]["revision"] = json!(6);
            fs::write(&path, serde_json::to_vec(&expected).unwrap()).unwrap();
        }
        let server = start(&root, &socket);
        let reply = json_api::request(
            &socket,
            r#"{"id":"status","method":"profile.status","params":{}}"#,
            Duration::from_secs(5),
        )
        .unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["result"]["policy_revision"], 7);
        assert_eq!(reply["result"]["execution_switch"], generation != 2);
        if generation == 2 {
            assert_eq!(reply["result"]["bindings"], json!([]));
            assert_eq!(
                reply["result"]["policy_error"],
                json!({"code":"policy_revision_regressed","store":6,"floor":7})
            );
            let normal = json_api::request(
                &socket,
                r#"{"id":"normal","method":"workspace.list","params":{}}"#,
                Duration::from_secs(5),
            )
            .unwrap();
            assert!(serde_json::from_str::<Value>(&normal)
                .unwrap()
                .get("error")
                .is_none());
        } else {
            assert_eq!(reply["result"]["bindings"], raw["bindings"]);
        }
        let reply = json_api::request(&socket, r#"{"id":"enable","method":"profile.enable","params":{"plan":"fixture-plan","expect":"fixture-digest"}}"#, Duration::from_secs(5)).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(
            reply["error"]["code"],
            if generation == 2 {
                "policy_revision_regressed"
            } else {
                "structured_profiles_not_ready"
            }
        );
        drop(server);
        let _ = fs::remove_file(&socket);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            expected
        );
    }
}
