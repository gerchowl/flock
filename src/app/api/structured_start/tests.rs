#[test]
fn configured_stall_policy_is_defaulted_and_range_checked() {
    let default: crate::config::Config = toml::from_str("").unwrap();
    assert_eq!(default.sessions.stall_ms, 120_000);
    for value in [5000, 3_600_000] {
        let config: crate::config::Config =
            toml::from_str(&format!("[sessions]\nstall_ms = {value}\n")).unwrap();
        assert_eq!(config.sessions.stall_ms, value);
    }
    for value in [0, 4999, 3_600_001] {
        let error =
            toml::from_str::<crate::config::Config>(&format!("[sessions]\nstall_ms = {value}\n"))
                .unwrap_err();
        assert_eq!(error.message(), format!("invalid value: integer `{value}`, expected sessions.stall_ms in 5000..=3600000 milliseconds"));
        assert!(error.to_string().contains("stall_ms"));
    }
}

use super::*;
use crate::{
    api::schema::{Method, Request},
    app::App,
    structured::runtime::tests::Fixture,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Instant,
};
const LIMIT: Duration = Duration::from_secs(10);

/// Every variable is restored after owned producers have joined.
struct Environment(Vec<(OsString, Option<OsString>)>);
impl Environment {
    fn set(&mut self, key: impl Into<OsString>, value: Option<OsString>) {
        let key = key.into();
        self.0.push((key.clone(), std::env::var_os(&key)));
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
struct AdmissionFixture {
    app: App,
    server: Option<crate::api::ServerHandle>,
    socket: PathBuf,
    fixture: Fixture,
    _env: Environment,
}
impl AdmissionFixture {
    fn new(mode: &str, stall_ms: u64) -> Self {
        Self::with_endpoint(mode, stall_ms, false, false)
    }
    fn with_endpoint(mode: &str, stall_ms: u64, relative: bool, unavailable: bool) -> Self {
        let fixture = Fixture::for_socket();
        let root = fixture.root.path();
        let mut env = Environment(vec![]);
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("FLOCK_"))
        {
            env.set(key, None);
        }
        for key in ["HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
            env.set(key, Some(root.into()));
        }
        for key in ["LANG", "LC_ALL"] {
            env.set(key, Some("C".into()));
        }
        for key in ["USER", "LOGNAME"] {
            env.set(key, Some("fixture-user".into()));
        }
        let mut path = vec![root.to_owned()];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        env.set("PATH", Some(std::env::join_paths(path).unwrap()));
        let socket = if relative {
            // guardrails-ok(hermetic): cwd is supplied by this owned subprocess fixture.
            assert_eq!(
                std::env::current_dir().unwrap(),
                PathBuf::from(
                    std::env::var_os("FLK_RELATIVE_FIXTURE_CWD").expect("owned child cwd")
                )
            );
            PathBuf::from("relative.sock")
        } else {
            root.join("api.sock")
        };
        env.set("FLOCK_SOCKET_PATH", Some(socket.clone().into()));
        fs::create_dir_all(crate::config::config_dir()).unwrap();
        let cwd = root.join("project");
        fs::create_dir(&cwd).unwrap();
        fixture.file(
            "case.json",
            json!({"mode":mode,"version":"2.1.296", "cwd":cwd,"model":"fixture-worker-model"})
                .to_string()
                .as_bytes(),
            0o600,
        );
        // The caller creates fresh UUIDs and private config paths. Validate those
        // argument slots instead of predicting entropy in the launch fixture.
        let script = fs::read_to_string(root.join("claude")).unwrap().replace(
            "assert sys.argv[1:] == json.loads((root / \"argv.json\").read_text()), \"argv drift\"",
            "assert sys.argv[1:4] == ['-p', '--input-format', 'stream-json']\nassert '--restricted' in sys.argv and '--strict-mcp-config' in sys.argv\nimport uuid\nuuid.UUID(sys.argv[sys.argv.index('--session-id') + 1])");
        // Publish complete observed config atomically before readiness events.
        let script = script.replace(
            r#"(root / "observed-mcp.json").write_text(json.dumps(registration))"#,
            "(root / \"observed-mcp.pending\").write_text(json.dumps(registration))\nos.replace(root / \"observed-mcp.pending\", root / \"observed-mcp.json\")");
        fixture.file("claude", script.as_bytes(), 0o700);
        let config: crate::config::Config = toml::from_str(&format!("[sessions]\nstall_ms = {stall_ms}\nblob_min_bytes = 64\n[structured]\nrun_as = 'operator'\nprofile = 'fixture'\ntoken_file = {:?}\n",root.join("token").to_str().unwrap())).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = crate::api::EventHub::default();
        let policy = crate::structured::identity::UidPolicy {
            dedicated: Ok(None).into(),
            ..Default::default()
        };
        let mut server = crate::api::server::start_server_with_capabilities(
            tx,
            hub.clone(),
            Some(crate::api::schema::ServerCapabilities {
                structured_agents: true,
                live_handoff: false,
                pane_input_write_ack: false,
                node_id: None,
                clone_detection_warning: None,
                agent_start_label: true,
                agent_questions: true,
            }),
            policy.clone(),
        )
        .expect("ordinary listener startup");
        if unavailable {
            server.mcp_executable = || Err(io::Error::other("private resolver detail canary"));
        }
        let mut app = App::new(&config, true, None, rx, hub);
        app.runtime_config.uid_policy = policy;
        let initialized = app.initialize_mesh_mail(Some(&server));
        let fixture = Self {
            app,
            server: Some(server),
            socket,
            fixture,
            _env: env,
        };
        assert!(
            initialized.is_ok(),
            "structured endpoint must not block startup: {initialized:?}"
        );
        fixture
    }
    async fn client<T: Send + 'static>(&mut self, call: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = tokio::sync::oneshot::channel();
        let client = std::thread::spawn(move || {
            let _ = done.send(call());
        });
        let request = tokio::time::timeout(LIMIT, self.app.api_rx.recv())
            .await
            .unwrap()
            .unwrap();
        self.app.handle_api_request_message(request);
        let result = tokio::time::timeout(LIMIT, result).await.unwrap().unwrap();
        client.join().unwrap();
        result
    }
    async fn request(&mut self, method: Method) -> Value {
        let socket = self.socket.clone();
        self.client(move || {
            let mut stream = UnixStream::connect(socket).unwrap();
            stream.set_read_timeout(Some(LIMIT)).unwrap();
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&Request {
                    id: "fixture-request".into(),
                    method
                })
                .unwrap()
            )
            .unwrap();
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).unwrap();
            serde_json::from_str(&response).unwrap()
        })
        .await
    }
    async fn start(&mut self) -> (String, u64) {
        let args = vec![
            "fixture-launch".into(),
            "--runtime".into(),
            "structured".into(),
            "--model".into(),
            "fixture-worker-model".into(),
            "--cwd".into(),
            self.fixture
                .root
                .path()
                .join("project")
                .to_string_lossy()
                .into_owned(),
            "--".into(),
            "claude".into(),
        ];
        assert_eq!(
            self.client(move || crate::cli::test_structured_start(&args))
                .await
                .unwrap(),
            crate::cli::exit::SUCCESS
        );
        let records = self.app.state.structured_agents.records();
        assert_eq!(records.len(), 1);
        let id = records[0].agent_id.clone();
        let generation = self.app.state.structured_agents.get(&id).unwrap().2;
        let notify = self.app.render_notify.clone();
        crate::structured::workers::tests::wait(&notify, || {
            self.app.structured_workers.poll(
                &mut self.app.state.structured_agents,
                Instant::now(),
                1000,
            );
            assert!(
                self.app
                    .structured_workers
                    .report(&id, generation)
                    .is_none(),
                "worker failed before readiness"
            );
            self.app
                .structured_workers
                .runtime(&id, generation)
                .is_some()
        })
        .await;
        assert_eq!(
            self.app.state.workspaces.len(),
            0,
            "structured launch creates no pane"
        );
        (id, generation)
    }
    fn submit(&self, id: &str, generation: u64) {
        self.app.structured_workers.runtime(id, generation).unwrap().try_submit(&json!({"type":"user","message":{"role":"user","content":"fixture turn"},"parent_tool_use_id":null,"session_id":""})).unwrap();
    }
}
impl Drop for AdmissionFixture {
    fn drop(&mut self) {
        self.app
            .structured_workers
            .shutdown(&mut self.app.state.structured_agents)
            .expect("fixture workers must join");
        self.server.take().unwrap().finish_for_test();
    }
}

/// Loaded policy passes through CLI, a real local socket and the production owner.
#[tokio::test]
async fn cli_socket_launch_observes_configured_stall_without_changing_phase() {
    let _guard = crate::config::test_config_env_guard();
    for stall_ms in [5000, 17_000] {
        let mut fixture = AdmissionFixture::new("s4-accepted-silent", stall_ms);
        let (id, generation) = fixture.start().await;
        let submitted = Instant::now();
        fixture.submit(&id, generation);
        let notify = fixture.app.render_notify.clone();
        crate::structured::workers::tests::wait(&notify, || {
            fixture.app.structured_workers.poll(
                &mut fixture.app.state.structured_agents,
                Instant::now(),
                1000,
            );
            fixture
                .app
                .state
                .structured_agents
                .get(&id)
                .unwrap()
                .1
                .update
                .observe_at
                .is_some()
        })
        .await;
        let observed = Instant::now();
        let due = fixture
            .app
            .structured_workers
            .next_deadline(&fixture.app.state.structured_agents)
            .unwrap();
        assert!(due >= submitted + Duration::from_millis(stall_ms));
        assert!(
            due <= observed + Duration::from_millis(stall_ms),
            "loaded policy must reach owner reducer"
        );
        let phase = fixture
            .app
            .state
            .structured_agents
            .get(&id)
            .unwrap()
            .1
            .update
            .state
            .phase;
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            due - Duration::from_nanos(1),
            1000,
        );
        assert!(fixture
            .app
            .state
            .structured_agents
            .get(&id)
            .unwrap()
            .1
            .update
            .stalled_since_ms
            .is_none());
        fixture
            .app
            .structured_workers
            .poll(&mut fixture.app.state.structured_agents, due, 1000);
        let view = &fixture
            .app
            .state
            .structured_agents
            .get(&id)
            .unwrap()
            .1
            .update;
        assert!(view.stalled_since_ms.is_some());
        assert_eq!(view.state.phase, phase);
        let read = fixture
            .request(Method::AgentGet(crate::api::schema::AgentTarget {
                target: "fixture-launch".into(),
            }))
            .await;
        assert_eq!(read["result"]["agent"]["agent_id"], id);
        assert!(read["result"]["agent"].get("pane_id").is_none());
        let by_id = fixture
            .request(Method::AgentGet(crate::api::schema::AgentTarget {
                target: id.clone(),
            }))
            .await;
        assert_eq!(
            by_id["result"]["agent"]["session_id"],
            read["result"]["agent"]["session_id"]
        );
        let listed = fixture
            .request(Method::AgentList(crate::api::schema::EmptyParams {}))
            .await;
        assert_eq!(listed["result"]["agents"], json!([]));
        assert_eq!(listed["result"]["structured_agents"][0]["agent_id"], id);
        let status: Method =
            serde_json::from_value(json!({"method":"agent.status", "params":{"target":id}}))
                .unwrap();
        let status = fixture.request(status).await;
        assert_eq!(status["result"]["agent"]["agent_id"], id);
        let stopped = fixture
            .request(Method::AgentStop(crate::api::schema::AgentTarget {
                target: id.clone(),
            }))
            .await;
        assert!(stopped.get("error").is_none(), "{stopped}");
        let notify = fixture.app.render_notify.clone();
        crate::structured::workers::tests::wait(&notify, || {
            fixture.app.structured_workers.poll(
                &mut fixture.app.state.structured_agents,
                Instant::now(),
                1000,
            );
            fixture
                .app
                .structured_workers
                .report(&id, generation)
                .is_some()
        })
        .await;
        let stopped = fixture
            .request(Method::AgentGet(crate::api::schema::AgentTarget {
                target: id.clone(),
            }))
            .await;
        assert_eq!(stopped["result"]["agent"]["lifecycle"], "offline");
        let resume: AgentStartParams = serde_json::from_value(
            json!({"name":"fixture-launch", "runtime":"structured", "argv":["claude"]}),
        )
        .unwrap();
        let resumed = fixture.request(Method::AgentStart(resume)).await;
        assert_eq!(resumed["result"]["agent"]["agent_id"], id);
        assert_eq!(
            resumed["result"]["agent"]["session_id"],
            read["result"]["agent"]["session_id"]
        );
        assert_eq!(
            resumed["result"]["agent"]["cwd"],
            read["result"]["agent"]["cwd"]
        );
        assert_eq!(resumed["result"]["agent"]["model"], "fixture-worker-model");
    }
}

/// A below-default complete block must follow the loaded policy at real admission.
#[tokio::test]
async fn cli_socket_launch_persists_configured_blob_reference() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("worker-exit-ok", 5000);
    let block = json!({"type":"thinking", "thinking":"x".repeat(256)});
    let bytes = serde_json::to_vec(&block).unwrap();
    assert!(bytes.len() > 64 && bytes.len() < 2048);
    fixture.fixture.file(
        "state-events.jsonl",
        format!(
            "{}\n{}\n",
            json!({"type":"assistant","message":{"role":"assistant","content":[block]}}),
            crate::structured::workers::tests::event("result")
        )
        .as_bytes(),
        0o600,
    );
    let (id, generation) = fixture.start().await;
    let session = fixture
        .app
        .state
        .structured_agents
        .get(&id)
        .unwrap()
        .0
        .session_id
        .clone();
    fixture.submit(&id, generation);
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        fixture
            .app
            .structured_workers
            .report(&id, generation)
            .is_some()
    })
    .await;
    assert!(fixture
        .app
        .structured_workers
        .report(&id, generation)
        .unwrap()
        .result
        .is_ok());
    let stream = fs::read_to_string(
        crate::session::data_dir()
            .join("sessions")
            .join(id)
            .join(format!("{session}.jsonl")),
    )
    .unwrap();
    let stored = stream
        .lines()
        .map(|line| {
            let envelope: Value = serde_json::from_str(line).unwrap();
            serde_json::from_str::<Value>(envelope["raw"].as_str().unwrap()).unwrap()
        })
        .find(|value| value["type"] == "assistant")
        .unwrap();
    let reference: crate::structured::blobs::BlobRef =
        serde_json::from_value(stored["message"]["content"][0].clone())
            .expect("production caller must pass loaded storage policy");
    let registration: Value = serde_json::from_slice(
        &fs::read(fixture.fixture.root.path().join("observed-mcp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registration["mcpServers"]["flock"]["command"],
        std::env::current_exe()
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        registration["mcpServers"]["flock"]["env"]["FLOCK_SOCKET_PATH"],
        fixture.socket.to_string_lossy().as_ref()
    );
    let store = crate::structured::blobs::BlobStore::open(&crate::session::data_dir()).unwrap();
    assert_eq!(store.get(&reference.blob).unwrap(), bytes);
}

/// Refused selectors and default run-as have no metadata, profile or process effects.
#[tokio::test]
async fn socket_structured_start_refuses_unavailable_launcher_and_unsupported_selectors() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("s4-accepted-silent", 5000);
    let base = json!({"name":"fixture-launch", "runtime":"structured", "argv":["claude"], "cwd":fixture.fixture.root.path().join("project")});
    fixture.app.state.config.structured.run_as = crate::structured::launch::RunAs::Agent;
    let response = fixture
        .request(Method::AgentStart(
            serde_json::from_value(base.clone()).unwrap(),
        ))
        .await;
    assert_eq!(response["error"]["code"], "structured_needs_agent_user");
    assert!(fixture.app.state.structured_agents.records().is_empty());
    assert!(!fixture.fixture.root.path().join("version.pid").exists());
    fixture.app.state.config.structured.run_as = crate::structured::launch::RunAs::Operator;
    for (field, value) in [
        ("focus", json!(true)),
        ("here", json!(true)),
        ("active", json!(true)),
        ("dry_run", json!(true)),
        ("workspace_id", json!("fixture-space")),
        ("tab_id", json!("fixture-tab")),
        ("split", json!("right")),
        ("label", json!("fixture-label")),
    ] {
        let mut params = base.clone();
        params[field] = value;
        let response = fixture
            .request(Method::AgentStart(serde_json::from_value(params).unwrap()))
            .await;
        assert_eq!(
            response["error"]["code"], "runtime_unsupported",
            "selector {field}: {response}"
        );
        assert!(fixture.app.state.structured_agents.records().is_empty());
    }
    for argv in [
        json!(["claude", "--dangerously-skip-permissions"]),
        json!(["codex"]),
    ] {
        let mut params = base.clone();
        params["argv"] = argv;
        let response = fixture
            .request(Method::AgentStart(serde_json::from_value(params).unwrap()))
            .await;
        assert_eq!(response["error"]["code"], "invalid_agent_argv");
        assert!(fixture.app.state.structured_agents.records().is_empty());
    }
    assert!(!fixture.fixture.root.path().join("version.pid").exists());
    assert!(serde_json::from_value::<AgentStartParams>(
        json!({"name":"fixture-launch", "argv":["claude"], "runtime":"future"})
    )
    .is_err());
    let pty: AgentStartParams =
        serde_json::from_value(json!({"name":"fixture-pty", "argv":["claude"]})).unwrap();
    assert_eq!(pty.runtime, crate::api::schema::AgentRuntime::Pty);
    assert!(serde_json::to_value(pty).unwrap().get("runtime").is_none());
}

/// Provisioning uses the actual listener handle and normal mesh initialization.
#[tokio::test]
async fn startup_with_relative_socket_keeps_legacy_routes_available() {
    if std::env::var_os("FLK_RELATIVE_FIXTURE_CHILD").is_none() {
        let cwd = crate::test_support::TempDir::socket();
        let mut command = crate::process::TracedCommand::new(
            std::env::current_exe().unwrap(),
            "relative-socket-fixture",
        );
        command.args(["--exact", "app::api::structured_start::tests::startup_with_relative_socket_keeps_legacy_routes_available", "--nocapture", "--test-threads=1"])
            .env_clear().env("PATH", std::env::var_os("PATH").unwrap())
            .env("FLK_RELATIVE_FIXTURE_CHILD", "1")
            .env("FLK_RELATIVE_FIXTURE_CWD", cwd.path())
            .current_dir(cwd.path()).process_group(0);
        let output = command
            .output_traced_with_timeout_and_limit(LIMIT, 1024 * 1024)
            .unwrap();
        assert!(
            output.status.success(),
            "relative socket fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fs::read_dir(cwd.path()).unwrap().next().is_none(),
            "fixture listener must clean up"
        );
        return;
    }
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::with_endpoint("s4-accepted-silent", 5000, true, false);
    assert!(!fixture.socket.is_absolute());
    let (agent, generation) = fixture.start().await;
    fixture.submit(&agent, generation);
    let observed = fixture.fixture.root.path().join("observed-mcp.json");
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        observed.is_file()
    })
    .await;
    let registered: Value = serde_json::from_slice(&fs::read(observed).unwrap()).unwrap();
    let registered_socket = PathBuf::from(
        registered["mcpServers"]["flock"]["env"]["FLOCK_SOCKET_PATH"]
            .as_str()
            .unwrap(),
    );
    assert!(registered_socket.is_absolute());
    assert_eq!(
        registered_socket.canonicalize().unwrap(),
        fixture.socket.canonicalize().unwrap()
    );
    let response = fixture
        .request(Method::AgentList(crate::api::schema::EmptyParams {}))
        .await;
    assert_eq!(response["result"]["agents"], json!([]));
}

#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Failure is local to structured admission, with fixed and typed diagnostics.
#[tokio::test]
async fn startup_with_unresolvable_endpoint_warns_and_only_structured_start_refuses() {
    let _guard = crate::config::test_config_env_guard();
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let mut fixture = tracing::subscriber::with_default(subscriber, || {
        AdmissionFixture::with_endpoint("s4-accepted-silent", 5000, false, true)
    });
    assert!(fixture.app.runtime_config.structured_mcp.is_none());
    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        output.contains("structured.mcp.endpoint_unavailable"),
        "{output}"
    );
    assert!(
        output.contains("structured_endpoint_restart_required"),
        "{output}"
    );
    assert!(!output.contains("private resolver detail canary"));
    let response = fixture
        .request(Method::AgentList(crate::api::schema::EmptyParams {}))
        .await;
    assert_eq!(response["result"]["agents"], json!([]));
    let params: AgentStartParams = serde_json::from_value(json!({"name":"fixture-launch", "runtime":"structured", "argv":["claude"], "cwd":fixture.fixture.root.path().join("project")})).unwrap();
    let response = fixture.request(Method::AgentStart(params)).await;
    assert_eq!(
        response["error"]["code"],
        "structured_endpoint_restart_required"
    );
    assert!(fixture.app.state.structured_agents.records().is_empty());
    assert!(!fixture.fixture.root.path().join("version.pid").exists());
}

#[test]
fn endpoint_warning_has_a_closed_path_free_reason() {
    for (failure, expected) in [
        (crate::api::server::McpEndpointFailure::Exe, "exe"),
        (crate::api::server::McpEndpointFailure::Socket, "socket"),
    ] {
        let capture = LogCapture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            crate::logging::structured_mcp_endpoint_unavailable(failure)
        });
        let bytes = capture.0.lock().unwrap().clone();
        let log: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(log["fields"]["reason"], expected);
        assert_eq!(
            log["fields"]["event"],
            "structured.mcp.endpoint_unavailable"
        );
        assert!(log["fields"].get("path").is_none());
    }
}

/// Resume travels through App admission and a real socket, retaining the child's
/// admitted ancestry rather than taking the operator's root exemption.
#[tokio::test]
async fn app_operator_resume_preserves_record_and_lease_lineage() {
    operator_resume_preserves_lineage(false).await;
}

/// The real session snapshot is the only lineage source in this fresh App.
#[tokio::test]
async fn cold_restore_operator_resume_preserves_lineage_and_enforces_fanout() {
    operator_resume_preserves_lineage(true).await;
}

async fn operator_resume_preserves_lineage(cold: bool) {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("s4-accepted-silent", 5000);
    fixture.app.state.config.fleet.agent_spawn_enabled = true;
    fixture.app.state.config.fleet.max_spawn_depth = 4;
    let params = serde_json::from_value(json!({"name":"lineage-child", "runtime":"structured", "argv":["claude"], "cwd":fixture.fixture.root.path().join("project"), "model":"fixture-worker-model"})).unwrap();
    fixture
        .app
        .start_structured(
            params,
            &SpawnCaller::Structured {
                agent_id: Some("original-parent".into()),
                depth: 2,
            },
        )
        .unwrap();
    let record = fixture.app.state.structured_agents.records().pop().unwrap();
    assert_eq!(
        record.lineage,
        Some(crate::structured::registry::Lineage {
            parent: Some("original-parent".into()),
            depth: 3
        })
    );
    let id = record.agent_id.clone();
    let generation = fixture.app.state.structured_agents.get(&id).unwrap().2;
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        assert!(
            fixture
                .app
                .structured_workers
                .report(&id, generation)
                .is_none(),
            "launch failed"
        );
        fixture
            .app
            .structured_workers
            .runtime(&id, generation)
            .is_some()
            && fixture.fixture.root.path().join("validated").is_file()
    })
    .await;
    let stopped = fixture
        .request(Method::AgentStop(crate::api::schema::AgentTarget {
            target: id.clone(),
        }))
        .await;
    assert!(stopped.get("error").is_none(), "{stopped}");
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        fixture
            .app
            .structured_workers
            .report(&id, generation)
            .is_some()
    })
    .await;
    crate::structured::workers::tests::join_reported(
        &mut fixture.app.structured_workers,
        &id,
        generation,
    )
    .await;
    for name in ["model.pid", "validated"] {
        fs::remove_file(fixture.fixture.root.path().join(name)).unwrap();
    }
    if cold {
        fixture.app.no_session = false;
        fixture.app.save_session_now();
        let snapshot = crate::persist::load().expect("saved structured registry");
        assert_eq!(snapshot.structured_agents.records(), vec![record.clone()]);
        let config = fixture.app.state.config.clone();
        let policy = fixture.app.runtime_config.uid_policy.clone();
        fixture
            .app
            .structured_workers
            .shutdown(&mut fixture.app.state.structured_agents)
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = crate::api::EventHub::default();
        let restored = App::new(&config, false, None, rx, hub.clone());
        let old = std::mem::replace(&mut fixture.app, restored);
        drop(old);
        fixture.server.take().unwrap().finish_for_test();
        let server = crate::api::server::start_server_with_capabilities(
            tx,
            hub,
            Some(crate::api::schema::ServerCapabilities {
                structured_agents: true,
                live_handoff: false,
                pane_input_write_ack: false,
                node_id: None,
                clone_detection_warning: None,
                agent_start_label: true,
                agent_questions: true,
            }),
            policy.clone(),
        )
        .unwrap();
        fixture.app.runtime_config.uid_policy = policy;
        fixture.app.initialize_mesh_mail(Some(&server)).unwrap();
        fixture.server = Some(server);
        assert_eq!(
            fixture.app.state.structured_agents.records(),
            vec![record.clone()]
        );
        assert!(fixture
            .app
            .structured_workers
            .runtime(&id, generation)
            .is_none());
        assert!(fixture
            .app
            .session_leases
            .admitted_identities()
            .unwrap()
            .is_empty());
        // Operator resume is exempt even when current policy allows no children.
        fixture.app.state.config.fleet.max_spawn_fanout = 0;
    }
    let notify = fixture.app.render_notify.clone();
    let resume = serde_json::from_value(
        json!({"name":"lineage-child", "runtime":"structured", "argv":["claude"]}),
    )
    .unwrap();
    let resumed = fixture.request(Method::AgentStart(resume)).await;
    assert_eq!(resumed["result"]["agent"]["agent_id"], id, "{resumed}");
    let generation = fixture.app.state.structured_agents.get(&id).unwrap().2;
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        assert!(
            fixture
                .app
                .structured_workers
                .report(&id, generation)
                .is_none(),
            "resume failed"
        );
        fixture
            .app
            .structured_workers
            .runtime(&id, generation)
            .is_some()
            && fixture.fixture.root.path().join("validated").is_file()
    })
    .await;
    assert_eq!(
        fixture.app.state.structured_agents.get(&id).unwrap().0,
        &record
    );
    let pid = fs::read_to_string(fixture.fixture.root.path().join("model.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        fixture.app.session_leases.caller(Some(pid)),
        crate::structured::lease::Caller::Agent {
            agent: id.clone(),
            depth: 3
        }
    );
    assert_eq!(
        fixture
            .app
            .session_leases
            .admitted_identities()
            .unwrap()
            .get(&id),
        Some(&Some("original-parent".into()))
    );
    if cold {
        fixture.app.state.config.fleet.max_spawn_fanout = 1;
        let params = serde_json::from_value(json!({
            "name":"next-child", "runtime":"structured", "argv":["claude"],
            "cwd":fixture.fixture.root.path().join("project"), "model":"fixture-worker-model"
        }))
        .unwrap();
        assert_eq!(
            fixture
                .app
                .start_structured(
                    params,
                    &SpawnCaller::Structured {
                        agent_id: Some("original-parent".into()),
                        depth: 2,
                    }
                )
                .unwrap_err(),
            ErrorCode::AtFanoutLimit
        );
        assert_eq!(fixture.app.state.structured_agents.records().len(), 1);
    }
}

#[tokio::test]
async fn socket_agent_start_reserves_self_before_any_launch_effect() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("s4-accepted-silent", 5000);
    let params = serde_json::from_value(json!({"name":"self", "runtime":"structured", "argv":["claude"], "cwd":fixture.fixture.root.path().join("project")})).unwrap();
    let response = fixture.request(Method::AgentStart(params)).await;
    assert_eq!(
        response["error"]["code"], "invalid_agent_name",
        "{response}"
    );
    assert!(fixture.app.state.structured_agents.records().is_empty());
    assert!(!fixture.fixture.root.path().join("version.pid").exists());
}

/// Legacy identity metadata is accepted leniently and only the operator explicitly
/// starts its root lease, without guessing a historical parent.
#[tokio::test]
async fn legacy_unknown_lineage_operator_resume_is_a_root() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("s4-accepted-silent", 5000);
    let old = json!([{"runtime":"structured", "agent_id":"legacy-structured", "name":"legacy", "harness":"claude", "session_id":"00000000-0000-4000-8000-000000002055", "cwd":fixture.fixture.root.path().join("project"), "model":"fixture-worker-model"}]);
    fixture.app.state.structured_agents = serde_json::from_value(old).unwrap();
    assert!(fixture
        .app
        .state
        .structured_agents
        .get("legacy-structured")
        .unwrap()
        .0
        .lineage
        .is_none());
    let params =
        serde_json::from_value(json!({"name":"legacy", "runtime":"structured", "argv":["claude"]}))
            .unwrap();
    let resumed = fixture.request(Method::AgentStart(params)).await;
    assert_eq!(
        resumed["result"]["agent"]["agent_id"], "legacy-structured",
        "{resumed}"
    );
    let generation = fixture
        .app
        .state
        .structured_agents
        .get("legacy-structured")
        .unwrap()
        .2;
    let notify = fixture.app.render_notify.clone();
    crate::structured::workers::tests::wait(&notify, || {
        fixture.app.structured_workers.poll(
            &mut fixture.app.state.structured_agents,
            Instant::now(),
            1000,
        );
        assert!(
            fixture
                .app
                .structured_workers
                .report("legacy-structured", generation)
                .is_none(),
            "legacy resume failed"
        );
        fixture
            .app
            .structured_workers
            .runtime("legacy-structured", generation)
            .is_some()
            && fixture.fixture.root.path().join("validated").is_file()
    })
    .await;
    let pid = fs::read_to_string(fixture.fixture.root.path().join("model.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        fixture.app.session_leases.caller(Some(pid)),
        crate::structured::lease::Caller::Agent {
            agent: "legacy-structured".into(),
            depth: 0
        }
    );
    assert_eq!(
        fixture
            .app
            .session_leases
            .admitted_identities()
            .unwrap()
            .get("legacy-structured"),
        Some(&None)
    );
}

/// A pre-reservation record must explain how the operator can recover it.
#[tokio::test]
async fn legacy_self_record_has_typed_actionable_recovery_refusal() {
    let _guard = crate::config::test_config_env_guard();
    let mut fixture = AdmissionFixture::new("s4-accepted-silent", 5000);
    fixture.app.state.structured_agents = serde_json::from_value(json!([{
        "runtime":"structured", "agent_id":"legacy-self", "name":"self",
        "harness":"claude", "session_id":"00000000-0000-4000-8000-000000002075",
        "cwd":fixture.fixture.root.path().join("project")
    }]))
    .unwrap();
    for name in ["self", "legacy-self"] {
        let params = serde_json::from_value(json!({
            "name":name, "runtime":"structured", "argv":["claude"]
        }))
        .unwrap();
        let refused = fixture.request(Method::AgentStart(params)).await;
        assert_eq!(
            refused["error"]["code"], "structured_legacy_name_reserved",
            "{refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("self") && message.contains("rename or remove"),
            "{message}"
        );
        assert_eq!(
            ErrorCode::from_code(refused["error"]["code"].as_str().unwrap())
                .unwrap()
                .retry(),
            crate::cli::exit::Retry::AfterHuman
        );
    }
    assert_eq!(fixture.app.state.structured_agents.records().len(), 1);
    assert!(!fixture.fixture.root.path().join("version.pid").exists());
}
