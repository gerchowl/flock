//! CLI launch, socket send and actual thin-client input against the owned fake worker.
use super::*;
use crate::{
    protocol::{self, ClientMessage, ServerMessage},
    server::headless::HeadlessServer,
    structured::{
        permissions::auth::tokens::app_sessions,
        view::contracts::{Action, Content},
    },
    structured_wire::PermissionDecision,
};
use std::{
    os::fd::AsRawFd,
    process::{Child, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
async fn public_send(owner: &mut AdmissionFixture, id: &str) {
    let socket = owner.socket.clone();
    let request = Request {
        id: "native-send".into(),
        method: Method::AgentSend(
            serde_json::from_value(json!({
                "target":id,"text":"native permission turn","submit":true
            }))
            .unwrap(),
        ),
    };
    let (done, mut result) = tokio::sync::oneshot::channel();
    let client = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        done.send(serde_json::from_str::<Value>(&response).unwrap())
            .unwrap();
    });
    let request = tokio::time::timeout(LIMIT, owner.app.api_rx.recv())
        .await
        .unwrap()
        .unwrap();
    owner.app.handle_api_request_message(request);
    let wake = owner.app.render_notify.clone();
    let sent = tokio::time::timeout(LIMIT, async {
        loop {
            let notified = wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            owner.app.service_structured_workers(Instant::now());
            tokio::select! {
                value = &mut result => break value.expect("send response producer died"),
                _ = notified => {},
            }
        }
    })
    .await
    .expect("durable send response deadline");
    client.join().unwrap();
    assert_eq!(
        sent["result"]["type"], "agent_queued",
        "public send: {sent}"
    );
}
#[derive(Default)]
struct Clock(AtomicU64);
impl app_sessions::Clock for Clock {
    fn now(&self) -> io::Result<Duration> {
        Ok(Duration::from_secs(self.0.load(Ordering::SeqCst)))
    }
    fn until(&self, _: Duration) -> app_sessions::Wait<'_> {
        Box::pin(std::future::pending())
    }
}
struct Fixture {
    server: HeadlessServer,
    owner: AdmissionFixture,
    clock: Arc<Clock>,
    id: String,
    generation: u64,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.native_test_swap_app(&mut self.owner.app);
    }
}
impl Fixture {
    async fn new() -> Self {
        let mut owner = AdmissionFixture::new("init-socket-null", 5000);
        let script = fs::read_to_string(owner.fixture.root.path().join("claude")).unwrap();
        owner.fixture.file(
            "claude",
            format!(
                "{}\n{}",
                script.lines().next().unwrap(),
                include_str!("../../../../../tests/fixtures/structured-public/claude.py")
            )
            .as_bytes(),
            0o700,
        );
        owner
            .fixture
            .file("native-permission-fixture", b"enabled", 0o600);
        let (id, generation) = owner.start().await;
        public_send(&mut owner, &id).await;
        let wake = owner.app.render_notify.clone();
        crate::structured::workers::tests::wait(&wake, || {
            owner.app.service_structured_workers(Instant::now());
            assert!(owner
                .app
                .structured_workers
                .report(&id, generation)
                .is_none());
            owner
                .app
                .structured_workers
                .runtime(&id, generation)
                .unwrap()
                .waiting_on_human()
                .is_ok_and(|requests| !requests.is_empty())
                && owner
                    .app
                    .state
                    .native_frames
                    .get(&id)
                    .is_some_and(|f| f.as_ref().is_ok_and(|s| !s.permissions.is_empty()))
        })
        .await;
        let clock = Arc::new(Clock::default());
        owner
            .app
            .client_auth
            .get(&crate::session::data_dir(), clock.clone())
            .unwrap();
        owner.app.state.mode = crate::app::Mode::Terminal;
        owner.app.state.mouse_capture = true;
        let server = HeadlessServer::native_test_server(&mut owner.app);
        Self {
            server,
            owner,
            clock,
            id,
            generation,
        }
    }
    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            version: protocol::PROTOCOL_VERSION,
            cols: 140,
            rows: 40,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: protocol::RenderEncoding::TerminalAnsi,
            keybindings: protocol::ClientKeybindings::Server,
            launch_mode: protocol::ClientLaunchMode::App,
            fleet: None,
            host_theme: None,
            notice: None,
        }
    }
    async fn attach(&mut self) -> UnixStream {
        self.attach_mode(protocol::ClientLaunchMode::App).await
    }
    async fn attach_mode(&mut self, mode: protocol::ClientLaunchMode) -> UnixStream {
        let mut stream = UnixStream::connect(self.server.native_test_socket()).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let mut hello = Self::hello();
        if let ClientMessage::Hello { launch_mode, .. } = &mut hello {
            *launch_mode = mode;
        }
        protocol::write_message(&mut stream, &hello).unwrap();
        self.server.native_test_accept();
        self.server.native_test_event().await;
        assert!(matches!(
            protocol::read_message::<_, ServerMessage>(&mut stream, protocol::MAX_FRAME_SIZE)
                .unwrap(),
            ServerMessage::Welcome { error: None, .. }
        ));
        stream
    }
    fn click_bytes(&mut self, decision: PermissionDecision) -> Vec<u8> {
        let app = self.server.native_test_app();
        app.service_structured_workers(Instant::now());
        crate::ui::compute_view_with_runtime_registry(
            &mut app.state,
            &app.terminal_runtimes,
            ratatui::layout::Rect::new(0, 0, 140, 40),
        );
        let pane = app
            .state
            .view
            .structured
            .values()
            .find_map(|p| p.pane.as_ref().ok())
            .expect("painted permission pane");
        let (x,y) = pane.positions().into_iter().find(|&(x,y)| matches!(pane.hit(x,y), Some(Action::Decide { decision:d,.. }) if *d == decision)).expect("painted decision control");
        format!("\x1b[<0;{};{}M", x + 1, y + 1).into_bytes()
    }
    async fn click(&mut self, stream: &mut UnixStream, decision: PermissionDecision) {
        let data = self.click_bytes(decision);
        protocol::write_message(stream, &ClientMessage::Input { data }).unwrap();
        self.server.native_test_event().await;
    }
    fn refused(&mut self, code: &str) {
        let app = self.server.native_test_app();
        assert_eq!(
            app.state.toast.as_ref().expect("typed refusal").context,
            code
        );
        assert!(app
            .structured_workers
            .runtime(&self.id, self.generation)
            .unwrap()
            .waiting_on_human()
            .is_ok_and(|requests| !requests.is_empty()));
    }
    async fn completed(&mut self, behavior: &str) {
        let app = self.server.native_test_app();
        assert!(
            app.state.toast.is_none(),
            "unexpected refusal: {:?}",
            app.state.toast.as_ref().map(|t| &t.context)
        );
        let wake = app.render_notify.clone();
        let expected = format!("native-decision:{behavior}");
        crate::structured::workers::tests::wait(&wake, || {
            app.service_structured_workers(Instant::now());
            assert!(app.structured_workers.report(&self.id,self.generation).is_none());
            app.state.native_frames[&self.id].as_ref().unwrap().transcript.iter().any(|card| {
                matches!(&card.content,Content::Text(t)|Content::Markdown(t) if t.contains(&expected))
            })
        }).await;
        assert!(app
            .structured_workers
            .runtime(&self.id, self.generation)
            .unwrap()
            .waiting_on_human()
            .is_ok_and(|requests| requests.is_empty()));
    }
}
#[tokio::test]
async fn s10e_public_native_allow_resumes_the_turn() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    f.click(&mut client, PermissionDecision::AllowOnce).await;
    f.completed("allow").await;
}
#[tokio::test]
async fn s10e_public_native_deny_is_reported_back() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    f.click(&mut client, PermissionDecision::Deny).await;
    f.completed("deny").await;
}
#[tokio::test]
async fn s10e_public_native_detach_refuses_later_input() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    let retained = f.server.native_test_app().native_test_probe(1);
    assert_eq!(retained(), Ok(()));
    protocol::write_message(&mut client, &ClientMessage::Detach).unwrap();
    f.server.native_test_event().await;
    // Bytes already queued after Detach may still reach the transport event loop.
    let data = f.click_bytes(PermissionDecision::AllowOnce);
    protocol::write_message(&mut client, &ClientMessage::Input { data }).unwrap();
    f.server.native_test_event().await;
    assert!(f
        .server
        .native_test_app()
        .structured_workers
        .runtime(&f.id, f.generation)
        .unwrap()
        .waiting_on_human()
        .is_ok_and(|requests| !requests.is_empty()));
    assert_eq!(
        retained(),
        Err(crate::api::error_code::ErrorCode::AuthRevoked)
    );
}
#[tokio::test]
async fn s10e_public_native_expired_token_is_refused() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    f.clock
        .0
        .store(app_sessions::TTL.as_secs(), Ordering::SeqCst);
    f.click(&mut client, PermissionDecision::AllowOnce).await;
    f.refused("auth_revoked");
}
#[tokio::test]
async fn s10e_public_native_always_requires_confirmation() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    f.click(&mut client, PermissionDecision::AllowAlways).await;
    f.refused("permission_decision_unsupported");
}

struct ClientProcess {
    child: Child,
    listener: std::os::unix::net::UnixListener,
    _root: crate::test_support::TempDir,
    stream: Option<UnixStream>,
}
impl ClientProcess {
    fn spawn(socket: &std::path::Path) -> Self {
        let root = crate::test_support::TempDir::socket();
        let path = root.path().join("return.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut command = crate::process::TracedCommand::new(
            std::env::current_exe().unwrap(),
            "native-client-fixture",
        );
        command
            .args([
                "--exact",
                "app::api::structured_start::tests::native_permissions::s10e_client_process",
                "--nocapture",
            ])
            .env("FLOCK_TEST_NATIVE_CLIENT", socket)
            .env("FLOCK_TEST_NATIVE_RETURN", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for key in ["FLOCK_SOCKET_PATH", "FLOCK_PANE_ID", "FLOCK_AGENT_ID"] {
            command.env_remove(key);
        }
        Self {
            child: command.spawn_traced().unwrap(),
            listener,
            _root: root,
            stream: None,
        }
    }
    fn ready(&mut self) {
        assert!(
            crate::platform::process_exit::tests::readable(self.listener.as_raw_fd(), LIMIT),
            "child did not complete attach"
        );
        let (socket, _) = self.listener.accept().unwrap();
        socket.set_read_timeout(Some(LIMIT)).unwrap();
        let fd = crate::server::handoff::recv_fds(&socket, 1)
            .unwrap()
            .pop()
            .unwrap();
        // SAFETY: recv_fds transfers sole ownership of the received descriptor.
        self.stream = Some(unsafe { <UnixStream as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    }
    fn exit(&mut self) {
        let pid = self.child.id();
        let watch =
            crate::platform::process_exit::ExitWatch::arm(crate::platform::peer::Credentials {
                pid: Some(pid),
                uid: Some(crate::platform::peer::effective_uid()),
                birth: crate::platform::process_start_time(pid),
            })
            .unwrap();
        drop(self.child.stdin.take());
        assert!(crate::platform::process_exit::tests::readable(
            watch.as_raw_fd(),
            LIMIT
        ));
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for ClientProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn s10e_client_process() {
    let Some(socket) = std::env::var_os("FLOCK_TEST_NATIVE_CLIENT") else {
        return;
    };
    let mut client = UnixStream::connect(socket).unwrap();
    protocol::write_message(&mut client, &Fixture::hello()).unwrap();
    assert!(matches!(
        protocol::read_message::<_, ServerMessage>(&mut client, protocol::MAX_FRAME_SIZE).unwrap(),
        ServerMessage::Welcome { error: None, .. }
    ));
    let return_socket =
        UnixStream::connect(std::env::var_os("FLOCK_TEST_NATIVE_RETURN").unwrap()).unwrap();
    crate::server::handoff::send_fds(&return_socket, &[client.as_raw_fd()]).unwrap();
    for line in io::stdin().lock().lines() {
        let data: Vec<u8> = serde_json::from_str(&line.unwrap()).unwrap();
        protocol::write_message(&mut client, &ClientMessage::Input { data }).unwrap();
    }
}

#[tokio::test]
async fn s10e_public_native_real_client_exit_refuses_a_held_socket() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut child = ClientProcess::spawn(f.server.native_test_socket());
    assert!(crate::platform::process_exit::tests::readable(
        f.server.native_test_listener_fd(),
        LIMIT
    ));
    f.server.native_test_accept();
    f.server.native_test_event().await;
    child.ready();
    child.exit();
    f.click(
        child.stream.as_mut().unwrap(),
        PermissionDecision::AllowOnce,
    )
    .await;
    f.refused("auth_revoked");
}
#[tokio::test]
async fn s10e_public_native_another_process_cannot_use_the_credential() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let _first = f.attach().await;
    let mut child = ClientProcess::spawn(f.server.native_test_socket());
    assert!(crate::platform::process_exit::tests::readable(
        f.server.native_test_listener_fd(),
        LIMIT
    ));
    f.server.native_test_accept();
    f.server.native_test_event().await;
    child.ready();
    // Fault injection substitutes only the credential, retaining the other
    // connection's genuine kernel peer. No bearer is ever sent over TUI wire.
    f.server.native_test_app().native_test_swap_session(2, 1);
    f.click(
        child.stream.as_mut().unwrap(),
        PermissionDecision::AllowOnce,
    )
    .await;
    f.refused("peer_process_changed");
}

#[tokio::test]
async fn s10e_public_native_answer_only_session_cannot_decide() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f.attach().await;
    f.server.native_test_app().native_test_answer_only(1);
    f.click(&mut client, PermissionDecision::Deny).await;
    f.refused("not_authorized");
}
#[tokio::test]
async fn s10e_public_native_direct_attach_without_capability_is_not_authorized() {
    let _guard = crate::config::test_config_env_guard();
    let mut f = Fixture::new().await;
    let mut client = f
        .attach_mode(protocol::ClientLaunchMode::TerminalAttach)
        .await;
    f.click(&mut client, PermissionDecision::AllowOnce).await;
    f.refused("not_authorized");
}
