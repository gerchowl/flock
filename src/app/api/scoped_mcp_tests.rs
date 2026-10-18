//! Real MCP orphan regression, with real kernel identity and App effects.
#![allow(clippy::disallowed_methods)] // Test harness owns every subprocess and socket.
use crate::{
    api::{ApiRequestMessage, EventHub},
    app::App,
    config::Config,
    platform,
    structured::{identity::UidPolicy, lease::SessionLeases},
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Child, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;
const CEILING: Duration = Duration::from_secs(5);

/// Exec fixture entry: the production MCP loop runs in a separate process.
#[test]
fn mcp_fixture_child() {
    if std::env::var_os("SCOPED_MCP_FIXTURE_CHILD").is_none() {
        return;
    }
    let code = crate::mcp::serve_over_stdio(true).expect("fixture MCP startup");
    std::process::exit(code);
}

struct Fixture {
    parent: Child,
    child: Option<(u32, u64)>,
    parent_dead: bool,
    lines: mpsc::UnboundedReceiver<Result<Value, String>>,
    _root: crate::test_support::TempDir,
    _profile: crate::structured::launch::LaunchProfile,
    _launch_fixture: crate::structured::runtime::tests::Fixture,
    socket: std::path::PathBuf,
    running: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some((pid, birth)) = self.child {
            if platform::process_start_time(pid) == Some(birth) {
                platform::signal_processes(&[pid], platform::Signal::Kill);
            }
        }
        let _ = self.parent.kill();
        let _ = self.parent.wait();
        self.running.store(false, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}
impl Fixture {
    fn new(
        lease: &crate::structured::lease::Lease,
        tx: crate::api::ApiRequestSender,
        policy: UidPolicy,
    ) -> Self {
        let root = crate::test_support::TempDir::socket();
        let socket = root.path().join("a");
        let mcp_output_path = root.path().join("o");
        let mcp_output = UnixListener::bind(&mcp_output_path).unwrap();
        let launch_fixture = crate::structured::runtime::tests::Fixture::for_socket();
        let profile = launch_fixture.profile("mcp-config", "2.1.296");
        let endpoint =
            crate::structured::launch::SessionMcp::new(&std::env::current_exe().unwrap(), &socket)
                .unwrap();
        profile.register_flock_mcp(Some(lease), &endpoint).unwrap();
        let config_index = profile
            .args()
            .iter()
            .position(|arg| arg == "--mcp-config")
            .unwrap();
        let mcp_config = &profile.args()[config_index + 1];
        let listener = UnixListener::bind(&socket).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let active = running.clone();
        let server = std::thread::spawn(move || {
            while active.load(Ordering::Acquire) {
                let (stream, _) = listener.accept().unwrap();
                if !active.load(Ordering::Acquire) {
                    break;
                }
                crate::api::server::handle_connection(
                    stream,
                    &tx,
                    &EventHub::default(),
                    &active,
                    None,
                    &Default::default(),
                    &policy,
                    &crate::api::client_listener::Health::default(),
                )
                .unwrap();
            }
        });
        let mut command = std::process::Command::new(crate::test_support::program_path("python3"));
        command
            .arg("-u")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/scoped-mcp/parent.py"
            ))
            .arg(std::env::current_exe().unwrap())
            .arg(mcp_config)
            .arg(&mcp_output_path)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("test PATH"))
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .env("XDG_STATE_HOME", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut parent = command.spawn().unwrap();
        let stdout = parent.stdout.take().unwrap();
        let (send, lines) = mpsc::unbounded_channel();
        let control_send = send.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line).map_err(|_| "invalid control stage".into())
                });
                if control_send.send(value).is_err() {
                    return;
                }
            }
            let _ = control_send.send(Ok(json!({"stage":"CONTROL_EOF"})));
        });
        std::thread::spawn(move || {
            let (output, _) = mcp_output.accept().unwrap();
            for line in BufReader::new(output).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        let _ = send.send(Err(error.to_string()));
                        return;
                    }
                };
                if line.trim().is_empty() || line.trim() == "running 1 test" {
                    continue;
                }
                let value = serde_json::from_str(&line).map_err(|_| "invalid MCP output".into());
                if send.send(value).is_err() {
                    return;
                }
            }
            let _ = send.send(Err("fixture stdout reached EOF".into()));
        });
        Self {
            parent,
            child: None,
            parent_dead: false,
            lines,
            _root: root,
            _profile: profile,
            _launch_fixture: launch_fixture,
            socket,
            running,
            server: Some(server),
        }
    }
    fn send(&mut self, stage: &str, request: Value) {
        let stdin = self.parent.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"stage":stage,"request":request})).unwrap();
        stdin.flush().unwrap();
    }
    async fn wait(&mut self, app: &mut App, key: &str, want: Value) -> Value {
        tokio::time::timeout(CEILING,async {
            loop {tokio::select! {
                line=self.lines.recv()=>{
                    let value=line.expect("fixture pipe disconnected").expect("fixture stage failed");
                    if self.parent_dead && value["stage"] == "CONTROL_EOF" {continue;}
                    assert_eq!(value[key],want,"unexpected fixture message: {value}");
                    return value;
                }
                msg=app.api_rx.recv()=>{app.handle_api_request_message(msg.expect("API disconnected"));}
            }}
        }).await.expect("fixture stage must arrive within fail ceiling")
    }
    fn resume(&self) {
        let (pid, birth) = self.child.unwrap();
        assert_eq!(
            platform::process_start_time(pid),
            Some(birth),
            "owned child birth changed"
        );
        let status = std::process::Command::new(crate::test_support::program_path("python3"))
            .args([
                "-c",
                "import os,signal,sys; os.kill(int(sys.argv[1]),signal.SIGCONT)",
                &pid.to_string(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }
    fn kill_parent(&mut self) {
        self.parent.kill().unwrap();
        self.parent_dead = true;
        assert!(!self.parent.wait().unwrap().success());
        assert!(platform::live_process_facts(self.parent.id()).is_none());
    }
}
fn tool(id: &str, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}
pub(super) async fn next_effect(app: &mut App) -> ApiRequestMessage {
    tokio::time::timeout(CEILING, async {
        loop {
            let msg = app
                .api_rx
                .recv()
                .await
                .expect("API disconnected before effect barrier");
            if !app.respond_scope_probe(&msg) {
                return msg;
            }
        }
    })
    .await
    .expect("effect admission barrier must arrive within fail ceiling")
}
async fn orphan_case(prior: bool, released: bool, queued: bool) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(&Config::default(), true, None, rx, EventHub::default());
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("fixture")];
    app.state.ensure_test_terminals();
    let pane = app
        .state
        .public_pane_id(0, app.state.workspaces[0].focused_pane_id().unwrap())
        .unwrap();
    let (a, _b) = UnixStream::pair().unwrap();
    let uid = platform::peer::credentials(&a).uid.unwrap();
    let policy = UidPolicy {
        operator: uid,
        dedicated: Ok(None).into(),
    };
    app.runtime_config.uid_policy = policy.clone();
    app.session_leases = SessionLeases::default();
    let mut lease = Some(
        app.session_leases
            .reserve(
                "fixture-agent",
                "00000000-0000-4000-8000-000000000001",
                None,
            )
            .unwrap(),
    );
    let mut fixture = Fixture::new(lease.as_ref().unwrap(), tx, policy);
    let child = fixture.wait(&mut app, "stage", json!("CHILD")).await["pid"]
        .as_u64()
        .unwrap() as u32;
    fixture.child = Some((child, platform::process_start_time(child).unwrap()));
    lease.as_mut().unwrap().bind_test_peer(fixture.parent.id());
    fixture.send(
        "SEND",
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{}}),
    );
    fixture.wait(&mut app, "id", json!("init")).await;
    if prior {
        fixture.send("SEND", tool("prior", "flock_agent_list", json!({})));
        let result = fixture.wait(&mut app, "id", json!("prior")).await;
        assert!(
            result.get("error").is_none(),
            "live lease call refused: {result}"
        );
    }
    fixture.send(
        "BUFFER",
        tool(
            "effect",
            "flock_msg_mute",
            json!({"pane":pane,"seconds":600}),
        ),
    );
    fixture.wait(&mut app, "stage", json!("QUEUED")).await;
    let pending = if queued {
        fixture.resume();
        Some(next_effect(&mut app).await)
    } else {
        None
    };
    fixture.kill_parent();
    if released {
        drop(lease.take());
    }
    if let Some(pending) = pending {
        app.handle_api_request_message(pending);
    } else {
        fixture.resume();
    }
    let result = fixture.wait(&mut app, "id", json!("effect")).await;
    assert!(
        app.mailboxes
            .muted_until(&pane, super::messages::now_ms())
            .is_none(),
        "orphan changed the mailbox: {result}"
    );
    assert_eq!(
        result["error"]["data"]["refusal"], "not_authorized",
        "orphan must refuse: {result}"
    );
    assert!(!app.state.should_quit);
    tokio::time::timeout(CEILING, async {
        loop {
            match fixture.lines.recv().await.expect("fixture channel ended") {
                Ok(value) => assert_eq!(
                    value["stage"], "CONTROL_EOF",
                    "unexpected trailing message: {value}"
                ),
                Err(error) => {
                    assert_eq!(error, "fixture stdout reached EOF");
                    break;
                }
            }
        }
    })
    .await
    .expect("MCP EOF must arrive within fail ceiling");
}

#[tokio::test]
async fn buffered_orphan_refuses_with_retained_dead_root_and_empty_table() {
    for prior in [false, true] {
        for released in [false, true] {
            orphan_case(prior, released, false).await;
        }
    }
}
#[tokio::test]
async fn queued_mcp_request_rechecks_at_effect_barrier_after_parent_death() {
    for released in [false, true] {
        orphan_case(false, released, true).await;
    }
}

#[tokio::test]
async fn attached_scoped_feed_closes_on_revocation() {
    use crate::api::{
        client::ApiClient,
        schema::{EventsSubscribeParams, Subscription},
    };
    let (leases, lease, _scope, peer) = crate::api::scoped::tests::live_scope();
    let root = crate::test_support::TempDir::socket();
    let path = root.path().join("s");
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(&Config::default(), true, None, rx, EventHub::default());
    app.session_leases = leases;
    let policy = UidPolicy {
        operator: peer.uid.unwrap(),
        dedicated: Ok(None).into(),
    };
    app.runtime_config.uid_policy = policy.clone();
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        crate::api::server::handle_connection(
            socket,
            &tx,
            &EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
            &Default::default(),
            &policy,
            &crate::api::client_listener::Health::default(),
        )
        .unwrap();
    });
    let client = ApiClient::scoped(path, lease.credential());
    let (attached, mut attachment) = tokio::sync::oneshot::channel();
    let (done, completed) = tokio::sync::oneshot::channel();
    let reader = std::thread::spawn(move || {
        let (ack, mut feed) = client
            .subscribe(
                "feed",
                EventsSubscribeParams {
                    after: None,
                    subscriptions: vec![Subscription::MsgQueued {
                        pane: "fixture-pane".into(),
                    }],
                },
                Some(CEILING),
            )
            .unwrap();
        assert!(matches!(
            ack.result,
            crate::api::schema::ResponseResult::SubscriptionStarted { .. }
        ));
        attached.send(()).unwrap();
        assert!(
            feed.next_value().unwrap().is_none(),
            "revoked feed must close before another event"
        );
        done.send(()).unwrap();
    });
    tokio::time::timeout(CEILING, async {
        loop {
            tokio::select! {
                ack=&mut attachment=>{ack.unwrap();break;}
                msg=app.api_rx.recv()=>{app.handle_api_request_message(msg.unwrap());}
            }
        }
    })
    .await
    .expect("feed attachment must arrive within fail ceiling");
    drop(lease);
    tokio::time::timeout(CEILING, completed)
        .await
        .expect("revoked feed must close within fail ceiling")
        .unwrap();
    reader.join().unwrap();
    server.join().unwrap();
}
