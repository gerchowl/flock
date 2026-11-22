//! Dormant A1/J1 transaction. M2 supplies verified facts and destination membership.
use super::{digest, Admission, Journal};
use crate::structured::permissions::auth::gateway;
use crate::web::wire::commands::{Command, ErrorCode};
use crate::{
    api::gateway_grants::ClockPair,
    structured::permissions::auth::{
        gateway::{
            lifecycle::{Decision, Evaluation, PrincipalKind, Refusal},
            Store,
        },
        tokens::Scope,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Durable consumption identity. Outcomes retain the original claim metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web::wire) struct Consumption {
    pub issuance: String,
    pub ordinal: u64,
    pub destination: String,
    pub target: String,
}
/// Authenticated preflight facts, never constructed from unverified mesh JSON.
/// `environment` runs under the generation lease immediately before evaluation.
/// P3c will supply durable paired-clock evidence; M2 adds budget/enrollment checks.
pub(in crate::web::wire) struct Claim {
    pub store: Store,
    pub generation: u64,
    pub issuance: String,
    pub home: String,
    pub principal: String,
    pub source: PrincipalKind,
    pub scopes: BTreeSet<Scope>,
    pub destination: String,
    pub target: String,
    pub command: Command,
    pub now: u64,
    pub environment: Box<dyn FnOnce() -> (Option<ClockPair>, Option<String>) + Send>,
}
impl Journal {
    pub(super) fn grant(&mut self, c: Claim) -> Result<Admission, ErrorCode> {
        if self.blocked {
            return Err(ErrorCode::SessionBusy);
        }
        c.store.with_listing(|listing| {
            if listing.generation != c.generation {
                return Ok(Admission::Refused(Refusal::Revoked));
            }
            let key = (c.home.clone(), c.principal.clone(), c.command.id.clone());
            let existing = self.index.records.get(&key);
            let mut consumed = self.index.consumed.clone();
            if let Some(record) = existing {
                let Some(use_) = &record.consumption else {
                    return Err(ErrorCode::InvalidParams);
                };
                if record.digest != digest(&c.command)?
                    || use_.destination != c.destination
                    || use_.target != c.target
                {
                    return Err(ErrorCode::InvalidParams);
                }
                // Recheck present authority without charging the retry's own use.
                let total = consumed
                    .get_mut(&use_.issuance)
                    .ok_or(ErrorCode::SessionUnconfirmed)?;
                *total = total.checked_sub(1).ok_or(ErrorCode::SessionUnconfirmed)?;
            }
            let selected = existing
                .and_then(|r| r.consumption.as_ref())
                .map_or(c.issuance.as_str(), |u| u.issuance.as_str());
            let request = gateway::Request {
                home: &c.home,
                principal: &c.principal,
                scopes: &c.scopes,
                verb: &c.command.verb,
                target: gateway::lifecycle::target(&c.command.verb, &c.target),
            };
            let (clock, workspace) = (c.environment)();
            let decision = gateway::lifecycle::evaluate_listing(
                &Evaluation {
                    request: &request,
                    source: c.source,
                    destination: &c.destination,
                    clock,
                    consumed: &consumed,
                },
                c.generation,
                listing,
                Some(selected),
                |_| workspace,
            );
            let issuance = match decision {
                Decision::Refused(reason) => return Ok(Admission::Refused(reason)),
                Decision::Allowed { issuance_id, .. } => issuance_id,
            };
            let consumption = match existing {
                Some(r) => r.consumption.clone().ok_or(ErrorCode::SessionUnconfirmed)?,
                None => Consumption {
                    ordinal: consumed
                        .get(&issuance)
                        .copied()
                        .unwrap_or(0)
                        .checked_add(1)
                        .ok_or(ErrorCode::SessionBusy)?,
                    issuance,
                    destination: c.destination,
                    target: c.target,
                },
            };
            let state = self.claim_with(
                &c.home,
                &c.principal,
                &c.command,
                c.now,
                Some(consumption.clone()),
            )?;
            Ok(Admission::Granted {
                consumption,
                state: Box::new(state),
            })
        })
    }
}
/// Rebuilt only from committed claims, never a second durable grants-file update.
/// P3b will retain these authoritative totals through bounded compaction.
pub(in crate::web::wire) type Totals = BTreeMap<String, u64>;
