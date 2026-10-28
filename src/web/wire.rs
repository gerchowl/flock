//! Server-owned, opt-in structured transport; no PTY or model dispatch.
mod commands;
mod fleet;
mod output;
mod subscriptions;
pub(crate) mod timing;
#[cfg(test)]
use crate::structured::permissions::auth::tokens::TokenStore;
use crate::{
    api::client_tokens::{Bearer, Scope},
    structured::permissions::auth::tokens::{
        server::{Owner, PeerAdmissionError, Session, Store},
        TokenError,
    },
    structured_wire::*,
};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::{extract::State, response::Response, routing::get, Extension, Router};
use futures_util::{FutureExt, Sink, SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::{
    collections::HashMap,
    future::Future,
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use timing::{Clock, Heartbeat, SystemClock};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
/// Maximum encoded, received or reassembled message bytes.
const MAX_MESSAGE: usize = 65_536;
/// Upgraded socket cap, including pending hellos.
const CONNECTIONS: u32 = 16;
/// Pending unauthenticated sockets; leave capacity for authenticated sessions.
const PENDING_HELLOS: usize = 4;
/// Registered sockets per authenticated bearer digest, including pending welcome.
const CREDENTIAL_CONNECTIONS: usize = 4;
/// Maximum queued control messages per socket.
const PRIORITY_CAPACITY: usize = 4;
/// Maximum queued data messages per socket.
const DATA_CAPACITY: usize = 8;
/// Maximum concurrent first-send commands on one socket.
const COMMANDS_PER_SOCKET: usize = 4;
/// Maximum buffered WebSocket output bytes.
const WRITE_BUFFER_BYTES: usize = MAX_MESSAGE * 2;
/// Failure ceiling for releasing all socket leases during shutdown.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// First-admission retention advertised in welcome, in milliseconds.
const IDEMPOTENCY_WINDOW_MS: u64 = 86_400_000;
/// Transport resource exhaustion or liveness failure.
const GOING_AWAY_CLOSE: u16 = 1001;
/// Binary or otherwise unsupported application operation.
const POLICY_VIOLATION_CLOSE: u16 = 1008;
/// Socket identity remains owned until teardown; later dispatch uses this verified session.
struct Client {
    session: Session,
    read_only: bool,
    data: mpsc::Sender<output::Delivery>,
    revoke: watch::Sender<bool>,
}
struct Shared {
    fleet: fleet::Client,
    histories: crate::structured::history::owner::Client,
    commands: commands::Client,
    store: Arc<Store>,
    epoch: String,
    clock: Arc<dyn Clock>,
    slots: Arc<Semaphore>,
    hellos: [Arc<Semaphore>; 2],
    admission: super::local_admission::Client,
    clients: Mutex<HashMap<u64, Client>>,
    next: AtomicU64,
    stop: watch::Sender<bool>,
}
impl Shared {
    fn revoke(&self, session: &Session) {
        if let Ok(clients) = self.clients.lock() {
            for client in clients.values() {
                if client.session.same_credential(session) {
                    let _ = client.revoke.send(true);
                }
            }
        }
    }
}
struct Lease {
    shared: Arc<Shared>,
    id: u64,
    live: Arc<AtomicBool>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut clients = self
            .shared
            .clients
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.live.store(false, Ordering::Release);
        clients.remove(&self.id);
    }
}
/// Unmounted protocol core. The listener slice owns HTTP hosting guards and binding.
/// Dropping the owner cancels every handshake and authenticated socket.
pub(crate) struct Core {
    owner: crate::structured::history::owner::Owner,
    command_owner: commands::Owner,
    admission: super::local_admission::Owner,
    shared: Arc<Shared>,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
    }
}
impl Core {
    /// New server epoch, independent of durable stream cursors or command identities.
    #[cfg(test)]
    pub(crate) async fn new(data: PathBuf, wake: Arc<tokio::sync::Notify>) -> io::Result<Self> {
        Self::new_shared(data, wake, Owner::default()).await
    }
    /// Share the socket owner's memory domain; opening runs on the bounded I/O worker.
    pub(crate) async fn new_shared(
        data: PathBuf,
        wake: Arc<tokio::sync::Notify>,
        owner: Owner,
    ) -> io::Result<Self> {
        let store = super::store_io::shared()
            .run(move || {
                owner.get(
                    &data,
                    Arc::new(
                        crate::structured::permissions::auth::tokens::app_sessions::SystemClock::new(),
                    ),
                )
            })
            .await
            .ok_or_else(|| io::Error::other("client token store worker unavailable"))?
            .map_err(io::Error::other)?;
        Self::from_store(store, Arc::new(SystemClock), wake)
    }
    /// Embed with a supplied monotonic clock shared by credential and transport deadlines.
    #[cfg(test)]
    pub(crate) fn with_clock(
        store: TokenStore,
        clock: Arc<dyn Clock>,
        wake: Arc<tokio::sync::Notify>,
    ) -> io::Result<Self> {
        Self::from_store(
            Arc::new(Store::new(store, Arc::new(AppClock::new(clock.clone())))),
            clock,
            wake,
        )
    }
    fn from_store(
        store: Arc<Store>,
        clock: Arc<dyn Clock>,
        wake: Arc<tokio::sync::Notify>,
    ) -> io::Result<Self> {
        let epoch = store.random::<16>().map_err(io::Error::other)?;
        let (stop, _) = watch::channel(false);
        let fleet = fleet::Client::new(wake.clone(), stop.subscribe());
        let (commands, command_owner) = commands::channel(wake.clone());
        let (admission, admission_owner) = super::local_admission::channel(wake.clone());
        let (histories, owner) = crate::structured::history::owner::channel(wake);
        Ok(Self {
            owner,
            command_owner,
            admission: admission_owner,
            shared: Arc::new(Shared {
                histories,
                commands,
                fleet,
                store,
                epoch,
                clock,
                slots: Arc::new(Semaphore::new(CONNECTIONS as usize)),
                hellos: std::array::from_fn(|_| Arc::new(Semaphore::new(PENDING_HELLOS))),
                admission,
                clients: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                stop,
            }),
        })
    }
    /// Service the bounded history mailbox from the registry-owning app loop.
    pub(crate) fn service(
        &mut self,
        registry: &crate::structured::registry::Registry,
        workers: &mut crate::structured::workers::Workers,
        root: &std::path::Path,
    ) {
        self.owner.service(registry, workers, root);
        self.shared.fleet.observe(registry, workers, root);
    }
    /// Mount only behind S2b-mount's loopback, Host/Origin and funnel guards.
    pub(crate) fn router(&self) -> Router {
        self.endpoint(false)
    }
    /// Same protocol and global caps, with a separate native pre-auth pool and kernel proof.
    pub(crate) fn local_router(&self) -> Router {
        self.endpoint(true)
            .layer(axum::middleware::from_fn_with_state(
                self.shared.admission.clone(),
                super::local_listener::admit,
            ))
    }
    fn endpoint(&self, local: bool) -> Router {
        Router::new().route(
            "/v1/ws",
            get(upgrade).with_state((self.shared.clone(), local)),
        )
    }
    /// Admit commands on the same owner that services structured workers.
    pub(crate) fn service_commands(&mut self, app: &mut crate::app::App) {
        self.admission.service(app);
        self.command_owner.service(app);
    }
    /// Stop and drain every transport lease even when app revocation fails.
    /// Return the revocation failure after bounded cleanup, logging it first.
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        let store = self.shared.store.clone();
        let revocation = super::store_io::shared()
            .run(move || store.shutdown())
            .await
            .ok_or_else(|| io::Error::other("app revocation worker unavailable"))
            .and_then(|out| out.map_err(io::Error::other));
        if let Err(error) = &revocation {
            crate::logging::client_app_revocation_failed(error);
        }
        self.shared.stop.send_replace(true);
        let drained = tokio::time::timeout(SHUTDOWN_WAIT, async {
            let _ = self
                .shared
                .slots
                .clone()
                .acquire_many_owned(CONNECTIONS)
                .await;
            self.shared.fleet.closed().await;
        })
        .await
        .map(|_| ())
        .map_err(io::Error::other);
        revocation.and(drained)
    }
}
/// Test adapter: clock waits share an elapsed origin without changing transport timing.
#[cfg(test)]
struct AppClock {
    clock: Arc<dyn Clock>,
    base: std::sync::OnceLock<Instant>,
}
#[cfg(test)]
impl AppClock {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            base: std::sync::OnceLock::new(),
        }
    }
}
#[cfg(test)]
impl crate::structured::permissions::auth::tokens::app_sessions::Clock for AppClock {
    fn now(&self) -> io::Result<Duration> {
        let now = self.clock.now();
        now.checked_duration_since(*self.base.get_or_init(|| now))
            .ok_or_else(|| io::Error::other("test clock reversed"))
    }
    fn until(
        &self,
        deadline: Duration,
    ) -> crate::structured::permissions::auth::tokens::app_sessions::Wait<'_> {
        Box::pin(async move {
            let at = self
                .base
                .get()
                .and_then(|base| base.checked_add(deadline))
                .ok_or_else(|| io::Error::other("test deadline has no clock origin"))?;
            self.clock.until(at).await;
            Ok(())
        })
    }
}
/// Kernel/clock readiness also revokes idle sockets; no command or heartbeat is needed.
async fn app_lifetime(shared: &Shared, session: &Session) -> u16 {
    let Some(app) = session.app() else {
        return std::future::pending().await;
    };
    let mut revoked = app.revoked.clone();
    if *revoked.borrow() {
        return AUTH_REVOKED_CLOSE;
    }
    tokio::select! { biased;
        _ = revoked.changed() => AUTH_REVOKED_CLOSE,
        expired = shared.store.app_expired(session) => {
            if expired.is_ok() { AUTH_REVOKED_CLOSE } else { GOING_AWAY_CLOSE }
        },
        exit = app.process.clone().wait() => {
            if exit.is_err() { GOING_AWAY_CLOSE } else { AUTH_REVOKED_CLOSE }
        },
    }
}
async fn upgrade(
    ws: WebSocketUpgrade,
    State((shared, local)): State<(Arc<Shared>, bool)>,
    peer: Option<Extension<crate::app::LocalPeer>>,
) -> Response {
    if local && peer.is_none() {
        return super::local_admission::refusal(super::local_admission::Refusal::Identity(
            crate::app::LocalPeerRefusal::OperatorOnly,
        ));
    }
    if *shared.stop.borrow() {
        return super::local_admission::refusal(super::local_admission::Refusal::Capacity);
    }
    let Ok(permit) = shared.slots.clone().try_acquire_owned() else {
        return super::local_admission::refusal(super::local_admission::Refusal::Capacity);
    };
    let Ok(hello) = shared.hellos[usize::from(local)]
        .clone()
        .try_acquire_owned()
    else {
        return super::local_admission::refusal(super::local_admission::Refusal::Capacity);
    };
    ws.max_frame_size(MAX_MESSAGE)
        .max_message_size(MAX_MESSAGE)
        .write_buffer_size(0)
        .max_write_buffer_size(WRITE_BUFFER_BYTES)
        .on_upgrade(move |socket| connection(socket, shared, permit, hello, peer.map(|p| p.0)))
}
async fn before<T>(
    clock: &dyn Clock,
    deadline: Instant,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! { biased; _ = clock.until(deadline) => None, out = work => Some(out) }
}
#[track_caller]
fn close(code: u16) -> Message {
    let source = std::panic::Location::caller();
    crate::logging::client_wire_closing(code, source.file(), source.line());
    Message::Close(Some(CloseFrame {
        code,
        reason: "client transport ended".into(),
    }))
}
fn unavailable(cause: &'static str) -> u16 {
    crate::logging::client_wire_unavailable(cause, None);
    GOING_AWAY_CLOSE
}
async fn authenticate(
    socket: &mut WebSocket,
    shared: &Shared,
    end: Instant,
) -> Result<(Session, bool, Bearer), HandshakeFailure> {
    let Some(Ok(Message::Text(text))) = before(shared.clock.as_ref(), end, socket.recv())
        .await
        .ok_or(HandshakeFailure::Unavailable)?
    else {
        return Err(HandshakeFailure::Refused);
    };
    let ClientFrame::Hello {
        protocol,
        client: _,
        token: Some(token),
    } = serde_json::from_str(&text).map_err(|_| HandshakeFailure::Refused)?
    else {
        return Err(HandshakeFailure::Refused);
    };
    let token = Bearer(token);
    let session = verify(shared, end, token.clone()).await?;
    Ok((session, protocol > WIRE_JSON_VERSION, token))
}
/// Handshake failures carry credential evidence separately from availability.
enum HandshakeFailure {
    /// Malformed/tokenless hello, or a credential/authentication-scope refusal.
    Refused,
    /// Verification capacity, store I/O or the original handshake deadline failed.
    Unavailable,
    /// Welcome encoding or socket delivery failed; no auth conclusion is possible.
    Transport,
}
impl From<TokenError> for HandshakeFailure {
    fn from(error: TokenError) -> Self {
        match error {
            TokenError::Unauthorized | TokenError::Revoked | TokenError::Scope => Self::Refused,
            TokenError::Busy | TokenError::Unsafe | TokenError::Io(_) => Self::Unavailable,
        }
    }
}
impl HandshakeFailure {
    fn hello_code(self) -> Option<u16> {
        match self {
            Self::Refused => Some(AUTH_REFUSED_CLOSE),
            Self::Unavailable => Some(GOING_AWAY_CLOSE),
            Self::Transport => None,
        }
    }
}
async fn verify(shared: &Shared, end: Instant, token: Bearer) -> Result<Session, HandshakeFailure> {
    let store = shared.store.clone();
    before(
        shared.clock.as_ref(),
        end,
        super::store_io::shared().run(move || store.authenticate(&token)),
    )
    .await
    .flatten()
    .ok_or(HandshakeFailure::Unavailable)?
    .map_err(HandshakeFailure::from)
}
/// Complete the registered handshake under its original ADR-0036 §5a deadline.
async fn finish_welcome<S: Sink<Message> + Unpin>(
    socket: &mut S,
    clock: &dyn Clock,
    end: Instant,
    confirmed: impl Future<Output = Result<(), HandshakeFailure>>,
    welcome: ServerFrame,
    signals: (&mut watch::Receiver<bool>, &mut watch::Receiver<bool>),
) -> bool {
    let (stop, revoked) = signals;
    let ready = async {
        confirmed.await?;
        let frame = output::encode(welcome).ok_or(HandshakeFailure::Transport)?;
        socket
            .send(frame)
            .await
            .map_err(|_| HandshakeFailure::Transport)
    };
    let accepted = tokio::select! { biased;
        _ = stop.changed() => return false,
        _ = revoked.changed() => Err(HandshakeFailure::Refused),
        sent = before(clock, end, ready) => sent.unwrap_or(Err(HandshakeFailure::Unavailable)),
    };
    let code = match accepted {
        Ok(()) => return true,
        Err(HandshakeFailure::Refused) => AUTH_REVOKED_CLOSE,
        Err(HandshakeFailure::Unavailable) => GOING_AWAY_CLOSE,
        Err(HandshakeFailure::Transport) => return false,
    };
    let _ = socket.send(close(code)).now_or_never();
    false
}
async fn connection(
    mut socket: WebSocket,
    shared: Arc<Shared>,
    permit: OwnedSemaphorePermit,
    hello: OwnedSemaphorePermit,
    peer: Option<crate::app::LocalPeer>,
) {
    let Ok(id) = shared
        .next
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
    else {
        return;
    };
    let live = Arc::new(AtomicBool::new(true));
    let _lease = Lease {
        shared: shared.clone(),
        id,
        live: live.clone(),
        _permit: permit,
    };
    let mut stop = shared.stop.subscribe();
    if *stop.borrow() {
        return;
    }
    let end = shared.clock.now() + Duration::from_millis(HANDSHAKE_DEADLINE_MS);
    let session = tokio::select! { biased; _ = stop.changed() => return, session = authenticate(&mut socket, &shared, end) => session };
    let (_session, read_only, token) = match session {
        Ok(session) => session,
        Err(failure) => {
            let Some(code) = failure.hello_code() else {
                return;
            };
            let _ = socket.send(close(code)).now_or_never();
            return;
        }
    };
    let (priority, priority_rx) = mpsc::channel(PRIORITY_CAPACITY);
    let (data, data_rx) = mpsc::channel(DATA_CAPACITY);
    let (revoke, mut revoked) = watch::channel(false);
    let registration = {
        let shared = shared.clone();
        let token = token.clone();
        super::store_io::shared().run(move || {
            let budget = end.saturating_duration_since(shared.clock.now());
            shared
                .store
                .admit_peer(&token, peer.as_ref(), budget, |session| {
                    let mut clients = shared
                        .clients
                        .lock()
                        .map_err(|_| PeerAdmissionError::from(TokenError::Busy))?;
                    if let Some(peer) = &peer {
                        if peer.process().exited().map_err(TokenError::from)? {
                            return Err(PeerAdmissionError::PeerProcessChanged);
                        }
                    }
                    if !live.load(Ordering::Acquire)
                        || *shared.stop.borrow()
                        || shared.clock.now() >= end
                        || clients
                            .values()
                            .filter(|c| c.session.same_credential(&session))
                            .count()
                            >= CREDENTIAL_CONNECTIONS
                    {
                        return Err(TokenError::Busy.into());
                    }
                    clients.insert(
                        id,
                        Client {
                            session: session.clone(),
                            read_only,
                            data,
                            revoke,
                        },
                    );
                    Ok(session)
                })
        })
    };
    let session = match before(shared.clock.as_ref(), end, registration)
        .await
        .flatten()
        .map(|result| result.and_then(|registered| registered))
    {
        Some(Ok(session)) => session,
        failure => {
            let changed = matches!(failure, Some(Err(PeerAdmissionError::PeerProcessChanged)));
            let code = match failure {
                // Nothing has registered yet: a credential refusal is a refused hello.
                Some(Err(PeerAdmissionError::Token(error))) => HandshakeFailure::from(error)
                    .hello_code()
                    .unwrap_or(GOING_AWAY_CLOSE),
                _ if changed => AUTH_REFUSED_CLOSE,
                _ => GOING_AWAY_CLOSE,
            };
            let mut frame = close(code);
            if changed {
                frame = Message::Close(Some(CloseFrame {
                    code,
                    reason: "peer_process_changed".into(),
                }));
            }
            let _ = socket.send(frame).now_or_never();
            return;
        }
    };
    // ADR-0036 §5/§6: advertise exact grants, while admission rechecks authority.
    let capabilities = session
        .info
        .scopes
        .iter()
        .filter(|scope| !read_only || **scope == Scope::Read)
        .map(|scope| {
            match scope {
                Scope::Read => "read",
                Scope::Send => "send",
                Scope::Answer => "answer",
                Scope::Operate => "operate",
            }
            .to_owned()
        })
        .collect();
    let principal = session.info.principal.clone();
    let welcome = ServerFrame::Welcome {
        protocol: WIRE_JSON_VERSION,
        server_version: crate::build_info::version(),
        capabilities,
        principal: principal.clone(),
        server_epoch: shared.epoch.clone(),
        idempotency_window_ms: IDEMPOTENCY_WINDOW_MS,
    };
    let registered = revoked.clone();
    let confirmed = async {
        let session = verify(&shared, end, token).await?;
        if session.info.principal != principal || *registered.borrow() {
            return Err(HandshakeFailure::Refused);
        }
        drop(hello);
        Ok(())
    };
    let lifetime = app_lifetime(&shared, &session);
    tokio::pin!(lifetime);
    let welcomed = tokio::select! { biased;
        code = &mut lifetime => {
            let _ = socket.send(close(code)).await;
            false
        },
        ready = finish_welcome(
        &mut socket,
        shared.clock.as_ref(),
        end,
        confirmed,
        welcome,
        (&mut stop, &mut revoked),
    ) => ready,
    };
    if !welcomed {
        return;
    }
    let (sink, mut stream) = socket.split();
    let writer = output::writer(sink, shared.clock.clone(), priority_rx, data_rx);
    tokio::pin!(writer);
    let Some((data, session, read_only)) = shared.clients.lock().ok().and_then(|clients| {
        clients.get(&id).map(|client| {
            (
                client.data.clone(),
                client.session.clone(),
                client.read_only,
            )
        })
    }) else {
        return;
    };
    let mut subscriptions = subscriptions::Subscriptions::new(
        shared.clone(),
        data.clone(),
        session.clone(),
        priority.clone(),
    );
    let mut running = tokio::task::JoinSet::new();
    let mut heartbeat = Heartbeat::new(shared.clock.now());
    let mut code = loop {
        tokio::select! { biased;
            code = &mut lifetime => break code,
            _ = stop.changed() => {
                if session.app().is_none() { return; }
                // Shutdown cannot service queued commands; their outcomes remain uncertain.
                running.abort_all();
                break AUTH_REVOKED_CLOSE;
            },
            _ = revoked.changed() => break AUTH_REVOKED_CLOSE,
            _ = &mut writer => return,
            result = running.join_next(), if !running.is_empty() => {
                if !matches!(result, Some(Ok(Ok(())))) { break unavailable("command_worker"); }
            },
            _ = shared.clock.until(heartbeat.expires()) => break unavailable("heartbeat_expired"),
            _ = shared.clock.until(heartbeat.next_ping) => {
                let Some(nonce) = heartbeat.ping(shared.clock.now(), &format!("{}/{id}", shared.epoch)) else { break unavailable("heartbeat_budget") };
                let Some(frame) = output::encode(ServerFrame::Ping { nonce }) else { break unavailable("ping_encoding") };
                if priority.try_send(frame).is_err() { break unavailable("control_capacity"); }
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(ClientFrame::Pong { nonce }) => heartbeat.pong(&nonce, shared.clock.now()),
                    Ok(ClientFrame::Sub { sub_id, stream, after }) => {
                        if !subscriptions.subscribe(sub_id, stream, after) { break unavailable("subscription_capacity") }
                    }
                    Ok(ClientFrame::Unsub { sub_id }) => subscriptions.unsubscribe(&sub_id),
                    Ok(ClientFrame::Cmd { cmd_id, verb, args, resume_only }) => {
                        let refusal = if !matches!(verb.as_str(), "agent.send" | "agent.decide") || read_only {
                            Some(crate::api::error_code::ErrorCode::ServerVersionGap)
                        } else if cmd_id.len() > 128 {
                            Some(crate::api::error_code::ErrorCode::InvalidParams)
                        } else if running.len() == COMMANDS_PER_SOCKET || shared.commands.full(&verb) {
                            Some(crate::api::error_code::ErrorCode::SessionBusy)
                        } else { None };
                        if let Some(code) = refusal {
                            let Some(ack) = output::encode(commands::ack(cmd_id, Err(code))) else { break unavailable("ack_encoding") };
                            tokio::select! { biased;
                                _ = stop.changed() => return,
                                _ = revoked.changed() => break AUTH_REVOKED_CLOSE,
                                _ = &mut writer => return,
                                _ = shared.clock.until(heartbeat.expires()) => break unavailable("heartbeat_expired"),
                                sent = priority.send(ack) => if sent.is_err() { return },
                            }
                            continue;
                        }
                        running.spawn(commands::execute(shared.clone(), session.clone(), data.clone(), commands::Command { id: cmd_id, verb, args, resume_only, read_only }));
                    }
                    _ => break POLICY_VIOLATION_CLOSE,
                },
                Some(Ok(Message::Binary(_))) => break POLICY_VIOLATION_CLOSE,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => (),
                Some(Ok(Message::Close(_))) => break unavailable("peer_close"),
                Some(Err(_)) => break unavailable("peer_transport"),
                None => break unavailable("peer_eof"),
            }
        }
    };
    // Revoked commands flush their ack before close, within the existing write ceiling.
    if code != AUTH_REVOKED_CLOSE {
        running.abort_all();
    }
    let close_deadline = shared.clock.now() + Duration::from_millis(LIVENESS_WINDOW_MS);
    while !running.is_empty() {
        tokio::select! { biased;
            _ = &mut writer => return,
            _ = shared.clock.until(close_deadline) => return,
            _ = running.join_next() => (),
        }
    }
    if code == AUTH_REVOKED_CLOSE
        && !*shared.stop.borrow()
        && session.app().is_some_and(|s| !*s.revoked.borrow())
    {
        let store = shared.store.clone();
        let session = session.clone();
        if !matches!(
            super::store_io::shared()
                .run(move || store.retire(&session))
                .await,
            Some(Ok(()) | Err(TokenError::Revoked))
        ) {
            code = GOING_AWAY_CLOSE;
        }
    }
    if priority.try_send(close(code)).is_ok() {
        tokio::select! { biased; _ = &mut writer => (), _ = shared.clock.until(close_deadline) => () }
    }
}
#[cfg(test)]
pub(crate) mod tests;
