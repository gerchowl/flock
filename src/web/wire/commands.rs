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
use std::{
    collections::VecDeque,
    sync::mpsc::{Receiver, TryRecvError},
};
use tokio::sync::{oneshot, Notify};
/// Closed registry shared by wire admission and refusal budget normalization.
pub(super) fn known_verb(verb: &str) -> Option<&'static str> {
    match verb {
        "agent.send" => Some("agent.send"),
        "agent.decide" => Some("agent.decide"),
        "agent.start" => Some("agent.start"),
        "agent.restart" => Some("agent.restart"),
        "agent.stop" => Some("agent.stop"),
        _ => None,
    }
}
/// Each command lane and the send receipt set have this fixed capacity.
const CAPACITY: usize = 32;
/// Completed principal/command keys retained in FIFO order; pending sends never evict.
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
    store: Arc<Store>,
    session: Session,
    response: oneshot::Sender<ServerFrame>,
}
/// Connection-side mailbox; cancelling a waiter cannot cancel committed input.
#[derive(Clone)]
pub(super) struct Client {
    tx: [mpsc::Sender<Request>; 2],
    wake: Arc<Notify>,
}
type Receipt = Receiver<Result<AgentSendResult, ErrorCode>>;
struct Pending {
    command: Command,
    session: Session,
    key: (String, String),
    // Shared core shutdown requires Sync; the app owner uses get_mut, never a blocking lock.
    receipt: parking_lot::Mutex<Receipt>,
    response: oneshot::Sender<ServerFrame>,
}
struct Completed {
    key: (String, String),
    lifecycle: Option<(String, Value, Result<Value, ErrorCode>)>,
}
/// Only the app owner touches App, runtime permission books and send admission.
pub(super) struct Owner {
    rx: [mpsc::Receiver<Request>; 2],
    completed: VecDeque<Completed>,
    pending: Vec<Pending>,
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
            pending: Vec::new(),
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
    Queued(Receipt),
    Decided(Value),
    Replayed(Result<Value, ErrorCode>),
}
fn dispatch(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
    session: &Session,
) -> Result<Outcome, ErrorCode> {
    if command.read_only {
        return Err(ErrorCode::ServerVersionGap);
    }
    // No durable command journal exists in S7-min; uncertain retries never dispatch.
    if command.resume_only {
        return Err(ErrorCode::SessionUnconfirmed);
    }
    dispatch_verb(app, command, principal, session)
}
fn missing_target(app: &App, target: &str) -> ErrorCode {
    if app.locate_agent(target).is_some() || app.resolve_terminal_target(target).is_ok() {
        ErrorCode::UnsupportedForAgent
    } else {
        ErrorCode::AgentNotFound
    }
}
fn lifecycle(verb: &str) -> bool {
    matches!(verb, "agent.start" | "agent.restart" | "agent.stop")
}
fn dispatch_verb(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
    session: &Session,
) -> Result<Outcome, ErrorCode> {
    if lifecycle(&command.verb) {
        let mut args = command.args.clone();
        if let Some(node) = args.as_object_mut().and_then(|args| args.remove("node_id")) {
            let node = node.as_str().ok_or(ErrorCode::InvalidParams)?;
            if app.node_id.as_deref() != Some(node) {
                return Err(ErrorCode::UnsupportedForAgent);
            }
        }
        if args
            .get("target")
            .and_then(Value::as_str)
            .and_then(|target| app.locate_agent(target))
            .is_some_and(|location| !location.local)
        {
            return Err(ErrorCode::UnsupportedForAgent);
        }
        let typed = serde_json::from_value(serde_json::json!({
            "verb": command.verb, "args": args,
        }))
        .map_err(|_| ErrorCode::InvalidParams)?;
        return app
            .wire_agent_lifecycle(typed, session)
            .map(Outcome::Decided);
    }
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
                return Err(missing_target(app, &params.target));
            }
            Ok(Outcome::Queued(
                app.structured_send(params, principal)?.committed,
            ))
        }
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
    fn remember(
        &mut self,
        key: (String, String),
        lifecycle: Option<(String, Value, Result<Value, ErrorCode>)>,
    ) {
        if self.completed.len() == COMMAND_IDS {
            self.completed.pop_front();
        }
        self.completed.push_back(Completed { key, lifecycle });
    }
    /// Revalidate under the token revocation lock through bounded admission.
    /// Queue completion is polled separately; no model or stdin completion is awaited.
    pub(super) fn service(&mut self, app: &mut App, shared: &Shared) {
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
            if let Err(code) = &result {
                shared.refusals.record(
                    shared.clock.now(),
                    &pending.session.info,
                    &pending.command.verb,
                    &pending.command.args,
                    *code,
                );
            }
            self.remember(pending.key.clone(), None);
            let _ = pending.response.send(ack(pending.key.1, result));
        }
        // Decisions have independent capacity even when every send receipt is pending.
        for lane in [1, 0] {
            for _ in 0..CAPACITY {
                if lane == 0 && self.pending.len() == CAPACITY {
                    break;
                }
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
                let is_lifecycle = lifecycle(&request.command.verb);
                let scope = if is_lifecycle {
                    Scope::Operate
                } else if lane == 1 {
                    Scope::Answer
                } else {
                    Scope::Send
                };
                // Only the owner mutates these sets; lookup need not hold the token lock.
                let completed = self.completed.iter().find(|entry| entry.key == key);
                let duplicate = completed.is_some() || self.pending.iter().any(|p| p.key == key);
                let mut dispatched = false;
                let outcome = request
                    .store
                    .admit(&request.session, scope, |token| {
                        if duplicate {
                            if let Some((verb, args, result)) = completed
                                .filter(|_| is_lifecycle)
                                .and_then(|entry| entry.lifecycle.as_ref())
                            {
                                if verb != &request.command.verb || args != &request.command.args {
                                    return Err(ErrorCode::InvalidParams);
                                }
                                return Ok(Outcome::Replayed(result.clone()));
                            }
                            return Err(ErrorCode::SessionUnconfirmed);
                        }
                        dispatched = true;
                        dispatch(
                            app,
                            &request.command,
                            &Principal::Token(token),
                            &request.session,
                        )
                    })
                    .unwrap_or_else(|error| {
                        Err(match error {
                            TokenError::Revoked | TokenError::Unauthorized => {
                                ErrorCode::AuthRevoked
                            }
                            TokenError::Scope if is_lifecycle => ErrorCode::NotAuthorized,
                            TokenError::Scope => ErrorCode::Scope,
                            TokenError::Busy => ErrorCode::SessionBusy,
                            TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
                        })
                    });
                if is_lifecycle {
                    let target = request
                        .command
                        .args
                        .get("target")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    // Registry context keeps refused decisions attributable without granting authority.
                    let identity = match &outcome {
                        Ok(Outcome::Decided(value) | Outcome::Replayed(Ok(value))) => {
                            value.get("agent").and_then(|agent| {
                                Some((
                                    agent.get("agent_id")?.as_str()?.to_owned(),
                                    agent.get("session_id")?.as_str()?.to_owned(),
                                ))
                            })
                        }
                        _ => None,
                    }
                    .or_else(|| {
                        if target == "self" {
                            None
                        } else {
                            app.state
                                .structured_agents
                                .get(target)
                                .map(|(record, _, _)| {
                                    (record.agent_id.clone(), record.session_id.clone())
                                })
                                .or_else(|| {
                                    app.state
                                        .structured_agents
                                        .records()
                                        .into_iter()
                                        .find(|record| record.name.as_deref() == Some(target))
                                        .map(|record| (record.agent_id, record.session_id))
                                })
                        }
                    });
                    let agent_id = identity
                        .as_ref()
                        .map(|(agent, _)| agent.clone())
                        .or_else(|| app.locate_agent(target).map(|location| location.agent_id))
                        .unwrap_or_default();
                    tracing::info!(
                        event = "wire.lifecycle.admission",
                        credential = request.session.info.name.as_str(),
                        principal = request.session.info.principal.as_str(),
                        target,
                        agent_id = agent_id.as_str(),
                        session_id = identity
                            .as_ref()
                            .map_or("", |(_, session)| session.as_str()),
                        verb = request.command.verb.as_str(),
                        cmd_id = request.command.id.as_str(),
                        outcome = outcome.as_ref().map_or_else(
                            |code| code.as_str(),
                            |value| {
                                match value {
                                    Outcome::Replayed(Err(code)) => code.as_str(),
                                    _ => "admitted",
                                }
                            }
                        ),
                        "wire lifecycle admission"
                    );
                }
                if let Some(code) = match &outcome {
                    Err(code) | Ok(Outcome::Replayed(Err(code))) => Some(*code),
                    _ => None,
                } {
                    shared.refusals.record(
                        shared.clock.now(),
                        &request.session.info,
                        &request.command.verb,
                        &request.command.args,
                        code,
                    );
                }
                match outcome {
                    Ok(Outcome::Queued(receipt)) => self.pending.push(Pending {
                        command: request.command,
                        session: request.session,
                        key,
                        receipt: parking_lot::Mutex::new(receipt),
                        response: request.response,
                    }),
                    Ok(Outcome::Decided(value)) => {
                        let cached = is_lifecycle.then(|| {
                            (
                                request.command.verb.clone(),
                                request.command.args.clone(),
                                Ok(value.clone()),
                            )
                        });
                        self.remember(key, cached);
                        let _ = request.response.send(ack(request.command.id, Ok(value)));
                    }
                    Ok(Outcome::Replayed(result)) => {
                        let _ = request.response.send(ack(request.command.id, result));
                    }
                    Err(code) => {
                        if is_lifecycle && dispatched {
                            self.remember(
                                key,
                                Some((
                                    request.command.verb.clone(),
                                    request.command.args.clone(),
                                    Err(code),
                                )),
                            );
                        }
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
            shared.refusals.record(
                shared.clock.now(),
                &session.info,
                &request.command.verb,
                &request.command.args,
                ErrorCode::SessionBusy,
            );
            ack(request.command.id, Err(ErrorCode::SessionBusy))
        }
        Err(mpsc::error::TrySendError::Closed(request)) => {
            shared.refusals.record(
                shared.clock.now(),
                &session.info,
                &request.command.verb,
                &request.command.args,
                ErrorCode::InternalError,
            );
            return Err(());
        }
    };
    let revoked = matches!(&frame, ServerFrame::Ack { error: Some(error), .. } if error.code == ErrorCode::AuthRevoked.as_str());
    let (delivered, flushed) = oneshot::channel();
    let flushed = async {
        data.send(output::Delivery {
            frame,
            active: None,
            delivered: Some(delivered),
        })
        .await
        .map_err(|_| ())?;
        match flushed.await {
            Ok(true) => Ok(()),
            _ => Err(()),
        }
    }
    .await;
    // ADR-0036 §5a: a lost ack must not suppress revocation of this credential’s sockets.
    if revoked {
        shared.revoke(&session);
    }
    flushed
}
