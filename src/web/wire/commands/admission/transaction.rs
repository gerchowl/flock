//! Dormant verified destination entry. Mesh VERSION 5 never calls this module.
use super::*;
use crate::{
    api::gateway_grants::ClockPair,
    mesh::command::{self, Body, Frame, Hop, TokenSource},
    structured::permissions::auth::gateway::{
        self,
        lifecycle::{PrincipalKind, Refusal},
    },
};
use std::{sync::Arc, time::Duration};

type Environment = (Option<ClockPair>, Option<String>, Option<Duration>);
type DestinationReader = dyn FnMut(&Body) -> Result<Environment, Refusal> + Send;
struct Ingress {
    frame: Frame,
    since: Option<Duration>,
    command: super::super::Command,
}
/// Destination-owned reader. It checks enrollment, membership and verb policy.
/// App ownership must remain exclusive through claim and dispatch.
struct Destination {
    node: String,
    read: Box<DestinationReader>,
}
fn verified(frame: &Frame) -> Result<Body, Refusal> {
    frame.verify().map_err(|code| match code {
        "not_authorized" => Refusal::Principal,
        "origin_mismatch" => Refusal::Binding,
        "message_expired" => Refusal::Budget,
        _ => Refusal::Provenance,
    })
}
fn enrolled(body: &Body) -> bool {
    crate::mesh::hello::with_store(|store| {
        store
            .explicitly_enrolled(&body.home_node_id)
            .map_err(|e| e.to_string())
    })
    .unwrap_or(false)
}
mod audit;
async fn admit(
    owner: &Ownership,
    phase: u64,
    ingress: Ingress,
    store: Arc<gateway::Store>,
    generation: u64,
    destination: Destination,
) -> Result<(journal::Admission, Option<GrantAdmission>), ErrorCode> {
    let audit = audit::Context::new(&ingress, generation);
    let result = admit_inner(owner, phase, ingress, store, generation, destination).await;
    audit.record(
        result
            .as_ref()
            .map(|(admission, _)| admission)
            .map_err(|e| *e),
    );
    result
}
async fn admit_inner(
    owner: &Ownership,
    phase: u64,
    ingress: Ingress,
    store: Arc<gateway::Store>,
    generation: u64,
    mut destination: Destination,
) -> Result<(journal::Admission, Option<GrantAdmission>), ErrorCode> {
    let body = match verified(&ingress.frame) {
        Ok(body) => body,
        Err(reason) => return Ok((journal::Admission::Refused(reason), None)),
    };
    let command = ingress.command;
    let target = match &body.target {
        command::Target::Agent(id) | command::Target::Workspace(id) => id.clone(),
    };
    let binding = body.destination_node_id == destination.node
        && command
            .args
            .get("node_id")
            .is_none_or(|node| node.as_str() == Some(body.destination_node_id.as_str()))
        && body.cmd_id == command.id
        && body.verb == command.verb
        && command::args_digest(&command.args).ok() == Some(body.args_digest)
        && match &body.target {
            command::Target::Workspace(id) => {
                command.verb == "agent.start"
                    && command
                        .args
                        .get("workspace_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(id)
            }
            command::Target::Agent(id) => {
                command.verb != "agent.start"
                    && command
                        .args
                        .get("target")
                        .and_then(serde_json::Value::as_str)
                        == Some(id)
            }
        };
    if !binding {
        return Ok((journal::Admission::Refused(Refusal::Binding), None));
    }
    if !super::super::lifecycle(&command.verb) || command.read_only {
        return Ok((journal::Admission::Refused(Refusal::Verb), None));
    }
    if body.requested_scope != crate::api::client_tokens::Scope::Operate {
        return Ok((journal::Admission::Refused(Refusal::Scope), None));
    }
    let source = match body.token_source {
        TokenSource::App => PrincipalKind::User,
        TokenSource::Agent => PrincipalKind::Agent,
        // Defence in depth: Body::validate also rejects File lifecycle tokens.
        TokenSource::File => return Ok((journal::Admission::Refused(Refusal::Principal), None)),
    };
    let mut hop =
        Hop::receive(ingress.frame, ingress.since).map_err(|_| ErrorCode::NotAuthorized)?;
    let claim = journal::grants::Claim {
        store,
        generation,
        issuance: String::new(),
        home: body.home_node_id.clone(),
        principal: body.principal.clone(),
        source,
        scopes: body.scopes.clone(),
        destination: destination.node,
        target,
        command,
        now: crate::app::notifications::now_ms(),
        environment: Box::new(move || {
            let (clock, member, residence) = (destination.read)(&body)?;
            hop.recheck(residence).map_err(|_| Refusal::Budget)?;
            Ok((clock, member))
        }),
    };
    GrantAdmission::claim(owner, phase, claim).await
}
/// In-process destination dispatcher, deliberately disconnected from mesh ingress.
/// Holding App exclusively keeps its membership/policy cut stable until dispatch.
async fn execute(
    app: &mut crate::app::App,
    owner: &Ownership,
    ingress: Ingress,
) -> Result<journal::Admission, ErrorCode> {
    let phase = owner.gate().load(std::sync::atomic::Ordering::Acquire);
    let body = match verified(&ingress.frame) {
        Ok(body) => body,
        Err(reason) => {
            let result = journal::Admission::Refused(reason);
            audit::Context::new(&ingress, 0).record(Ok(&result));
            return Ok(result);
        }
    };
    let member = app.gateway_lifecycle_member(&body);
    let destination = Destination {
        node: app.node_id.clone().ok_or(ErrorCode::NotAuthorized)?,
        read: Box::new(move |body| {
            if !enrolled(body) {
                return Err(Refusal::Enrollment);
            }
            Ok((
                gateway::lifecycle::retirement_clock(),
                Some(member.clone()?),
                crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel).ok(),
            ))
        }),
    };
    let store = Arc::new(gateway::Store::open(owner.data())?);
    let generation = store.generation()?;
    let mut audit = audit::Context::new(&ingress, generation);
    #[cfg(all(test, debug_assertions))]
    tests::before_admit(owner).await;
    let (result, grant) = admit(owner, phase, ingress, store, generation, destination).await?;
    let Some(grant) = grant else {
        return Ok(result);
    };
    audit.remember(&result);
    let result = complete(app, owner, phase, body, grant, result).await;
    audit.completed(result.as_ref().map_err(|code| *code));
    result
}
async fn complete(
    app: &mut crate::app::App,
    owner: &Ownership,
    phase: u64,
    body: Body,
    grant: GrantAdmission,
    result: journal::Admission,
) -> Result<journal::Admission, ErrorCode> {
    let journal::Admission::Granted { state, .. } = result else {
        return Err(ErrorCode::SessionUnconfirmed);
    };
    let journal::Admission::Claimed(admitted_ms) = *state else {
        return Err(ErrorCode::SessionUnconfirmed);
    };
    if owner.gate().load(std::sync::atomic::Ordering::Acquire) != phase {
        return Err(ErrorCode::SessionUnconfirmed);
    }
    let key = (grant.home.clone(), grant.principal.clone(), body.cmd_id);
    let outcome = app.wire_agent_lifecycle(
        grant.command.clone(),
        crate::structured::permissions::auth::lifecycle::Authority::Grant(Box::new(grant)),
    );
    let state = owner
        .submit(
            phase,
            journal::Operation::Finish {
                key,
                admitted_ms,
                result: outcome,
            },
        )?
        .await
        .map_err(|_| ErrorCode::SessionUnconfirmed)??;
    Ok(state)
}

// VERSION 6 negotiation overrides exist only in debug builds.
#[cfg(all(test, debug_assertions))]
pub(super) mod tests;
