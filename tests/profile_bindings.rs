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

fn rpc(socket: &Path, method: &str, params: Value) -> Value {
    let request = json!({"id":"transaction-test","method":method,"params":params});
    serde_json::from_str(
        &json_api::request(socket, &request.to_string(), Duration::from_secs(5)).unwrap(),
    )
    .unwrap()
}

fn bind_request(binding: &str) -> Value {
    json!({"action":"bind","binding_id":binding,
        "target":{"repo_id":"repo-fixture","node_ids":["node-fixture"],"agent_ids":["agent-fixture"]},
        "account_alias":"fixture-account","credential_ref":"fixture-reference","credential_generation":1,
        "bundle_digests":["fixture-digest"],"grants":[]})
}

#[test]
fn socket_binding_transactions_replay_after_restart_and_revoke_terminally() {
    let dir = SocketDir::reserve("pb-transact");
    let root = dir.path().join("c");
    let state = root.join(support::paths::app_dir_name());
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), "onboarding = false\n").unwrap();
    let socket = dir.path().join("s.sock");
    let server = start(&root, &socket);
    let mut create = bind_request("binding-a");
    create.as_object_mut().unwrap().remove("binding_id");
    let plan = rpc(&socket, "profile.plan", create)["result"].clone();
    let binding_id = plan["review"]["binding"]["binding_id"]
        .as_str()
        .expect("server allocates binding identity")
        .to_owned();
    assert_eq!(binding_id.len(), 36);
    assert!(
        plan["plan"].is_string(),
        "socket API must produce a review plan: {plan}"
    );
    let stale = rpc(&socket, "profile.plan", bind_request("binding-b"))["result"].clone();
    assert_eq!(
        rpc(
            &socket,
            "profile.bind",
            json!({"plan":plan["plan"],"expect":"altered"})
        )["error"]["code"],
        "profile_plan_invalid"
    );
    let commit = json!({"plan":plan["plan"],"expect":plan["expect"]});
    let bound = rpc(&socket, "profile.bind", commit.clone());
    assert_eq!(bound["result"]["binding"]["revision"], 1);
    assert_eq!(rpc(&socket, "profile.bind", commit.clone()), bound);
    assert_eq!(
        rpc(
            &socket,
            "profile.bind",
            json!({"plan":stale["plan"],"expect":stale["expect"]})
        )["error"]["code"],
        "profile_revision_conflict"
    );
    drop(server);
    let _ = fs::remove_file(&socket);
    let server = start(&root, &socket);
    assert_eq!(rpc(&socket, "profile.bind", commit.clone()), bound);
    let revoke = json!({"binding":binding_id,"reason":"fixture revoke"});
    let revoked = rpc(&socket, "profile.revoke", revoke.clone());
    assert_eq!(revoked["result"]["binding"]["revision"], 2);
    assert_eq!(
        revoked["result"]["binding"]["revoked"]["operation_id"],
        revoked["result"]["operation_id"]
    );
    assert_eq!(rpc(&socket, "profile.revoke", revoke.clone()), revoked);
    assert_eq!(rpc(&socket, "profile.bind", commit.clone()), bound);
    drop(server);
    let _ = fs::remove_file(&socket);
    let _server = start(&root, &socket);
    assert_eq!(rpc(&socket, "profile.revoke", revoke), revoked);
    assert_eq!(rpc(&socket, "profile.bind", commit), bound);
    assert_eq!(
        rpc(&socket, "profile.plan", bind_request(&binding_id))["error"]["code"],
        "profile_binding_revoked"
    );
    let status = rpc(&socket, "profile.status", json!({}));
    assert_eq!(status["result"]["policy_revision"], 2);
    assert_eq!(status["result"]["execution_switch"], false);
    assert!(status["result"]["bindings"][0]["revoked"].is_object());
}

fn profile_cli(root: &Path, socket: &Path, args: &[&str], exit: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_flk"))
        .arg("profile")
        .args(args)
        .args(["--format", "json"])
        .envs(support::environment::isolated_env(
            root,
            &root.join("runtime"),
        ))
        .env("FLOCK_SOCKET_PATH", socket)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_binding_round_trip_against_isolated_server() {
    let dir = SocketDir::reserve("pb-cli");
    let root = dir.path().join("c");
    let state = root.join(support::paths::app_dir_name());
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), "onboarding = false\n").unwrap();
    let socket = dir.path().join("s.sock");
    let _server = start(&root, &socket);
    let plan = profile_cli(
        &root,
        &socket,
        &[
            "plan",
            "bind",
            "--repo",
            "repo-fixture",
            "--nodes",
            "node-fixture,node-other",
            "--agents",
            "agent-fixture",
            "--account",
            "fixture-account",
            "--bundle",
            "fixture-digest",
            "--credential-ref",
            "fixture-reference",
            "--generation",
            "1",
            "--grants",
            "grant-fixture",
        ],
        0,
    );
    let plan = &plan["result"];
    assert_eq!(
        plan["review"]["binding"]["target"]["node_ids"],
        json!(["node-fixture", "node-other"])
    );
    let commit = [
        "bind",
        "--plan",
        plan["plan"].as_str().unwrap(),
        "--expect",
        plan["expect"].as_str().unwrap(),
    ];
    let bound = profile_cli(&root, &socket, &commit, 0);
    let id = bound["result"]["binding"]["binding_id"].as_str().unwrap();
    assert_eq!(profile_cli(&root, &socket, &commit, 0), bound);
    let status = profile_cli(&root, &socket, &["status"], 0);
    assert_eq!(status["result"]["bindings"][0]["binding_id"], id);
    assert_eq!(status["result"]["execution_switch"], false);
    let invalid = profile_cli(
        &root,
        &socket,
        &[
            "bind",
            "--plan",
            plan["plan"].as_str().unwrap(),
            "--expect",
            "altered",
        ],
        1,
    );
    assert_eq!(invalid["error"]["code"], "profile_plan_invalid");
    assert_eq!(invalid["error"]["exit"], 1);
    let revoke = ["revoke", "--binding", id, "--reason", "fixture retirement"];
    let revoked = profile_cli(&root, &socket, &revoke, 0);
    assert_eq!(revoked["result"]["binding"]["revision"], 2);
    assert_eq!(profile_cli(&root, &socket, &revoke, 0), revoked);
    let status = profile_cli(&root, &socket, &["status"], 0);
    assert!(status["result"]["bindings"][0]["revoked"].is_object());
}

#[test]
fn socket_native_storage_recovers_staging_without_moving_published_evidence() {
    use std::os::unix::fs::PermissionsExt;
    let dir = SocketDir::reserve("pb-storage");
    let root = dir.path().join("c");
    let state = root.join(support::paths::app_dir_name());
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("config.toml"), "onboarding = false\n").unwrap();
    let storage = state.join("profile-storage");
    let sessions = storage.join("sessions");
    let session = "11111111-1111-4111-8111-111111111111";
    let published = sessions.join(session);
    let native = published.join("config");
    let staging = sessions.join(".alloc-22222222-2222-4222-8222-222222222222");
    for path in [&storage, &sessions, &published, &native, &staging] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let record = json!({"schema":1,"session_id":session,"repo_id":"fixture-repo",
        "node_id":"node.example","agent_id":"fixture-agent",
        "binding":{"binding_id":"fixture-binding","revision":7}});
    let bytes = serde_json::to_vec(&record).unwrap();
    fs::write(published.join("record.json"), &bytes).unwrap();
    fs::write(native.join("transcript"), b"published evidence").unwrap();
    fs::write(staging.join("partial"), b"crash evidence").unwrap();
    let socket = dir.path().join("s.sock");
    let mut first = None;
    for _ in 0..2 {
        let server = start(&root, &socket);
        let status = rpc(&socket, "profile.status", json!({}));
        let storage_status = &status["result"]["session_storage"];
        assert_eq!(storage_status["sessions"], json!([record]));
        assert_eq!(storage_status["quarantined"].as_array().unwrap().len(), 1);
        assert_eq!(storage_status["unavailable"], false);
        if let Some(previous) = &first {
            assert_eq!(storage_status, previous);
        }
        first = Some(storage_status.clone());
        assert!(!staging.exists());
        assert_eq!(fs::read(published.join("record.json")).unwrap(), bytes);
        assert_eq!(
            fs::read(native.join("transcript")).unwrap(),
            b"published evidence"
        );
        assert_eq!(
            rpc(
                &socket,
                "profile.enable",
                json!({"plan":"fixture-plan","expect":"fixture-digest"})
            )["error"]["code"],
            "structured_profiles_not_ready"
        );
        drop(server);
        let _ = fs::remove_file(&socket);
    }
    let quarantined = fs::read_dir(storage.join("quarantine"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(quarantined.path().join("partial")).unwrap(),
        b"crash evidence"
    );
}
