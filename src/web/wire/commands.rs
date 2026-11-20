//! Bounded app-owner command admission (ADR-0036 §5/§5a, S7-min slice 5).
use super::*;
mod journal;
mod ownership;
use crate::{
    api::error_code::ErrorCode,
    app::App,
    structured::permissions::{
        self,
        auth::tokens::{Scope, TokenError},
        Principal,
    },
};
pub(super) use ownership::Ownership;
use serde_json::Value;
use std::sync::mpsc::{Receiver, TryRecvError};
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
/// One decoded first-send command; identity and authority never come from args.
#[derive(Clone)]
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
    gate: Arc<AtomicU64>,
}
type Receipt = Receiver<Result<AgentSendResult, ErrorCode>>;
enum Stage {
    Claim(journal::Response),
    Send(parking_lot::Mutex<Receipt>),
    Finish(journal::Response),
}
struct Pending {
    request: Request,
    joined: Vec<Request>,
    phase: u64,
    admitted_ms: u64,
    stage: Stage,
}
/// Only the app owner touches App, runtime permission books and send admission.
pub(super) struct Owner {
    rx: [mpsc::Receiver<Request>; 2],
    gate: Arc<AtomicU64>,
    journal: Ownership,
    pending: Vec<Pending>,
}
/// Share the existing app-loop wake without introducing a second state owner.
pub(super) fn channel(wake: Arc<Notify>, journal: Ownership) -> (Client, Owner) {
    let gate = journal.gate();
    let (send, sends) = mpsc::channel(CAPACITY);
    let (decide, decisions) = mpsc::channel(CAPACITY);
    (
        Client {
            tx: [send, decide],
            wake,
            gate: gate.clone(),
        },
        Owner {
            rx: [sends, decisions],
            gate,
            journal,
            pending: Vec::new(),
        },
    )
}
impl Client {
    /// Check admission pressure without allocating a command task or owner request.
    pub(super) fn full(&self, verb: &str) -> bool {
        self.gate.load(Ordering::Acquire) & 1 == 0
            || self.tx[usize::from(verb == "agent.decide")].capacity() == 0
    }
}
enum Outcome {
    Queued(Receipt),
    Decided(Value),
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
fn scope(command: &Command) -> Scope {
    if lifecycle(&command.verb) {
        Scope::Operate
    } else if command.verb == "agent.decide" {
        Scope::Answer
    } else {
        Scope::Send
    }
}
fn authorize<T>(
    request: &Request,
    apply: impl for<'a> FnOnce(permissions::auth::VerifiedToken<'a>) -> Result<T, ErrorCode>,
) -> Result<T, ErrorCode> {
    request
        .store
        .admit(&request.session, scope(&request.command), apply)
        .unwrap_or_else(|error| {
            Err(match error {
                TokenError::Revoked | TokenError::Unauthorized => ErrorCode::AuthRevoked,
                TokenError::Scope if lifecycle(&request.command.verb) => ErrorCode::NotAuthorized,
                TokenError::Scope => ErrorCode::Scope,
                TokenError::Busy => ErrorCode::SessionBusy,
                TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
            })
        })
}
fn respond(request: Request, frame: ServerFrame, shared: &Shared, app: &App) {
    let frame = match authorize(&request, |_| Ok(())) {
        Ok(()) => frame,
        Err(code) => ack(request.command.id.clone(), Err(code)),
    };
    let outcome = match &frame {
        ServerFrame::Ack {
            result: Some(value),
            error: None,
            ..
        } => Ok(Outcome::Decided(value.clone())),
        ServerFrame::Ack {
            error: Some(error), ..
        } => Err(ErrorCode::from_code(&error.code).unwrap_or(ErrorCode::InternalError)),
        _ => Err(ErrorCode::InternalError),
    };
    trace_lifecycle(app, &request, &outcome);
    if let ServerFrame::Ack {
        error: Some(ref error),
        ..
    } = frame
    {
        if let Some(code) = ErrorCode::from_code(&error.code) {
            shared.refusals.record(
                shared.clock.now(),
                &request.session.info,
                &request.command.verb,
                &request.command.args,
                code,
            );
        }
    }
    let _ = request.response.send(frame);
}
fn trace_lifecycle(app: &App, request: &Request, outcome: &Result<Outcome, ErrorCode>) {
    if lifecycle(&request.command.verb) {
        let target = request
            .command
            .args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Registry context keeps refused decisions attributable without granting authority.
        let identity = match outcome {
            Ok(Outcome::Decided(value)) => value.get("agent").and_then(|agent| {
                Some((
                    agent.get("agent_id")?.as_str()?.to_owned(),
                    agent.get("session_id")?.as_str()?.to_owned(),
                ))
            }),
            _ => None,
        }
        .or_else(|| {
            if target == "self" {
                None
            } else {
                app.state
                    .structured_agents
                    .get(target)
                    .map(|(record, _, _)| (record.agent_id.clone(), record.session_id.clone()))
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
            outcome = outcome
                .as_ref()
                .map_or_else(|code| code.as_str(), |_| "admitted"),
            "wire lifecycle admission"
        );
    }
}
impl Pending {
    fn key(&self) -> (String, String, String) {
        (
            "local".into(),
            self.request.session.info.principal.clone(),
            self.request.command.id.clone(),
        )
    }
    fn finish(
        &mut self,
        journal: &Ownership,
        result: Result<Value, ErrorCode>,
    ) -> Result<(), ErrorCode> {
        self.stage = Stage::Finish(
            journal
                .submit(
                    self.phase,
                    journal::Operation::Finish {
                        key: self.key(),
                        admitted_ms: self.admitted_ms,
                        result,
                    },
                )
                .map_err(|_| ErrorCode::SessionUnconfirmed)?,
        );
        Ok(())
    }
    fn respond(self, frame: ServerFrame, shared: &Shared, app: &App) {
        let expired = self.admitted_ms != 0
            && crate::app::notifications::now_ms().saturating_sub(self.admitted_ms)
                >= IDEMPOTENCY_WINDOW_MS;
        for request in self.joined {
            let frame = if expired {
                ack(
                    request.command.id.clone(),
                    Err(if request.command.resume_only {
                        ErrorCode::SessionUnconfirmed
                    } else {
                        ErrorCode::SessionBusy
                    }),
                )
            } else {
                frame.clone()
            };
            respond(request, frame, shared, app);
        }
        respond(self.request, frame, shared, app);
    }
}
impl Owner {
    #[cfg(test)]
    pub(in crate::web::wire) fn queued_test(&self) -> usize {
        self.rx.iter().map(mpsc::Receiver::len).sum()
    }
    /// Poll durable claims, effects and outcome sync without waiting on the writer.
    pub(super) fn service(&mut self, app: &mut App, shared: &Shared) {
        for lane in [1, 0] {
            for _ in 0..CAPACITY {
                if self
                    .pending
                    .iter()
                    .filter(|p| usize::from(p.request.command.verb == "agent.decide") == lane)
                    .map(|p| 1 + p.joined.len())
                    .sum::<usize>()
                    >= CAPACITY
                {
                    break;
                }
                let Ok(request) = self.rx[lane].try_recv() else {
                    break;
                };
                if request.response.is_closed() {
                    continue;
                }
                let phase = self.gate.load(Ordering::Acquire);
                let admitted = authorize(&request, |_| {
                    if phase & 1 == 0 {
                        Err(ErrorCode::SessionBusy)
                    } else if request.command.read_only {
                        Err(ErrorCode::ServerVersionGap)
                    } else {
                        Ok(())
                    }
                });
                if let Err(code) = admitted {
                    let frame = ack(request.command.id.clone(), Err(code));
                    respond(request, frame, shared, app);
                    continue;
                }
                if let Some(pending) = self.pending.iter_mut().find(|p| {
                    p.phase == phase
                        && p.request.session.info.principal == request.session.info.principal
                        && p.request.command.id == request.command.id
                }) {
                    if journal::digest(&pending.request.command)
                        .and_then(|digest| {
                            journal::digest(&request.command).map(|other| digest == other)
                        })
                        .unwrap_or(false)
                    {
                        pending.joined.push(request);
                    } else {
                        let frame = ack(request.command.id.clone(), Err(ErrorCode::InvalidParams));
                        respond(request, frame, shared, app);
                    }
                    continue;
                }
                let operation = journal::Operation::Claim {
                    namespace: "local".into(),
                    principal: request.session.info.principal.clone(),
                    command: request.command.clone(),
                    now: crate::app::notifications::now_ms(),
                };
                match self.journal.submit(phase, operation) {
                    Ok(reply) => self.pending.push(Pending {
                        request,
                        joined: Vec::new(),
                        phase,
                        admitted_ms: 0,
                        stage: Stage::Claim(reply),
                    }),
                    Err(code) => {
                        let frame = ack(request.command.id.clone(), Err(code));
                        respond(request, frame, shared, app);
                    }
                }
            }
        }
        let mut i = 0;
        while i < self.pending.len() {
            let pending = &mut self.pending[i];
            let completion = match &mut pending.stage {
                Stage::Claim(reply) | Stage::Finish(reply) => match reply.try_recv() {
                    Ok(result) => result,
                    Err(oneshot::error::TryRecvError::Empty) => {
                        i += 1;
                        continue;
                    }
                    Err(oneshot::error::TryRecvError::Closed) => Err(ErrorCode::SessionUnconfirmed),
                },
                Stage::Send(receipt) => {
                    let result = match receipt.get_mut().try_recv() {
                        Ok(result) => result.and_then(|v| {
                            serde_json::to_value(v).map_err(|_| ErrorCode::SerializationError)
                        }),
                        Err(TryRecvError::Empty) => {
                            i += 1;
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
                    };
                    match pending.finish(&self.journal, result) {
                        Ok(()) => {
                            i += 1;
                            continue;
                        }
                        Err(code) => Err(code),
                    }
                }
            };
            let completion = match completion {
                Ok(journal::Admission::Claimed(admitted_ms)) => {
                    pending.admitted_ms = admitted_ms;
                    let request = &pending.request;
                    let outcome = authorize(request, |token| {
                        if self.gate.load(Ordering::Acquire) != pending.phase {
                            return Err(ErrorCode::SessionBusy);
                        }
                        dispatch(
                            app,
                            &request.command,
                            &Principal::Token(token),
                            &request.session,
                        )
                    });
                    let result = match outcome {
                        Ok(Outcome::Queued(receipt)) => {
                            pending.stage = Stage::Send(parking_lot::Mutex::new(receipt));
                            i += 1;
                            continue;
                        }
                        Ok(Outcome::Decided(value)) => Ok(value),
                        Err(code) => Err(code),
                    };
                    match pending.finish(&self.journal, result) {
                        Ok(()) => {
                            i += 1;
                            continue;
                        }
                        Err(code) => Err(code),
                    }
                }
                Ok(journal::Admission::Pending(admitted_ms)) => {
                    pending.admitted_ms = admitted_ms;
                    Err(ErrorCode::SessionUnconfirmed)
                }
                other => other,
            };
            let pending = self.pending.swap_remove(i);
            let frame = match completion {
                Ok(journal::Admission::Replayed(frame)) => frame,
                Err(code) => ack(pending.request.command.id.clone(), Err(code)),
                _ => ack(
                    pending.request.command.id.clone(),
                    Err(ErrorCode::SessionUnconfirmed),
                ),
            };
            pending.respond(frame, shared, app);
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
