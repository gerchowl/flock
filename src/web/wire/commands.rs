//! Bounded app-owner command admission (ADR-0036 §5/§5a, S7-min slice 5).
use super::*;
mod journal;
// Dormant until M2a-2 verifies destination ingress.
#[allow(dead_code)]
pub(super) mod admission;
mod ownership;
use crate::structured::publication::causal::{Ack, Cause};
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
/// Socket revocation follows the requesting credential's current authorization.
struct Reply {
    frame: ServerFrame,
    credential_revoked: bool,
    causal: Option<Ack>,
}
struct Request {
    command: Command,
    store: Arc<Store>,
    session: Session,
    response: oneshot::Sender<Reply>,
    origin: u64,
    cause: Cause,
    causal: Option<Ack>,
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
fn dispatch(
    app: &mut App,
    command: &Command,
    principal: &Principal<'_>,
    session: &Session,
    cause: &Cause,
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
            .wire_agent_lifecycle(
                typed,
                crate::structured::permissions::auth::lifecycle::Authority::Local(session),
            )
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
            runtime.agent_decide_caused(principal, &backend, cause.clone())?;
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
fn authorize<T>(
    request: &Request,
    apply: impl for<'a> FnOnce(permissions::auth::VerifiedToken<'a>) -> Result<T, ErrorCode>,
) -> Result<T, ErrorCode> {
    let verb = request.command.verb.as_str();
    let required = match verb {
        _ if lifecycle(verb) => Scope::Operate,
        "agent.decide" => Scope::Answer,
        _ => Scope::Send,
    };
    request
        .store
        .admit(&request.session, required, |token| {
            match (verb, token.decision_subject().is_some()) {
                ("agent.decide", false) => Err(ErrorCode::NotAuthorized),
                _ => apply(token),
            }
        })
        .map_err(|error| match error {
            TokenError::Revoked | TokenError::Unauthorized => ErrorCode::AuthRevoked,
            TokenError::Scope if lifecycle(&request.command.verb) => ErrorCode::NotAuthorized,
            TokenError::Scope => ErrorCode::Scope,
            TokenError::Busy => ErrorCode::SessionBusy,
            TokenError::Unsafe | TokenError::Io(_) => ErrorCode::InternalError,
        })?
}
fn respond(request: Request, frame: ServerFrame, shared: &Shared, app: &App) {
    let authorization = authorize(&request, |_| Ok(()));
    let credential_revoked = matches!(authorization, Err(ErrorCode::AuthRevoked));
    let frame = match authorization {
        Ok(()) => frame,
        Err(code) => ack(request.command.id.clone(), Err(code)),
    };
    trace_reply(app, &request, &frame, shared);
    let _ = request.response.send(Reply {
        frame,
        credential_revoked,
        causal: request.causal,
    });
}

fn trace_reply(app: &App, request: &Request, frame: &ServerFrame, shared: &Shared) {
    let ServerFrame::Ack { result, error, .. } = frame else {
        return;
    };
    let registry = &app.state.structured_agents;
    if lifecycle(&request.command.verb)
        || matches!(request.command.verb.as_str(), "agent.send" | "agent.decide")
    {
        let target = request
            .command
            .args
            .get("target")
            .or_else(|| request.command.args.get("agent"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Registry context keeps refused decisions attributable without granting authority.
        let identity = result
            .as_ref()
            .and_then(|value| value.get("agent"))
            .and_then(|agent| {
                Some((
                    agent.get("agent_id")?.as_str()?.to_owned(),
                    agent.get("session_id")?.as_str()?.to_owned(),
                ))
            })
            .or_else(|| {
                if target == "self" {
                    return None;
                }
                registry
                    .get(target)
                    .map(|(record, _, _)| (record.agent_id.clone(), record.session_id.clone()))
                    .or_else(|| {
                        registry
                            .records()
                            .into_iter()
                            .find(|record| record.name.as_deref() == Some(target))
                            .map(|record| (record.agent_id, record.session_id))
                    })
            });
        let agent_id = identity
            .as_ref()
            .map(|(agent, _)| agent.clone())
            .or_else(|| app.locate_agent(target).map(|location| location.agent_id))
            .unwrap_or_default();
        crate::logging::wire_lifecycle_admission(crate::logging::LifecycleAudit {
            credential: request.session.info.name.as_str(),
            principal: request.session.info.principal.as_str(),
            target,
            agent_id: agent_id.as_str(),
            session_id: identity
                .as_ref()
                .map_or("", |(_, session)| session.as_str()),
            verb: request.command.verb.as_str(),
            cmd_id: request.command.id.as_str(),
            outcome: error
                .as_ref()
                .map_or("admitted", |error| error.code.as_str()),
            identity: "verified",
            home: app.node_id.as_deref().unwrap_or_default(),
            ..Default::default()
        });
    }
    if let Some(code) = error
        .as_ref()
        .and_then(|error| ErrorCode::from_code(&error.code))
    {
        shared.refusals.record(
            shared.clock.now(),
            &request.session.info,
            &request.command.verb,
            &request.command.args,
            code,
        );
    }
}
impl Pending {
    fn finish(
        &mut self,
        journal: &Ownership,
        result: Result<Value, ErrorCode>,
    ) -> Result<(), ErrorCode> {
        let operation = journal::Operation::Finish {
            key: (
                "local".into(),
                self.request.session.info.principal.clone(),
                self.request.command.id.clone(),
            ),
            admitted_ms: self.admitted_ms,
            result,
        };
        self.stage = Stage::Finish(
            journal
                .submit(self.phase, operation)
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
    pub(super) fn service(&mut self, app: &mut App, shared: &Shared, nodes: &mut fleet::Nodes) {
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
                let Ok(mut request) = self.rx[lane].try_recv() else {
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
                        let joined = match pending.request.cause.join() {
                            Ok(ack) => bind(shared, request.origin, ack),
                            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(None),
                            Err(_) => Err(ErrorCode::SessionBusy),
                        };
                        match joined {
                            Ok(ack) => {
                                request.causal = ack;
                                pending.joined.push(request);
                            }
                            Err(code) => {
                                let frame = ack(request.command.id.clone(), Err(code));
                                respond(request, frame, shared, app);
                            }
                        }
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
            i += 1;
            let completion = match &mut pending.stage {
                Stage::Claim(reply) | Stage::Finish(reply) => match reply.try_recv() {
                    Ok(result) => result,
                    Err(oneshot::error::TryRecvError::Empty) => {
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
                            continue;
                        }
                        Err(TryRecvError::Disconnected) => Err(ErrorCode::InputWriteUnknown),
                    };
                    if let Err(code) = pending.finish(&self.journal, result) {
                        Err(code)
                    } else {
                        continue;
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
                        let publication = shared
                            .fleet
                            .reserve(request.cause.clone())
                            .map_err(|_| ErrorCode::SessionBusy)?;
                        let outcome = dispatch(
                            app,
                            &request.command,
                            &Principal::Token(token),
                            &request.session,
                            &request.cause,
                        );
                        let projection = nodes.observe(app, shared.clock.now());
                        publication.observe(
                            &app.state.structured_agents,
                            &app.structured_workers,
                            self.journal.data(),
                            app.node_id.as_deref(),
                            Some(projection),
                        );
                        outcome
                    });
                    let result = match outcome {
                        Ok(Outcome::Queued(receipt)) => {
                            pending.stage = Stage::Send(parking_lot::Mutex::new(receipt));
                            continue;
                        }
                        Ok(Outcome::Decided(value)) => Ok(value),
                        Err(code) => Err(code),
                    };
                    if let Err(code) = pending.finish(&self.journal, result) {
                        Err(code)
                    } else {
                        continue;
                    }
                }
                Ok(journal::Admission::Pending(admitted_ms)) => {
                    pending.admitted_ms = admitted_ms;
                    Err(ErrorCode::SessionUnconfirmed)
                }
                other => other,
            };
            i -= 1;
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
fn bind(shared: &Shared, origin: u64, ack: Ack) -> Result<Option<Ack>, ErrorCode> {
    shared
        .clients
        .lock()
        .map_err(|_| ErrorCode::InternalError)?
        .get_mut(&origin)
        .map(|client| client.causal.bind(ack).map_err(|_| ErrorCode::SessionBusy))
        .transpose()
}
/// Wait for the committed outcome independently of the reader and heartbeat.
/// The triggering acknowledgement must be flushed before broadcasting revocation.
pub(super) async fn execute(
    shared: Arc<Shared>,
    session: Session,
    data: mpsc::Sender<output::Delivery>,
    command: Command,
    origin: u64,
) -> Result<(), ()> {
    let (cause, guard) = Cause::hold();
    let causal = bind(&shared, origin, guard).map_err(|_| ())?.ok_or(())?;
    let (response, received) = oneshot::channel();
    let lane = usize::from(command.verb == "agent.decide");
    let Reply {
        frame,
        credential_revoked,
        causal,
    } = match shared.commands.tx[lane].try_send(Request {
        command,
        store: shared.store.clone(),
        session: session.clone(),
        response,
        origin,
        cause,
        causal: Some(causal),
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
            Reply {
                frame: ack(request.command.id, Err(ErrorCode::SessionBusy)),
                credential_revoked: false,
                causal: request.causal,
            }
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
    let (delivered, flushed) = oneshot::channel();
    let flushed = async {
        data.send(output::Delivery {
            frame,
            active: None,
            delivered: Some(delivered),
            causal,
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
    if credential_revoked {
        shared.revoke(&session);
    }
    flushed
}
