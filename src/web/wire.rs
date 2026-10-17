//! Server-owned, opt-in structured transport; no PTY or model dispatch.
mod output;
pub(crate) mod timing;
use crate::{
    api::client_tokens::Bearer,
    structured::permissions::auth::tokens::{Session, TokenStore},
    structured_wire::*,
};
use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{FutureExt, SinkExt, StreamExt};
use std::{
    collections::HashMap,
    future::Future,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use timing::{Clock, Heartbeat, SystemClock};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
const MAX_MESSAGE: usize = 65_536;
const CONNECTIONS: u32 = 16;
/// Socket identity remains owned until teardown; later dispatch uses this verified session.
struct Client {
    session: Session,
    // The command slice enforces version gating before allocation.
    #[allow(dead_code)]
    read_only: bool,
    // The replay slice supplies bounded data through this same writer.
    #[allow(dead_code)]
    data: mpsc::Sender<ServerFrame>,
    revoke: watch::Sender<bool>,
}
struct Shared {
    store: Arc<TokenStore>,
    epoch: String,
    clock: Arc<dyn Clock>,
    slots: Arc<Semaphore>,
    clients: Mutex<HashMap<u64, Client>>,
    next: AtomicU64,
    stop: watch::Sender<bool>,
}
impl Shared {
    /// Slice 5 calls only after writing the triggering command's refusal ack.
    #[allow(dead_code)]
    fn revoke(&self, principal: &str) {
        if let Ok(clients) = self.clients.lock() {
            for client in clients
                .values()
                .filter(|c| c.session.info.principal == principal)
            {
                let _ = client.revoke.send(true);
            }
        }
    }
}
struct Lease {
    shared: Arc<Shared>,
    id: u64,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut clients) = self.shared.clients.lock() {
            clients.remove(&self.id);
        }
    }
}
/// Unmounted protocol core. The listener slice owns HTTP hosting guards and binding.
/// Dropping the owner cancels every handshake and authenticated socket.
pub(crate) struct Core {
    shared: Arc<Shared>,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
    }
}
impl Core {
    /// New server epoch, independent of durable stream cursors or command identities.
    pub(crate) async fn new(data: PathBuf) -> io::Result<Self> {
        let store = super::store_io::shared()
            .run(move || TokenStore::open(&data))
            .await
            .ok_or_else(|| io::Error::other("client token store worker unavailable"))?
            .map_err(io::Error::other)?;
        Self::with_clock(store, Arc::new(SystemClock))
    }
    /// Embed with a supplied monotonic clock; deadlines never use wall-clock time.
    pub(crate) fn with_clock(store: TokenStore, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let (stop, _) = watch::channel(false);
        Ok(Self {
            shared: Arc::new(Shared {
                store: Arc::new(store),
                epoch: random.iter().map(|b| format!("{b:02x}")).collect(),
                clock,
                slots: Arc::new(Semaphore::new(CONNECTIONS as usize)),
                clients: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
                stop,
            }),
        })
    }
    /// Mount only behind S2b-mount's loopback, Host/Origin and funnel guards.
    pub(crate) fn router(&self) -> Router {
        Router::new()
            .route("/v1/ws", get(upgrade))
            .with_state(self.shared.clone())
    }
    /// Later dispatch calls after writing the triggering auth_revoked acknowledgement.
    pub(crate) fn revoke(&self, session: &Session) {
        self.shared.revoke(&session.info.principal);
    }
    /// Cancel transport work and wait for every connection lease, including hello.
    pub(crate) async fn shutdown(&self) -> Result<(), tokio::time::error::Elapsed> {
        self.shared.stop.send_replace(true);
        tokio::time::timeout(
            Duration::from_secs(5),
            self.shared.slots.clone().acquire_many_owned(CONNECTIONS),
        )
        .await
        .map(|_| ())
    }
}
async fn upgrade(ws: WebSocketUpgrade, State(shared): State<Arc<Shared>>) -> Response {
    if *shared.stop.borrow() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(permit) = shared.slots.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_frame_size(MAX_MESSAGE)
        .max_message_size(MAX_MESSAGE)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_MESSAGE * 2)
        .on_upgrade(move |socket| connection(socket, shared, permit))
}
async fn before<T>(
    clock: &dyn Clock,
    deadline: Instant,
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! { biased; _ = clock.until(deadline) => None, out = work => Some(out) }
}
fn close(code: u16) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: "client transport ended".into(),
    }))
}
async fn authenticate(
    socket: &mut WebSocket,
    shared: &Shared,
    end: Instant,
) -> Option<(Session, bool)> {
    let Some(Ok(Message::Text(text))) = before(shared.clock.as_ref(), end, socket.recv()).await?
    else {
        return None;
    };
    let ClientFrame::Hello {
        protocol,
        client: _,
        token: Some(token),
    } = serde_json::from_str(&text).ok()?
    else {
        return None;
    };
    let store = shared.store.clone();
    let result = before(
        shared.clock.as_ref(),
        end,
        super::store_io::shared().run(move || store.authenticate(&Bearer(token))),
    )
    .await?;
    let session = result?.ok()?;
    let welcome = ServerFrame::Welcome {
        protocol: WIRE_JSON_VERSION,
        server_version: crate::build_info::version(),
        capabilities: vec![],
        principal: session.info.principal.clone(),
        server_epoch: shared.epoch.clone(),
        idempotency_window_ms: 86_400_000,
    };
    before(
        shared.clock.as_ref(),
        end,
        socket.send(output::encode(welcome)?),
    )
    .await?
    .ok()?;
    Some((session, protocol > WIRE_JSON_VERSION))
}
async fn connection(mut socket: WebSocket, shared: Arc<Shared>, permit: OwnedSemaphorePermit) {
    let Ok(id) = shared
        .next
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
    else {
        return;
    };
    let _lease = Lease {
        shared: shared.clone(),
        id,
        _permit: permit,
    };
    let mut stop = shared.stop.subscribe();
    if *stop.borrow() {
        return;
    }
    let end = shared.clock.now() + Duration::from_millis(HANDSHAKE_DEADLINE_MS);
    let session = tokio::select! { biased; _ = stop.changed() => return, session = authenticate(&mut socket, &shared, end) => session };
    let Some((session, read_only)) = session else {
        let _ = socket.send(close(AUTH_REFUSED_CLOSE)).now_or_never();
        return;
    };
    let (priority, priority_rx) = mpsc::channel(4);
    let (data, data_rx) = mpsc::channel(8);
    let (revoke, mut revoked) = watch::channel(false);
    if let Ok(mut clients) = shared.clients.lock() {
        clients.insert(
            id,
            Client {
                session,
                read_only,
                data,
                revoke,
            },
        );
    } else {
        return;
    }
    let (sink, mut stream) = socket.split();
    let writer = output::writer(sink, shared.clock.clone(), priority_rx, data_rx);
    tokio::pin!(writer);
    let mut heartbeat = Heartbeat::new(shared.clock.now());
    let code = loop {
        tokio::select! { biased;
            _ = stop.changed() => return,
            _ = revoked.changed() => break AUTH_REVOKED_CLOSE,
            _ = &mut writer => return,
            _ = shared.clock.until(heartbeat.expires()) => break 1001,
            _ = shared.clock.until(heartbeat.next_ping) => {
                let Some(nonce) = heartbeat.ping(shared.clock.now(), &format!("{}/{id}", shared.epoch)) else { break 1001 };
                let Some(frame) = output::encode(ServerFrame::Ping { nonce }) else { break 1001 };
                if priority.try_send(frame).is_err() { break 1001; }
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(ClientFrame::Pong { nonce }) => heartbeat.pong(&nonce, shared.clock.now()),
                    _ => break 1008,
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => (),
                _ => break 1001,
            }
        }
    };
    if priority.try_send(close(code)).is_ok() {
        let _ = before(shared.clock.as_ref(), heartbeat.expires(), &mut writer).await;
    }
}
#[cfg(test)]
mod tests;
