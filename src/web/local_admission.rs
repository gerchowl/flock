//! Bounded app-owner LocalOperator admission for the ADR-0036 Unix transport.
use crate::{
    api::error_code::ErrorCode,
    app::{App, LocalPeer, LocalPeerRefusal},
    platform::peer::Credentials,
};
use axum::{
    http::{header, HeaderValue, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Notify};
/// At most one request per admitted local HTTP connection; work per owner tick is bounded too.
const CAPACITY: usize = 32;
/// The only native pre-auth refusals: two identity failures or exhausted capacity.
#[derive(Debug)]
pub(super) enum Refusal {
    /// Preserve the app owner's exhaustive identity result.
    Identity(LocalPeerRefusal),
    /// No bounded admission reservation is available.
    Capacity,
}
type Request = (Credentials, oneshot::Sender<Result<LocalPeer, Refusal>>);
/// Socket-side capability; it cannot inspect or modify caller state itself.
#[derive(Clone)]
pub(super) struct Client(mpsc::Sender<Request>, Arc<Notify>);
/// The application thread owns every LocalOperator decision.
pub(super) struct Owner(mpsc::Receiver<Request>);
/// Keep admission on the same application owner as commands and history.
pub(super) fn channel(wake: Arc<Notify>) -> (Client, Owner) {
    let (tx, rx) = mpsc::channel(CAPACITY);
    (Client(tx, wake), Owner(rx))
}
impl Client {
    /// Queue exhaustion or a stopped owner has no pre-auth admission capacity.
    pub(super) async fn admit(&self, peer: Credentials) -> Result<LocalPeer, Refusal> {
        let (tx, rx) = oneshot::channel();
        self.0.try_send((peer, tx)).map_err(|_| Refusal::Capacity)?;
        self.1.notify_one();
        rx.await.unwrap_or(Err(Refusal::Capacity))
    }
}
impl Owner {
    /// Skip cancelled HTTP requests before allocating any kernel watch.
    pub(super) fn service(&mut self, app: &mut App) {
        for _ in 0..CAPACITY {
            let Ok((peer, reply)) = self.0.try_recv() else {
                break;
            };
            if !reply.is_closed() {
                let _ = reply.send(app.admit_local_peer(peer).map_err(Refusal::Identity));
            }
        }
    }
}
/// Typed pre-upgrade refusals close HTTP without a WebSocket frame or welcome.
pub(super) fn refusal(refusal: Refusal) -> Response {
    let (status, code, message) = match refusal {
        Refusal::Identity(LocalPeerRefusal::OperatorOnly) => (
            StatusCode::FORBIDDEN,
            ErrorCode::OperatorOnly,
            "Local operator required",
        ),
        Refusal::Identity(LocalPeerRefusal::PeerProcessChanged) => (
            StatusCode::FORBIDDEN,
            ErrorCode::PeerProcessChanged,
            "Peer process identity unavailable or changed",
        ),
        Refusal::Capacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::PreauthCapacity,
            "Pre-authentication capacity exhausted",
        ),
    };
    let mut response = super::gui::refusal(status, code, message);
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}
