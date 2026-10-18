//! Bounded app-owner command admission (ADR-0036 §5/§5a, S7-min slice 5).
use super::*;
use crate::{
    api::error_code::ErrorCode,
    app::App,
    structured::permissions::{
        self,
        auth::tokens::{Scope, TokenError},
        Principal,
    },
};
use serde_json::Value;
use std::sync::mpsc::{Receiver, TryRecvError};
use tokio::sync::{oneshot, Notify};
/// Pending owner messages and durable tickets each have this fixed capacity.
const CAPACITY: usize = 32;
/// Successful admissions retained per connection; overflow refuses before dispatch.
const COMMAND_IDS: usize = 1024;
/// One decoded first-send command; identity and authority never come from args.
pub(super) struct Command {
    pub(super) id: String,
    pub(super) verb: String,
    pub(super) args: Value,
    pub(super) resume_only: bool,
    pub(super) read_only: bool,
    pub(super) ids: Arc<parking_lot::Mutex<HashSet<String>>>,
}
struct Request {
    command: Command,
    store: Arc<TokenStore>,
    session: Session,
    response: oneshot::Sender<ServerFrame>,
}
/// Connection-side mailbox; cancelling a waiter cannot cancel committed input.
#[derive(Clone)]
pub(super) struct Client {
    tx: mpsc::Sender<Request>,
    wake: Arc<Notify>,
}
type Receipt = Receiver<Result<AgentSendResult, ErrorCode>>;
struct Pending {
    id: String,
    // Shared core shutdown requires Sync; the app owner uses get_mut, never a blocking lock.
    receipt: parking_lot::Mutex<Receipt>,
    response: oneshot::Sender<ServerFrame>,
}
/// Only the app owner touches App, runtime permission books and send admission.
pub(super) struct Owner {
    rx: mpsc::Receiver<Request>,
    pending: Vec<Pending>,
}
/// Share the existing app-loop wake without introducing a second state owner.
pub(super) fn channel(wake: Arc<Notify>) -> (Client, Owner) {
    let (tx, rx) = mpsc::channel(CAPACITY);
    (
        Client { tx, wake },
        Owner {
            rx,
            pending: Vec::new(),
        },
    )
}
enum Outcome {
    Queued(Receipt),
    Decided(Value),
}
fn dispatch(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
) -> Result<Outcome, ErrorCode> {
    if command.read_only {
        return Err(ErrorCode::ServerVersionGap);
    }
    // No durable command journal exists in S7-min; uncertain retries never dispatch.
    if command.resume_only {
        return Err(ErrorCode::SessionUnconfirmed);
    }
    if !matches!(command.verb.as_str(), "agent.send" | "agent.decide") {
        return Err(ErrorCode::UnsupportedForAgent);
    }
    let mut ids = command.ids.try_lock().ok_or(ErrorCode::SessionBusy)?;
    if ids.contains(&command.id) {
        return Err(ErrorCode::SessionUnconfirmed);
    }
    if ids.len() == COMMAND_IDS {
        return Err(ErrorCode::SessionBusy);
    }
    let result = dispatch_verb(app, command, principal);
    if result.is_ok() {
        ids.insert(command.id.clone());
    }
    result
}
fn dispatch_verb(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
) -> Result<Outcome, ErrorCode> {
    match command.verb.as_str() {
        "agent.send" => {
            let args: crate::api::schema::AgentSendParams =
                serde_json::from_value(command.args.clone())
                    .map_err(|_| ErrorCode::InvalidParams)?;
            if args.submit && (args.raw || args.write_timeout_ms.is_some()) {
                return Err(ErrorCode::InvalidParams);
            }
            let params = crate::app::StructuredSendParams {
                target: args.target,
                text: args.text,
                submit: args.submit,
                item_id: args.item_id,
            };
            // Let the shared service supply its refusal before any queue mutation.
            if params.submit && app.structured_send_target(&params.target)?.is_none() {
                return Err(ErrorCode::UnsupportedForAgent);
            }
            Ok(Outcome::Queued(
                app.structured_send(params, principal)?.committed,
            ))
        }
        "agent.decide" => {
            let args: AgentDecide = serde_json::from_value(command.args.clone())
                .map_err(|_| ErrorCode::InvalidParams)?;
            let (_, _, generation) = app
                .state
                .structured_agents
                .get(&args.agent)
                .ok_or(ErrorCode::UnsupportedForAgent)?;
            let runtime = app
                .structured_workers
                .runtime(&args.agent, generation)
                .ok_or(ErrorCode::AgentNotFound)?;
            let backend: permissions::AgentDecide = serde_json::from_value(
                serde_json::to_value(&args).map_err(|_| ErrorCode::SerializationError)?,
            )
            .map_err(|_| ErrorCode::InvalidParams)?;
            runtime.agent_decide(principal, &backend)?;
            Ok(Outcome::Decided(
                serde_json::to_value(AgentDecideResult {
                    request_id: args.request_id,
                    decision: args.decision,
                })
                .map_err(|_| ErrorCode::SerializationError)?,
            ))
        }
        _ => Err(ErrorCode::UnsupportedForAgent),
    }
}
fn ack(id: String, result: Result<Value, ErrorCode>) -> ServerFrame {
    use crate::cli::exit::Retry;
    let (result, error) = match result {
        Ok(result) => (Some(result), None),
        Err(code) => (
            None,
            Some(WireError {
                code: code.as_str().into(),
                message: format!("command refused: {code}"),
                // The wire uncertainty contract uses exit 8 (ADR-0036 §5a).
                exit: if code == ErrorCode::SessionUnconfirmed {
                    crate::cli::exit::UNCONFIRMED
                } else {
                    code.exit()
                },
                retry: match code.retry() {
                    Retry::NotApplicable => WireRetry::NotApplicable,
                    Retry::Never => WireRetry::Never,
                    Retry::Maybe => WireRetry::Maybe,
                    Retry::Yes => WireRetry::Yes,
                    Retry::Later => WireRetry::Later,
                    Retry::AfterInspecting => WireRetry::AfterInspecting,
                    Retry::AfterHuman => WireRetry::AfterHuman,
                    Retry::AfterHandoff => WireRetry::AfterHandoff,
                },
                data: None,
            }),
        ),
    };
    ServerFrame::Ack {
        cmd_id: id,
        ok: error.is_none(),
        result,
        error,
    }
}
impl Owner {
    /// Revalidate under the token revocation lock through bounded admission.
    /// Queue completion is polled separately; no model or stdin completion is awaited.
    pub(super) fn service(&mut self, app: &mut App) {
        self.pending.retain(|pending| !pending.response.is_closed());
        let mut i = 0;
        while i < self.pending.len() {
            let result = match self.pending[i].receipt.get_mut().try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
            };
            let pending = self.pending.swap_remove(i);
            let result = result.and_then(|result| {
                serde_json::to_value(result).map_err(|_| ErrorCode::SerializationError)
            });
            let _ = pending.response.send(ack(pending.id, result));
        }
        for _ in 0..CAPACITY {
            if self.pending.len() == CAPACITY {
                break;
            }
            let Ok(request) = self.rx.try_recv() else {
                break;
            };
            if request.response.is_closed() {
                continue;
            }
            let scope = match request.command.verb.as_str() {
                "agent.send" => Scope::Send,
                "agent.decide" => Scope::Answer,
                _ => Scope::Read,
            };
            let outcome = request
                .store
                .admit(&request.session, scope, |token| {
                    dispatch(app, &request.command, &Principal::Token(token))
                })
                .unwrap_or_else(|error| {
                    Err(match error {
                        TokenError::Revoked | TokenError::Unauthorized => ErrorCode::AuthRevoked,
                        TokenError::Scope => ErrorCode::Scope,
                        TokenError::Busy => ErrorCode::SessionBusy,
                        TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
                    })
                });
            match outcome {
                Ok(Outcome::Queued(receipt)) => self.pending.push(Pending {
                    id: request.command.id,
                    receipt: parking_lot::Mutex::new(receipt),
                    response: request.response,
                }),
                Ok(Outcome::Decided(value)) => {
                    let _ = request.response.send(ack(request.command.id, Ok(value)));
                }
                Err(code) => {
                    let _ = request.response.send(ack(request.command.id, Err(code)));
                }
            }
        }
    }
}
/// Wait for the committed outcome independently of the reader and heartbeat.
/// The triggering acknowledgement must be flushed before broadcasting revocation.
pub(super) async fn execute(
    shared: Arc<Shared>,
    session: Session,
    data: mpsc::Sender<output::Delivery>,
    command: Command,
) -> Result<(), ()> {
    let (response, received) = oneshot::channel();
    shared
        .commands
        .tx
        .try_send(Request {
            command,
            store: shared.store.clone(),
            session: session.clone(),
            response,
        })
        .map_err(|_| ())?;
    shared.commands.wake.notify_one();
    let frame = received.await.map_err(|_| ())?;
    let revoked = matches!(&frame, ServerFrame::Ack { error: Some(error), .. } if error.code == ErrorCode::AuthRevoked.as_str());
    let (delivered, flushed) = oneshot::channel();
    data.send(output::Delivery {
        frame,
        active: None,
        delivered: Some(delivered),
    })
    .await
    .map_err(|_| ())?;
    if !flushed.await.map_err(|_| ())? {
        return Err(());
    }
    if revoked {
        shared.revoke(&session);
    }
    Ok(())
}
