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
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    sync::{atomic::AtomicBool, Arc},
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
    listener: UnixListener,
    fixture: Fixture,
    _env: Environment,
}
impl AdmissionFixture {
    fn new(mode: &str, stall_ms: u64) -> Self {
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
        env.set("FLOCK_SOCKET_PATH", Some(root.join("api.sock").into()));
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
        fixture.file("claude", script.as_bytes(), 0o700);
        let config: crate::config::Config = toml::from_str(&format!("[sessions]\nstall_ms = {stall_ms}\nblob_min_bytes = 64\n[structured]\nrun_as = 'operator'\nprofile = 'fixture'\ntoken_file = {:?}\n",root.join("token").to_str().unwrap())).unwrap();
        let listener = UnixListener::bind(root.join("api.sock")).unwrap();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&config, true, None, rx, Default::default());
        app.runtime_config.uid_policy.dedicated = Ok(None).into();
        app.runtime_config.structured_mcp = Some(fixture.mcp());
        Self {
            app,
            listener,
            fixture,
            _env: env,
        }
    }
    async fn client<T: Send + 'static>(&mut self, call: impl FnOnce() -> T + Send + 'static) -> T {
        let listener = self.listener.try_clone().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.app.api_rx = rx;
        let socket = std::thread::spawn(move || {
            let mut fd = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: initialized pollfd borrowing the owned short listener.
            assert_eq!(
                unsafe { libc::poll(&mut fd, 1, 10_000) },
                1,
                "client never connected"
            );
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(LIMIT)).unwrap();
            let policy = crate::structured::identity::UidPolicy {
                dedicated: Ok(None).into(),
                ..Default::default()
            };
            crate::api::server::handle_connection(
                stream,
                &tx,
                &Default::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
                &Default::default(),
                &policy,
            )
            .unwrap();
        });
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
        socket.join().unwrap();
        result
    }
    async fn request(&mut self, method: Method) -> Value {
        let socket = self.fixture.root.path().join("api.sock");
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
