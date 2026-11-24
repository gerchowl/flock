//! Gateway context on the existing lifecycle admission event, without payloads.
use super::*;
pub(super) struct Context {
    body: Option<Body>,
    verified: bool,
    generation: u64,
    receipt: Option<journal::grants::Consumption>,
}
impl Context {
    pub(super) fn new(ingress: &Ingress, generation: u64) -> Self {
        let body = verified(&ingress.frame).ok();
        Self {
            verified: body.is_some(),
            body: body.or_else(|| ingress.frame.claimed_body()),
            generation,
            receipt: None,
        }
    }
    pub(super) fn record(&self, result: Result<&journal::Admission, ErrorCode>) {
        self.emit(result, None);
    }
    pub(super) fn remember(&mut self, result: &journal::Admission) {
        if let journal::Admission::Granted { consumption, .. } = result {
            self.receipt = Some(consumption.clone());
        }
    }
    pub(super) fn completed(&self, result: Result<&journal::Admission, ErrorCode>) {
        self.emit(result, result.is_ok().then_some("result"));
    }
    fn emit(&self, result: Result<&journal::Admission, ErrorCode>, outcome_override: Option<&str>) {
        let (consumption, state) = match result {
            Ok(journal::Admission::Granted { consumption, state }) => {
                (Some(consumption), Ok(state.as_ref()))
            }
            Ok(journal::Admission::RefusedReplay { consumption, .. }) => {
                (Some(consumption), result)
            }
            other => (self.receipt.as_ref(), other),
        };
        let mut verified = self.verified && self.body.as_ref().is_some_and(enrolled);
        let (outcome, detail) = match state {
            Ok(journal::Admission::Claimed(_)) => ("admitted", String::new()),
            Ok(journal::Admission::Pending(_)) => ("replay", "pending".into()),
            Ok(journal::Admission::Replayed(crate::web::wire::ServerFrame::Ack {
                error: Some(error),
                ..
            })) => ("replay", error.code.clone()),
            Ok(journal::Admission::Replayed(_)) => ("replay", "result".into()),
            Ok(
                journal::Admission::Refused(reason)
                | journal::Admission::RefusedReplay { reason, .. },
            ) => {
                verified &= *reason != Refusal::Enrollment;
                ("refused", format!("{reason:?}"))
            }
            Err(reason) => ("refused", reason.as_str().into()),
            _ => ("refused", "session_unconfirmed".into()),
        };
        let body = self.body.as_ref();
        let target = body.map(|b| match &b.target {
            command::Target::Agent(id) | command::Target::Workspace(id) => id.as_str(),
        });
        crate::logging::wire_lifecycle_admission(crate::logging::LifecycleAudit {
            identity: if verified { "verified" } else { "UNVERIFIED" },
            home: body.map_or("", |b| b.home_node_id.as_str()),
            principal: body.map_or("", |b| b.principal.as_str()),
            issuance: consumption.map(|c| c.issuance.as_str()),
            use_ordinal: consumption.map(|c| c.ordinal),
            generation: verified.then_some(self.generation),
            verb: body.map_or("", |b| b.verb.as_str()),
            target: target.unwrap_or_default(),
            cmd_id: body.map_or("", |b| b.cmd_id.as_str()),
            outcome: outcome_override.unwrap_or(outcome),
            detail: &detail,
            ..Default::default()
        });
    }
}
