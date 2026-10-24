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
use std::collections::VecDeque;
use tokio::sync::{oneshot, Notify};
/// Each command lane has this fixed capacity.
const CAPACITY: usize = 32;
/// Completed principal/command keys retained in FIFO order across all principals.
const COMMAND_IDS: usize = 1024;
/// One decoded first-send command; identity and authority never come from args.
pub(super) struct Command {
    pub(super) id: String,
    pub(super) verb: String,
    pub(super) args: Value,
    pub(super) resume_only: bool,
    pub(super) read_only: bool,
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
    tx: [mpsc::Sender<Request>; 2],
    wake: Arc<Notify>,
}
/// Only the app owner touches App, runtime permission books and send admission.
pub(super) struct Owner {
    rx: [mpsc::Receiver<Request>; 2],
    completed: VecDeque<(String, String)>,
}
/// Share the existing app-loop wake without introducing a second state owner.
pub(super) fn channel(wake: Arc<Notify>) -> (Client, Owner) {
    let (send, sends) = mpsc::channel(CAPACITY);
    let (decide, decisions) = mpsc::channel(CAPACITY);
    (
        Client {
            tx: [send, decide],
            wake,
        },
        Owner {
            rx: [sends, decisions],
            completed: VecDeque::new(),
        },
    )
}
impl Client {
    /// Check admission pressure without allocating a command task or owner request.
    pub(super) fn full(&self, verb: &str) -> bool {
        self.tx[usize::from(verb == "agent.decide")].capacity() == 0
    }
}
enum Outcome {
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
    dispatch_verb(app, command, principal)
}
fn missing_target(app: &App, target: &str) -> ErrorCode {
    if app.locate_agent(target).is_some() || app.resolve_terminal_target(target).is_ok() {
        ErrorCode::UnsupportedForAgent
    } else {
        ErrorCode::AgentNotFound
    }
}
fn dispatch_verb(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
) -> Result<Outcome, ErrorCode> {
    match command.verb.as_str() {
        "agent.decide" => {
            // TODO(#2249): Bound decide message independently of the frame limit.
            let args: AgentDecide = serde_json::from_value(command.args.clone())
                .map_err(|_| ErrorCode::InvalidParams)?;
            let (_, _, generation) = app
                .state
                .structured_agents
                .get(&args.agent)
                .ok_or_else(|| missing_target(app, &args.agent))?;
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
        _ => Err(ErrorCode::ServerVersionGap),
    }
}
/// Preserve the registered error envelope on ingress refusals and owner outcomes.
pub(super) fn ack(id: String, result: Result<Value, ErrorCode>) -> ServerFrame {
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
    fn remember(&mut self, key: (String, String)) {
        if self.completed.len() == COMMAND_IDS {
            self.completed.pop_front();
        }
        self.completed.push_back(key);
    }
    /// Revalidate under the token revocation lock through bounded admission.
    /// No model or stdin completion is awaited.
    pub(super) fn service(&mut self, app: &mut App) {
        // Decisions retain first admission priority; send routing is unmounted (#1831).
        for lane in [1, 0] {
            for _ in 0..CAPACITY {
                let Ok(request) = self.rx[lane].try_recv() else {
                    break;
                };
                if request.response.is_closed() {
                    continue;
                }
                let key = (
                    request.session.info.principal.clone(),
                    request.command.id.clone(),
                );
                let scope = if lane == 1 {
                    Scope::Answer
                } else {
                    Scope::Send
                };
                let outcome = request
                    .store
                    .admit(&request.session, scope, |token| {
                        if self.completed.contains(&key) {
                            return Err(ErrorCode::SessionUnconfirmed);
                        }
                        dispatch(app, &request.command, &Principal::Token(token))
                    })
                    .unwrap_or_else(|error| {
                        Err(match error {
                            TokenError::Revoked | TokenError::Unauthorized => {
                                ErrorCode::AuthRevoked
                            }
                            TokenError::Scope => ErrorCode::Scope,
                            TokenError::Busy => ErrorCode::SessionBusy,
                            TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
                        })
                    });
                match outcome {
                    Ok(Outcome::Decided(value)) => {
                        self.remember(key);
                        let _ = request.response.send(ack(request.command.id, Ok(value)));
                    }
                    Err(code) => {
                        let _ = request.response.send(ack(request.command.id, Err(code)));
                    }
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
    let lane = usize::from(command.verb == "agent.decide");
    let frame = match shared.commands.tx[lane].try_send(Request {
        command,
        store: shared.store.clone(),
        session: session.clone(),
        response,
    }) {
        Ok(()) => {
            shared.commands.wake.notify_one();
            received.await.map_err(|_| ())?
        }
        Err(mpsc::error::TrySendError::Full(request)) => {
            ack(request.command.id, Err(ErrorCode::SessionBusy))
        }
        Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
    };
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
