//! Bounded app-owner LocalOperator admission for the ADR-0036 Unix transport.
use crate::{
    api::error_code::ErrorCode,
    app::{App, LocalPeer},
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
type Request = (Credentials, oneshot::Sender<Result<LocalPeer, ErrorCode>>);
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
    pub(super) async fn admit(&self, peer: Credentials) -> Result<LocalPeer, ErrorCode> {
        let (tx, rx) = oneshot::channel();
        self.0
            .try_send((peer, tx))
            .map_err(|_| ErrorCode::PreauthCapacity)?;
        self.1.notify_one();
        rx.await.unwrap_or(Err(ErrorCode::PreauthCapacity))
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
                let _ = reply.send(app.admit_local_peer(peer));
            }
        }
    }
}
/// Typed pre-upgrade refusals close HTTP without a WebSocket frame or welcome.
pub(super) fn refusal(code: ErrorCode) -> Response {
    let (status, message) = match code {
        ErrorCode::OperatorOnly => (StatusCode::FORBIDDEN, "Local operator required"),
        ErrorCode::PeerProcessChanged => (
            StatusCode::FORBIDDEN,
            "Peer process identity unavailable or changed",
        ),
        ErrorCode::PreauthCapacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Pre-authentication capacity exhausted",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Local admission unavailable",
        ),
    };
    let mut response = super::gui::refusal(status, code, message);
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}
