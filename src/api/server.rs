use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::debug;

#[cfg(test)]
use std::fs;

use crate::api::reply_wait::wait_for_reply;
use crate::api::schema::{
    ErrorBody, ErrorResponse, Method, Request, ResponseResult, ServerCapabilities, SuccessResponse,
};
use crate::api::subscriptions::ActiveSubscription;
use crate::api::wait::wait_for_output;
use crate::api::{request_changes_ui, socket_path, ApiRequestMessage, ApiRequestSender, EventHub};
use crate::ipc::{remove_socket_file_if_owned, socket_file_identity, SocketFileIdentity};

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
const ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(2);
const ACCEPT_UNKNOWN_ERROR_LIMIT: u64 = 5;

const SOCKET_PERMISSION_MODE: u32 = 0o600;
pub(super) const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const APP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_INITIAL_REQUEST_BYTES: usize = 1024 * 1024;

pub struct ServerHandle {
    _thread: std::thread::JoinHandle<()>,
    path: PathBuf,
    pub(crate) node_id: Option<String>,
    pub(crate) clone_detection_warning: Option<String>,
    identity: SocketFileIdentity,
    running: Arc<AtomicBool>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);

        if let Err(err) = self.remove_socket_file_if_owned() {
            if err.kind() != std::io::ErrorKind::NotFound {
                crate::logging::api_socket_remove_failed(&self.path, &err.to_string());
            }
        }
    }
}

impl ServerHandle {
    pub(crate) fn remove_socket_file_if_owned(&self) -> std::io::Result<()> {
        remove_socket_file_if_owned(&self.path, self.identity)
    }
}

pub fn start_server(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
) -> std::io::Result<ServerHandle> {
    let identity = crate::mesh::identity::NodeIdentity::load()?;
    if let Some(warning) = &identity.clone_detection_warning {
        crate::logging::node_clone_detection_unavailable(warning);
    }
    start_server_with_capabilities(
        api_tx,
        event_hub,
        Some(ServerCapabilities {
            live_handoff: true,
            node_id: Some(identity.node_id()),
            clone_detection_warning: identity.clone_detection_warning.clone(),
        }),
    )
}

pub fn start_server_with_capabilities(
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    capabilities: Option<ServerCapabilities>,
) -> std::io::Result<ServerHandle> {
    let path = socket_path();
    prepare_socket_path(&path)?;

    let listener = UnixListener::bind(&path)?;
    restrict_socket_permissions(&path)?;
    let identity = socket_file_identity(&path)?;
    crate::logging::api_server_listening(&path);

    let running = Arc::new(AtomicBool::new(true));
    let listener_running = Arc::clone(&running);
    let node_id = capabilities.as_ref().and_then(|caps| caps.node_id.clone());
    let clone_detection_warning = capabilities
        .as_ref()
        .and_then(|caps| caps.clone_detection_warning.clone());
    let thread = std::thread::spawn(move || {
        let health = Arc::new(Mutex::new(crate::api::schema::ApiListenerHealth::default()));
        run_accept_loop(
            || listener.accept().map(|(stream, _)| stream),
            &listener_running,
            &health,
            |stream| {
                let api_tx = api_tx.clone();
                let event_hub = event_hub.clone();
                let capabilities = capabilities.clone();
                let connection_running = Arc::clone(&listener_running);
                let health = Arc::clone(&health);
                std::thread::spawn(move || {
                    if let Err(err) = handle_connection(
                        stream,
                        &api_tx,
                        &event_hub,
                        &connection_running,
                        capabilities,
                        &health,
                    ) {
                        crate::logging::api_connection_failed(&err.to_string());
                    }
                });
            },
        );
        debug!("api server thread exiting");
    });

    Ok(ServerHandle {
        _thread: thread,
        node_id,
        clone_detection_warning,
        path,
        identity,
        running,
    })
}

fn run_accept_loop<T>(
    accept: impl FnMut() -> io::Result<T>,
    running: &AtomicBool,
    health: &Mutex<crate::api::schema::ApiListenerHealth>,
    serve: impl FnMut(T),
) {
    run_accept_loop_with_sleep(accept, running, health, serve, std::thread::sleep);
}

fn run_accept_loop_with_sleep<T>(
    mut accept: impl FnMut() -> io::Result<T>,
    running: &AtomicBool,
    health: &Mutex<crate::api::schema::ApiListenerHealth>,
    mut serve: impl FnMut(T),
    mut sleep: impl FnMut(Duration),
) {
    let mut failures = 0u64;
    let mut backoff = ACCEPT_ERROR_BACKOFF;
    while running.load(Ordering::Relaxed) {
        match accept() {
            Ok(stream) => {
                if failures > 0 {
                    crate::logging::api_listener_accept_recovered(failures);
                }
                failures = 0;
                backoff = ACCEPT_ERROR_BACKOFF;
                serve(stream);
            }
            Err(err) => {
                failures = failures.saturating_add(1);
                let fatal = accept_error_is_fatal(&err, failures);
                let mut status = health
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                status.accept_errors = status.accept_errors.saturating_add(1);
                status.last_accept_error = Some(err.to_string());
                status.stopped = fatal;
                drop(status);
                if fatal {
                    crate::logging::api_listener_accept_failed(&err.to_string(), failures, true);
                    break;
                }
                if failures == 1 {
                    crate::logging::api_listener_accept_failed(&err.to_string(), failures, false);
                }
                sleep(backoff);
                backoff = (backoff * 2).min(ACCEPT_ERROR_MAX_BACKOFF);
            }
        }
    }
}

fn accept_error_is_fatal(err: &io::Error, failures: u64) -> bool {
    match err.raw_os_error() {
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK) => true,
        Some(
            libc::EMFILE
            | libc::ENFILE
            | libc::ECONNABORTED
            | libc::EINTR
            | libc::EAGAIN
            | libc::ENOBUFS
            | libc::ENOMEM,
        ) => false,
        _ if matches!(
            err.kind(),
            io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::ConnectionAborted
        ) =>
        {
            false
        }
        _ => failures >= ACCEPT_UNKNOWN_ERROR_LIMIT,
    }
}

fn prepare_socket_path(path: &Path) -> std::io::Result<()> {
    crate::ipc::prepare_socket_path(path, |path| {
        format!("flk is already running (socket busy at {})", path.display())
    })
}

fn restrict_socket_permissions(path: &Path) -> std::io::Result<()> {
    crate::ipc::restrict_socket_permissions(path, SOCKET_PERMISSION_MODE)
}

fn handle_connection(
    mut stream: UnixStream,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
    capabilities: Option<ServerCapabilities>,
    listener_health: &Mutex<crate::api::schema::ApiListenerHealth>,
) -> std::io::Result<()> {
    let peer_pid = socket_peer_pid(&stream);
    if let Err(err) = stream.set_write_timeout(Some(STREAM_WRITE_TIMEOUT)) {
        crate::logging::api_connection_write_timeout_unavailable(&err.to_string());
    }

    let Some(line) = read_initial_request_line(&mut stream)? else {
        return Ok(());
    };

    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let request = match serde_json::from_str::<Request>(line) {
        Ok(request) => request,
        Err(err) => {
            write_json_line_allow_disconnect(
                &mut stream,
                &ErrorResponse {
                    id: String::new(),
                    error: ErrorBody {
                        code: "invalid_request".into(),
                        message: format!("invalid request: {err}"),
                    },
                },
            )?;
            return Ok(());
        }
    };

    let request_id = request.id.clone();
    let method = api_method_name(&request.method);
    let changes_ui = request_changes_ui(&request);
    crate::logging::api_request_started(&request_id, method, changes_ui);

    match request.method {
        Method::EventsSubscribe(params) => {
            let result = stream_subscriptions(
                stream,
                request_id.clone(),
                params,
                api_tx,
                event_hub,
                running,
            );
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "stream_closed",
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        Method::MsgWaitReply(params) => {
            let Some(response) =
                wait_for_reply(request_id.clone(), params, &mut stream, event_hub, running)?
            else {
                crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "client_disconnected",
                    changes_ui,
                );
                return Ok(());
            };
            let result = write_text_line_allow_disconnect(&mut stream, &response);
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    api_response_outcome(&response),
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        Method::PaneWaitForOutput(params) => {
            let Some(response) =
                wait_for_output(request_id.clone(), params, &mut stream, api_tx, running)?
            else {
                crate::logging::api_request_completed(
                    &request_id,
                    method,
                    "client_disconnected",
                    changes_ui,
                );
                return Ok(());
            };
            let result = write_text_line_allow_disconnect(&mut stream, &response);
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    api_response_outcome(&response),
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
        method_body => {
            let response = handle_request(
                Request {
                    id: request_id.clone(),
                    method: method_body,
                },
                api_tx,
                capabilities,
                peer_pid,
                listener_health,
            );
            let result = write_text_line_allow_disconnect(&mut stream, &response);
            match &result {
                Ok(()) => crate::logging::api_request_completed(
                    &request_id,
                    method,
                    api_response_outcome(&response),
                    changes_ui,
                ),
                Err(err) => {
                    crate::logging::api_request_failed(&request_id, method, &err.to_string())
                }
            }
            result
        }
    }
}

/// PID of the process at the other end of the unix socket. macOS exposes it
/// via LOCAL_PEERPID; Linux via SO_PEERCRED. None when unavailable.
pub(crate) fn socket_peer_pid(stream: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SOL_LOCAL = 0, LOCAL_PEERPID = 2 (sys/un.h)
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                0,
                2,
                &mut pid as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        return (rc == 0 && pid > 0).then_some(pid as u32);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        return (rc == 0 && cred.pid > 0).then_some(cred.pid as u32);
    }
    #[allow(unreachable_code)]
    {
        let _ = stream;
        None
    }
}

fn handle_request(
    request: Request,
    api_tx: &ApiRequestSender,
    capabilities: Option<ServerCapabilities>,
    peer_pid: Option<u32>,
    listener_health: &Mutex<crate::api::schema::ApiListenerHealth>,
) -> String {
    match request.method {
        Method::Ping(_) => serde_json::to_string(&SuccessResponse {
            id: request.id,
            result: ResponseResult::Pong {
                version: crate::build_info::version(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities,
                // The server's own confirmed verdict, mirrored process-wide (#426).
                // Not a second probe: this task cannot reach the App's core,
                // and an undebounced second opinion here could say `Broken` a
                // full reading before the banner is allowed to — the two
                // surfaces disagreeing about the same fault is worse than a
                // reading that is up to one TTL behind. Nothing blocks here at
                // all, which matters because a blocking `getpwuid` on an
                // unavailable opendirectoryd would hang `flk status` for
                // exactly as long as the fault lasts.
                session_health: Some(crate::health::confirmed()),
                api_listener: Some(
                    listener_health
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                ),
            },
        })
        .unwrap_or_else(|_| {
            r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
                .to_string()
        }),
        _ => dispatch_to_app(request, api_tx, peer_pid),
    }
}

/// Wire name for a method, for the request log line.
///
/// This restates the `#[serde(rename = ...)]` on each `Method` variant, which
/// looks like the kind of duplication that drifts — an audit flagged it as
/// exactly that. It does not, and the reason is worth writing down so the next
/// audit doesn't re-litigate it: **the match has no catch-all**, so adding a
/// variant fails to compile until its arm exists (verified: adding a probe
/// variant errors with "non-exhaustive patterns"). The compiler enforces the
/// one dimension that matters — the set.
///
/// What remains is a typo in a single arm, which mislabels one log field.
/// Deriving the name from serde instead would mean either serializing the
/// params on every request just to read the tag, or ~60 lines of custom
/// `Serializer` to capture it without them. Both are worse than a
/// compiler-checked table for a logging label, so this stays deliberate.
fn api_method_name(method: &Method) -> &'static str {
    match method {
        Method::Ping(_) => "ping",
        Method::ServerStop(_) => "server.stop",
        Method::ServerLiveHandoff(_) => "server.live_handoff",
        Method::ServerReloadConfig(_) => "server.reload_config",
        Method::NotificationShow(_) => "notification.show",
        Method::NotificationList(_) => "notification.list",
        Method::NotificationAck(_) => "notification.ack",
        Method::HandoffList(_) => "handoff.list",
        Method::HandoffRead(_) => "handoff.read",
        Method::WorkspaceCreate(_) => "workspace.create",
        Method::WorkspaceList(_) => "workspace.list",
        Method::WorkspaceGet(_) => "workspace.get",
        Method::WorkspaceFocus(_) => "workspace.focus",
        Method::WorkspaceRename(_) => "workspace.rename",
        Method::WorkspaceClose(_) => "workspace.close",
        Method::WorktreeList(_) => "worktree.list",
        Method::WorktreeCreate(_) => "worktree.create",
        Method::WorktreeOpen(_) => "worktree.open",
        Method::WorktreeRemove(_) => "worktree.remove",
        Method::WorktreeKill(_) => "worktree.kill",
        Method::TabCreate(_) => "tab.create",
        Method::TabList(_) => "tab.list",
        Method::TabGet(_) => "tab.get",
        Method::TabFocus(_) => "tab.focus",
        Method::TabRename(_) => "tab.rename",
        Method::TabClose(_) => "tab.close",
        Method::PeersSummary(_) => "peers.summary",
        Method::PeersHubFleet(_) => "peers.hub_fleet",
        Method::PeersCheckoutPrepare(_) => "peers.checkout_prepare",
        Method::AgentList(_) => "agent.list",
        Method::AgentGet(_) => "agent.get",
        Method::AgentRead(_) => "agent.read",
        Method::AgentHistory(_) => "agent.history",
        Method::AgentResult(_) => "agent.result",
        Method::AgentSend(_) => "agent.send",
        Method::AgentRename(_) => "agent.rename",
        Method::AgentFocus(_) => "agent.focus",
        Method::AgentStart(_) => "agent.start",
        Method::AgentFork(_) => "agent.fork",
        Method::AgentSpawn(_) => "agent.spawn",
        Method::AgentHibernate(_) => "agent.hibernate",
        Method::AgentResume(_) => "agent.resume",
        Method::AgentRestart(_) => "agent.restart",
        Method::AgentLineage(_) => "agent.lineage",
        Method::MsgSend(_) => "msg.send",
        Method::MsgReply(_) => "msg.reply",
        Method::MsgList(_) => "msg.list",
        Method::MsgRead(_) => "msg.read",
        Method::MsgStatus(_) => "msg.status",
        Method::MsgWaitReply(_) => "msg.wait_reply",
        Method::MsgWake(_) => "msg.wake",
        Method::MsgMute(_) => "msg.mute",
        Method::MsgUplinkTake(_) => "msg.uplink_take",
        Method::MeshHello(_) => "mesh.hello",
        Method::PeersEnrollReset(_) => "peers.enroll_reset",
        Method::PeersEnrollment(_) => "peers.enrollment",
        Method::PeersRelayAttach(_) => "peers.relay_attach",
        Method::MsgUplinkResult(_) => "msg.uplink_result",
        Method::PaneSplit(_) => "pane.split",
        Method::PaneMove(_) => "pane.move",
        Method::PaneList(_) => "pane.list",
        Method::PaneGet(_) => "pane.get",
        Method::PaneRename(_) => "pane.rename",
        Method::PaneSendText(_) => "pane.send_text",
        Method::PaneSendKeys(_) => "pane.send_keys",
        Method::PaneSendInput(_) => "pane.send_input",
        Method::PaneSubmit(_) => "pane.submit",
        Method::PaneArmSelfCompact(_) => "pane.arm_self_compact",
        Method::PaneRead(_) => "pane.read",
        Method::PaneReportAgent(_) => "pane.report_agent",
        Method::PaneReportAgentSession(_) => "pane.report_agent_session",
        Method::PaneReportPrompt(_) => "pane.report_prompt",
        Method::PaneReportRecap(_) => "pane.report_recap",
        Method::PaneReportReply(_) => "pane.report_reply",
        Method::PaneReportMetadata(_) => "pane.report_metadata",
        Method::PaneSetHeaderField(_) => "pane.set_header_field",
        Method::PaneClearHeaderField(_) => "pane.clear_header_field",
        Method::PaneClearAgentAuthority(_) => "pane.clear_agent_authority",
        Method::PaneReleaseAgent(_) => "pane.release_agent",
        Method::PaneClose(_) => "pane.close",
        Method::EventsSubscribe(_) => "events.subscribe",
        Method::EventsWait(_) => "events.wait",
        Method::PaneWaitForOutput(_) => "pane.wait_for_output",
        Method::IntegrationInstall(_) => "integration.install",
        Method::IntegrationUninstall(_) => "integration.uninstall",
        Method::ChecksList(_) => "checks.list",
        Method::ChecksAck(_) => "checks.ack",
        Method::ChecksRun(_) => "checks.run",
        Method::DigestRender(_) => "digest.render",
        Method::FleetPause(_) => "fleet.pause",
        Method::FleetResume(_) => "fleet.resume",
        Method::FleetStatus(_) => "fleet.status",
        Method::RevertRun(_) => "revert.run",
    }
}

fn api_response_outcome(response: &str) -> &'static str {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(response) else {
        return "error";
    };

    match value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_str())
    {
        Some("timeout") => "timeout",
        Some(_) => "error",
        None => "ok",
    }
}

fn read_initial_request_line(stream: &mut UnixStream) -> std::io::Result<Option<String>> {
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + INITIAL_REQUEST_TIMEOUT;
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                stream.set_nonblocking(false)?;
                return Ok(None);
            }
            Ok(_) => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    stream.set_nonblocking(false)?;
                    return String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
                }
                if bytes.len() > MAX_INITIAL_REQUEST_BYTES {
                    stream.set_nonblocking(false)?;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "api request line is too large",
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    stream.set_nonblocking(false)?;
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out reading api request",
                    ));
                }
                std::thread::sleep(CONNECTION_POLL_INTERVAL);
            }
            Err(err) => {
                stream.set_nonblocking(false)?;
                return Err(err);
            }
        }
    }
}

fn stream_subscriptions(
    mut stream: UnixStream,
    request_id: String,
    params: crate::api::schema::EventsSubscribeParams,
    api_tx: &ApiRequestSender,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut subscriptions = Vec::with_capacity(params.subscriptions.len());
    for (index, subscription) in params.subscriptions.into_iter().enumerate() {
        let active =
            match ActiveSubscription::new(subscription, &request_id, index, api_tx, event_hub) {
                Ok(active) => active,
                Err(response) => {
                    if let Err(err) = write_json_line(&mut stream, &response) {
                        if is_connection_closed_error(&err) {
                            return Ok(());
                        }
                        return Err(err);
                    }
                    return Ok(());
                }
            };
        subscriptions.push(active);
    }

    if let Err(err) = write_json_line(
        &mut stream,
        &SuccessResponse {
            id: request_id,
            result: ResponseResult::SubscriptionStarted {},
        },
    ) {
        if is_connection_closed_error(&err) {
            return Ok(());
        }
        return Err(err);
    }

    loop {
        if should_stop_connection(&mut stream, running)? {
            return Ok(());
        }

        let mut wrote = false;
        for subscription in &mut subscriptions {
            if let Some(event) = subscription.poll(api_tx, event_hub) {
                wrote = true;
                if let Err(err) = write_json_line(&mut stream, &event) {
                    if is_connection_closed_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
        }
        // A stream made only of hub-driven subscriptions sleeps ON the hub
        // (#438): the push that queues a message wakes it, so delivery costs
        // a notify rather than up to a whole tick. The tick stays as the
        // bound on noticing a hung-up client or a stopping server. Any
        // subscription that re-reads pane state keeps the plain tick, and so
        // keeps its old pacing exactly.
        let cursor: Option<u64> = subscriptions
            .iter()
            .map(ActiveSubscription::hub_cursor)
            .collect::<Option<Vec<u64>>>()
            .and_then(|cursors| cursors.into_iter().min());
        match cursor {
            // Drain a burst before sleeping: one poll yields one event.
            Some(_) if wrote => {}
            Some(cursor) => {
                event_hub.wait_after(cursor, CONNECTION_POLL_INTERVAL);
            }
            None => std::thread::sleep(CONNECTION_POLL_INTERVAL),
        }
    }
}

fn write_text_line(stream: &mut UnixStream, value: &str) -> std::io::Result<()> {
    stream.write_all(value.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn write_text_line_allow_disconnect(stream: &mut UnixStream, value: &str) -> std::io::Result<()> {
    match write_text_line(stream, value) {
        Err(err) if is_connection_closed_error(&err) => Ok(()),
        result => result,
    }
}

fn write_json_line<T: serde::Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line(stream, &encoded)
}

fn write_json_line_allow_disconnect<T: serde::Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|err| std::io::Error::other(format!("failed to encode json: {err}")))?;
    write_text_line_allow_disconnect(stream, &encoded)
}

pub(super) fn should_stop_connection(
    stream: &mut UnixStream,
    running: &Arc<AtomicBool>,
) -> std::io::Result<bool> {
    if !running.load(Ordering::Relaxed) {
        return Ok(true);
    }

    probe_stream_closed(stream)
}

fn probe_stream_closed(stream: &mut UnixStream) -> std::io::Result<bool> {
    stream.set_nonblocking(true)?;
    let mut probe = [0u8; 1];
    let status = match stream.read(&mut probe) {
        Ok(0) => Ok(true),
        Ok(_) => Ok(true),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(err) if is_connection_closed_error(&err) => Ok(true),
        Err(err) => Err(err),
    };
    stream.set_nonblocking(false)?;
    status
}

fn is_connection_closed_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::WriteZero
    )
}

fn dispatch_to_app(request: Request, api_tx: &ApiRequestSender, peer_pid: Option<u32>) -> String {
    dispatch_to_app_with_timeout(request, api_tx, None, peer_pid)
}

pub(super) fn dispatch_to_app_with_timeout(
    request: Request,
    api_tx: &ApiRequestSender,
    timeout: Option<Duration>,
    peer_pid: Option<u32>,
) -> String {
    let request_id = request.id.clone();
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    if let Err(err) = api_tx.send(ApiRequestMessage {
        request,
        respond_to,
        peer_pid,
    }) {
        return error_response_json(
            request_id,
            "server_unavailable",
            format!("failed to dispatch request: {err}"),
        );
    }

    let response = match timeout {
        Some(timeout) => response_rx.recv_timeout(timeout).map_err(|err| match err {
            std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for app response after {} ms",
                    timeout.as_millis()
                ),
            ),
            std::sync::mpsc::RecvTimeoutError::Disconnected => std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "app response channel closed",
            ),
        }),
        None => response_rx
            .recv()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::BrokenPipe, err)),
    };

    match response {
        Ok(response) => response,
        Err(err) => error_response_json(
            request_id,
            "server_unavailable",
            format!("request handling failed: {err}"),
        ),
    }
}

fn error_response_json(id: String, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
    .unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode error response"}}"#
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use tokio::sync::mpsc;

    #[test]
    fn accept_error_then_success_serves_ping_and_reports_health() {
        let path = unique_test_path("accept");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .write_all(b"{\"id\":\"retry\",\"method\":\"ping\",\"params\":{}}\n")
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let running = AtomicBool::new(true);
        let connection_running = Arc::new(AtomicBool::new(true));
        let health = Mutex::new(crate::api::schema::ApiListenerHealth::default());
        let (tx, _rx) = mpsc::unbounded_channel();
        let hub = EventHub::default();
        let mut fail = true;
        let started = Instant::now();
        run_accept_loop(
            || {
                if std::mem::take(&mut fail) {
                    Err(io::Error::from_raw_os_error(libc::EMFILE))
                } else {
                    listener.accept().map(|(stream, _)| stream)
                }
            },
            &running,
            &health,
            |stream| {
                handle_connection(stream, &tx, &hub, &connection_running, None, &health).unwrap();
                running.store(false, Ordering::Relaxed);
            },
        );
        assert!(started.elapsed() >= ACCEPT_ERROR_BACKOFF);
        let response: serde_json::Value = serde_json::from_str(&read_line(&mut client)).unwrap();
        assert_eq!(response["id"], "retry");
        assert_eq!(response["result"]["type"], "pong");
        assert_eq!(response["result"]["api_listener"]["accept_errors"], 1);
        assert_eq!(
            response["result"]["api_listener"]["last_accept_error"],
            io::Error::from_raw_os_error(libc::EMFILE).to_string()
        );
        drop(listener);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn repeated_accept_errors_back_off_and_observe_shutdown() {
        let running = AtomicBool::new(true);
        let health = Mutex::new(crate::api::schema::ApiListenerHealth::default());
        let mut attempts = 0;
        let started = Instant::now();
        run_accept_loop(
            || {
                attempts += 1;
                if attempts == 2 {
                    running.store(false, Ordering::Relaxed);
                }
                Err::<(), _>(io::Error::other("injected accept failure"))
            },
            &running,
            &health,
            |_| panic!("failed accepts must not dispatch a connection"),
        );
        assert_eq!(attempts, 2);
        assert!(started.elapsed() >= ACCEPT_ERROR_BACKOFF * 2);
        assert_eq!(health.lock().unwrap().accept_errors, 2);
    }

    #[test]
    fn transient_accept_errors_grow_cap_and_reset_backoff_without_log_flood() {
        let running = AtomicBool::new(true);
        let health = Mutex::new(crate::api::schema::ApiListenerHealth::default());
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let mut served = 0;
        let logs = crate::logging::capture_logs(|| {
            run_accept_loop_with_sleep(
                || {
                    attempts += 1;
                    if attempts == 9 || attempts == 11 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(libc::EMFILE))
                    }
                },
                &running,
                &health,
                |_| {
                    served += 1;
                    if served == 2 {
                        running.store(false, Ordering::Relaxed);
                    }
                },
                |duration| sleeps.push(duration),
            );
        });
        assert_eq!(
            sleeps,
            [50, 100, 200, 400, 800, 1600, 2000, 2000, 50].map(Duration::from_millis)
        );
        assert_eq!(health.lock().unwrap().accept_errors, 9);
        assert!(!health.lock().unwrap().stopped);
        assert_eq!(
            logs.matches("event=\"api.listener.accept\"").count(),
            4,
            "{logs}"
        );
        assert_eq!(logs.matches("WARN").count(), 2, "{logs}");
        assert_eq!(logs.matches("INFO").count(), 2, "{logs}");
        assert!(!logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("failures=8"), "{logs}");
    }

    #[test]
    fn fatal_accept_errors_stop_once_and_surface_in_ping() {
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            let running = AtomicBool::new(true);
            let health = Mutex::new(crate::api::schema::ApiListenerHealth::default());
            let mut attempts = 0;
            let logs = crate::logging::capture_logs(|| {
                run_accept_loop_with_sleep(
                    || {
                        attempts += 1;
                        Err::<(), _>(io::Error::from_raw_os_error(code))
                    },
                    &running,
                    &health,
                    |_| panic!("fatal accepts cannot serve"),
                    |_| panic!("fatal accepts cannot retry"),
                );
            });
            assert_eq!(attempts, 1);
            assert_eq!(logs.matches("ERROR").count(), 1, "{logs}");
            assert_eq!(
                logs.matches("event=\"api.listener.accept\"").count(),
                1,
                "{logs}"
            );
            let (tx, _rx) = mpsc::unbounded_channel();
            let response = handle_request(
                Request {
                    id: "fatal-status".into(),
                    method: Method::Ping(crate::api::schema::PingParams::default()),
                },
                &tx,
                None,
                None,
                &health,
            );
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["result"]["api_listener"]["stopped"], true);
            assert_eq!(response["result"]["api_listener"]["accept_errors"], 1);
            assert_eq!(
                response["result"]["api_listener"]["last_accept_error"],
                io::Error::from_raw_os_error(code).to_string()
            );
        }
    }

    #[test]
    fn unclassified_accept_errors_stop_after_bounded_failures() {
        let running = AtomicBool::new(true);
        let health = Mutex::new(crate::api::schema::ApiListenerHealth::default());
        let mut sleeps = Vec::new();
        let logs = crate::logging::capture_logs(|| {
            run_accept_loop_with_sleep(
                || Err::<(), _>(io::Error::other("injected unknown accept error")),
                &running,
                &health,
                |_| panic!("failed accepts cannot serve"),
                |duration| sleeps.push(duration),
            );
        });
        assert_eq!(sleeps.len() as u64, ACCEPT_UNKNOWN_ERROR_LIMIT - 1);
        assert_eq!(
            health.lock().unwrap().accept_errors,
            ACCEPT_UNKNOWN_ERROR_LIMIT
        );
        assert!(health.lock().unwrap().stopped);
        assert_eq!(logs.matches("WARN").count(), 1, "{logs}");
        assert_eq!(logs.matches("ERROR").count(), 1, "{logs}");
    }

    #[test]
    fn documented_transient_accept_errors_keep_retrying() {
        for code in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EAGAIN,
            libc::ENOBUFS,
            libc::ENOMEM,
        ] {
            assert!(!accept_error_is_fatal(
                &io::Error::from_raw_os_error(code),
                100
            ));
        }
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted] {
            assert!(!accept_error_is_fatal(&io::Error::from(kind), 100));
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_test_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flock-{name}-{}-{nanos}", std::process::id()))
    }

    fn read_line(stream: &mut UnixStream) -> String {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn socket_path_prefers_explicit_env_override() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unique = format!("/tmp/flock-test-{}.sock", std::process::id());
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &unique);
        assert_eq!(socket_path(), PathBuf::from(&unique));
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
    }

    #[test]
    fn socket_path_defaults_to_config_dir_even_when_xdg_runtime_dir_is_set() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config_home = unique_test_path("socket-default-config-home");
        let runtime_dir = unique_test_path("socket-default-runtime");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("flock.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_RUNTIME_DIR");
    }

    #[test]
    fn socket_path_uses_named_session_dir() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config_home = unique_test_path("socket-named-config-home");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        std::env::set_var(crate::session::SESSION_ENV_VAR, "work");
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        let expected = config_home
            .join(crate::config::app_dir_name())
            .join("sessions")
            .join("work")
            .join("flock.sock");
        assert_eq!(socket_path(), expected);

        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn restrict_socket_permissions_sets_user_only_mode() {
        // SUN_LEN caps unix-socket paths at ~104 bytes on macOS. unique_test_path
        // honors TMPDIR, which under `nix develop` gains a nix-shell.XXXXXX
        // segment (~62 bytes before our name) — the bind then fails with
        // "path must be shorter than SUN_LEN". Bind under /tmp with a short
        // unique name instead; only this test actually binds a socket there.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from("/tmp").join(format!("flock-sp-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        restrict_socket_permissions(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_PERMISSION_MODE);

        drop(_listener);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_response_outcome_uses_top_level_error_shape() {
        let ok_with_error_text = r#"{"id":"req","result":{"read":{"text":"user said \"error\": \"timeout\"","revision":1}}}"#;
        assert_eq!(api_response_outcome(ok_with_error_text), "ok");

        let timeout = r#"{"id":"req","error":{"code":"timeout","message":"timed out waiting for output match"}}"#;
        assert_eq!(api_response_outcome(timeout), "timeout");

        let generic_error =
            r#"{"id":"req","error":{"code":"server_unavailable","message":"boom"}}"#;
        assert_eq!(api_response_outcome(generic_error), "error");
    }

    #[test]
    fn ping_request_returns_pong() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let response = handle_request(
            Request {
                id: "req_1".into(),
                method: Method::Ping(crate::api::schema::PingParams::default()),
            },
            &tx,
            Some(ServerCapabilities {
                live_handoff: true,
                node_id: None,
                clone_detection_warning: None,
            }),
            None,
            &Mutex::default(),
        );

        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_1");
        assert!(matches!(parsed.result, ResponseResult::Pong { .. }));
    }

    #[test]
    fn request_dispatches_to_app_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let request = Request {
            id: "req_2".into(),
            method: Method::WorkspaceList(crate::api::schema::EmptyParams::default()),
        };

        let request_for_thread = request.clone();
        let thread = std::thread::spawn(move || {
            handle_request(request_for_thread, &tx, None, None, &Mutex::default())
        });

        let msg = rx.blocking_recv().unwrap();
        assert_eq!(msg.request.id, "req_2");
        msg.respond_to
            .send(
                serde_json::to_string(&SuccessResponse {
                    id: "req_2".into(),
                    result: ResponseResult::Ok {},
                })
                .unwrap(),
            )
            .unwrap();

        let response = thread.join().unwrap();
        let parsed: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed.id, "req_2");
    }

    #[test]
    fn wait_for_output_stops_when_client_disconnects() {
        let (api_tx, mut api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (first_read_tx, first_read_rx) = std::sync::mpsc::channel();
        let responder = std::thread::spawn(move || {
            let mut notified = false;
            while let Some(msg) = api_rx.blocking_recv() {
                assert!(matches!(msg.request.method, Method::PaneRead(_)));
                if !notified {
                    first_read_tx.send(()).unwrap();
                    notified = true;
                }
                msg.respond_to
                    .send(
                        serde_json::to_string(&SuccessResponse {
                            id: msg.request.id,
                            result: ResponseResult::PaneRead {
                                read: crate::api::schema::PaneReadResult {
                                    pane_id: "pane_1".into(),
                                    workspace_id: "ws_1".into(),
                                    tab_id: "tab_1".into(),
                                    source: crate::api::schema::ReadSource::RecentUnwrapped,
                                    format: crate::api::schema::ReadFormat::Text,
                                    text: String::new(),
                                    revision: 0,
                                    truncated: false,
                                },
                            },
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        });

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(br#"{"id":"req_wait","method":"pane.wait_for_output","params":{"pane_id":"pane_1","source":"recent","match":{"type":"substring","value":"never"}}}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &Mutex::default(),
            );
            done_tx.send(result).unwrap();
        });

        first_read_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());

        server_thread.join().unwrap();
        drop(running);
        responder.join().unwrap();
    }

    #[test]
    fn subscriptions_stop_when_client_disconnects() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(
                br#"{"id":"sub_1","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &Mutex::default(),
            );
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        drop(client);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }

    fn queued_for(to_pane: &str, correlation_id: &str) -> crate::api::schema::EventEnvelope {
        crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::MessageQueued,
            data: crate::api::schema::EventData::MessageQueued {
                correlation_id: correlation_id.into(),
                from_pane: Some("ws_1:p9".into()),
                from_agent: None,
                from_host: None,
                from_repo: None,
                to_pane: to_pane.into(),
                to_repo: None,
                cross_repo: false,
                in_reply_to: None,
                enqueued_at_ms: 1,
                intent: crate::api::schema::MsgIntent::NeedsReply,
                body: "hello".into(),
            },
        }
    }

    /// #438: the inbox feed reports only its own pane's mail, only mail that
    /// arrived after it attached, and a burst in full.
    #[test]
    fn a_msg_queued_subscription_streams_only_its_panes_new_mail() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(
                br#"{"id":"sub_msg","method":"events.subscribe","params":{"subscriptions":[{"type":"msg.queued","pane":"ws_1:p1"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        // Mail that was already there is the inbox's to report, not the feed's.
        event_hub.push(queued_for("ws_1:p1", "c-before"));
        let hub = event_hub.clone();
        let server_thread = std::thread::spawn(move || {
            handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &Mutex::default(),
            )
        });

        // ONE reader for the whole stream: a burst lands in a single read, and
        // a reader per line would swallow the second event into a buffer it
        // then drops — which hung this test. The timeout turns any future
        // miss into a failure instead of a hang.
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut next = || {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<serde_json::Value>(&line).unwrap()
        };
        assert_eq!(next()["result"]["type"], "subscription_started");

        hub.push(queued_for("ws_1:p2", "c-other"));
        hub.push(queued_for("ws_1:p1", "c-1"));
        hub.push(queued_for("ws_1:p1", "c-2"));
        let first = next();
        let second = next();
        assert_eq!(first["data"]["correlation_id"], "c-1");
        assert_eq!(second["data"]["correlation_id"], "c-2");
        assert_eq!(first["data"]["body"], "hello");

        running.store(false, Ordering::Relaxed);
        assert!(server_thread.join().unwrap().is_ok());
    }

    #[test]
    fn subscriptions_stop_when_server_shuts_down() {
        let (api_tx, _api_rx) = mpsc::unbounded_channel::<ApiRequestMessage>();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .write_all(
                br#"{"id":"sub_2","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"}]}}"#,
            )
            .unwrap();
        client.write_all(b"\n").unwrap();
        client.flush().unwrap();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = Arc::clone(&running);
        let event_hub = EventHub::default();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let result = handle_connection(
                server,
                &api_tx,
                &event_hub,
                &server_running,
                None,
                &Mutex::default(),
            );
            done_tx.send(result).unwrap();
        });

        let ack = read_line(&mut client);
        let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started");

        running.store(false, Ordering::Relaxed);

        let result = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(result.is_ok());
        server_thread.join().unwrap();
    }
}
