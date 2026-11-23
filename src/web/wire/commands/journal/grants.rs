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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockPair>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
}
/// Immutable receipt context, retained by both the J1 claim and its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::web::wire) struct Binding {
    pub home: String,
    pub principal: String,
    pub cmd_id: String,
    pub args_digest: [u8; 32],
    pub verb: String,
    pub generation: u64,
}
type EnvironmentReader = dyn FnMut() -> Result<(Option<ClockPair>, Option<String>), Refusal> + Send;

/// Authenticated preflight facts, never constructed from unverified mesh JSON.
/// `environment` rechecks destination enrollment, membership, policy and budget
/// under the generation lease at preflight and immediately before the claim.
pub(in crate::web::wire) struct Claim {
    pub store: std::sync::Arc<Store>,
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
    pub environment: Box<EnvironmentReader>,
}
impl Journal {
    pub(super) fn grant(&mut self, mut c: Claim) -> Result<Admission, ErrorCode> {
        if self.blocked {
            return Err(ErrorCode::SessionBusy);
        }
        let store = c.store.clone();
        store.with_accounting(|listing, fence| {
            if listing.generation != c.generation {
                return Ok(Admission::Refused(Refusal::Revoked));
            }
            let key = (c.home.clone(), c.principal.clone(), c.command.id.clone());
            let existing = self.index.records.get(&key);
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
                let Some(binding) = &use_.binding else {
                    return Ok(Admission::Refused(Refusal::AccountingLost));
                };
                if binding.generation != listing.generation {
                    return Ok(Admission::Refused(Refusal::Revoked));
                }
                return self.replay_grant(&mut c, listing, record.clone());
            }
            if c.command.resume_only {
                return Err(ErrorCode::SessionUnconfirmed);
            }
            let consumed = &self.index.consumed;
            let selected = (!c.issuance.is_empty()).then_some(c.issuance.as_str());
            let request = gateway::Request {
                home: &c.home,
                principal: &c.principal,
                scopes: &c.scopes,
                verb: &c.command.verb,
                target: gateway::lifecycle::target(&c.command.verb, &c.target),
            };
            let (clock, workspace) = match (c.environment)() {
                Ok(environment) => environment,
                Err(reason) => return Ok(Admission::Refused(reason)),
            };
            let mut candidates = listing.clone();
            let mut clock_refused = false;
            // Each retry removes an entry, followed by one final no-match evaluation.
            let mut budget = 0..=candidates.entries.len();
            let (issuance, timed) = loop {
                if budget.next().is_none() {
                    return Ok(Admission::Refused(Refusal::SelectionLimit));
                }
                #[cfg(test)]
                if self.selection_stalled() {
                    continue;
                }
                let preflight = gateway::lifecycle::evaluate_listing(
                    &Evaluation {
                        request: &request,
                        source: c.source,
                        destination: &c.destination,
                        clock: clock.clone(),
                        consumed,
                    },
                    c.generation,
                    &candidates,
                    selected,
                    |_| workspace.clone(),
                );
                let selected = match &preflight {
                    Decision::Allowed { issuance_id, .. } => Some(issuance_id.as_str()),
                    Decision::Refused(_) => selected,
                };
                let timed = listing
                    .entries
                    .values()
                    .flatten()
                    .filter_map(|g| g.lifecycle.as_ref())
                    .any(|p| {
                        p.lifetime_ms.is_some()
                            && p.issuance
                                .as_ref()
                                .is_some_and(|i| Some(i.id.as_str()) == selected)
                    });
                if timed
                    && !fence.clock_check(
                        listing,
                        selected.ok_or(ErrorCode::SessionUnconfirmed)?,
                        clock.as_ref(),
                    )?
                {
                    if selected.is_some() && !c.issuance.is_empty() {
                        return Ok(Admission::Refused(Refusal::Expired));
                    }
                    candidates.entries.retain(|_, grant| {
                        grant
                            .as_ref()
                            .and_then(|g| g.lifecycle.as_ref())
                            .and_then(|p| p.issuance.as_ref())
                            .is_none_or(|i| Some(i.id.as_str()) != selected)
                    });
                    clock_refused = true;
                    continue;
                }
                let issuance = match preflight {
                    Decision::Refused(reason) => {
                        return Ok(Admission::Refused(if clock_refused {
                            Refusal::Expired
                        } else {
                            reason
                        }))
                    }
                    Decision::Allowed { issuance_id, .. } => issuance_id,
                };
                break (issuance, timed);
            };
            let (clock, workspace) = match (c.environment)() {
                Ok(environment) => environment,
                Err(reason) => return Ok(Admission::Refused(reason)),
            };
            if timed && !fence.clock_check(listing, &issuance, clock.as_ref())? {
                return Ok(Admission::Refused(Refusal::Expired));
            }
            let evidence = timed.then(|| clock.clone()).flatten();
            if let Decision::Refused(reason) = gateway::lifecycle::evaluate_listing(
                &Evaluation {
                    request: &request,
                    source: c.source,
                    destination: &c.destination,
                    clock,
                    consumed,
                },
                c.generation,
                listing,
                Some(&issuance),
                |_| workspace,
            ) {
                return Ok(Admission::Refused(reason));
            }
            let counted = listing.entries.values().flatten().any(|g| {
                g.lifecycle.as_ref().is_some_and(|p| {
                    p.max_uses.is_some() && p.issuance.as_ref().is_some_and(|i| i.id == issuance)
                })
            });
            if counted && !fence.check(&issuance, self.index.stamp(&issuance))? {
                return Ok(Admission::Refused(Refusal::AccountingLost));
            }
            let consumption = Consumption {
                ordinal: consumed
                    .get(&issuance)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or(ErrorCode::SessionBusy)?,
                issuance,
                destination: c.destination,
                target: c.target,
                clock: evidence,
                binding: Some(Binding {
                    home: c.home.clone(),
                    principal: c.principal.clone(),
                    cmd_id: c.command.id.clone(),
                    args_digest: crate::mesh::command::args_digest(&c.command.args)
                        .map_err(|_| ErrorCode::SerializationError)?,
                    verb: c.command.verb.clone(),
                    generation: c.generation,
                }),
            };
            let state = self.claim_with(
                &c.home,
                &c.principal,
                &c.command,
                c.now,
                Some(consumption.clone()),
                counted.then_some(fence),
            )?;
            Ok(Admission::Granted {
                consumption,
                state: Box::new(state),
            })
        })
    }
    /// Expiry and remaining uses apply only at claim. Replay rechecks identity,
    /// generation, membership, enrollment and disclosure policy without a use.
    fn replay_grant(
        &mut self,
        c: &mut Claim,
        listing: &crate::api::gateway_grants::Listing,
        record: super::Record,
    ) -> Result<Admission, ErrorCode> {
        let consumption = record.consumption.ok_or(ErrorCode::SessionUnconfirmed)?;
        let mut current = listing.clone();
        let mut present = false;
        for policy in current
            .entries
            .values_mut()
            .flatten()
            .filter_map(|g| g.lifecycle.as_mut())
        {
            if policy
                .issuance
                .as_ref()
                .is_some_and(|i| i.id == consumption.issuance)
            {
                policy.lifetime_ms = None;
                policy.max_uses = None;
                present = true;
            }
        }
        if !present {
            return Ok(Admission::Refused(Refusal::Revoked));
        }
        let (clock, workspace) = match (c.environment)() {
            Ok(environment) => environment,
            Err(reason) => return Ok(Admission::Refused(reason)),
        };
        let request = gateway::Request {
            home: &c.home,
            principal: &c.principal,
            scopes: &c.scopes,
            verb: &c.command.verb,
            target: gateway::lifecycle::target(&c.command.verb, &c.target),
        };
        if let Decision::Refused(reason) = gateway::lifecycle::evaluate_listing(
            &Evaluation {
                request: &request,
                source: c.source,
                destination: &c.destination,
                clock,
                consumed: &BTreeMap::new(),
            },
            c.generation,
            &current,
            Some(&consumption.issuance),
            |_| workspace,
        ) {
            return Ok(Admission::Refused(reason));
        }
        let state = self.claim_with(
            &c.home,
            &c.principal,
            &c.command,
            c.now,
            Some(consumption.clone()),
            None,
        )?;
        Ok(Admission::Granted {
            consumption,
            state: Box::new(state),
        })
    }
}
/// Rebuilt from synced claims and compacted issuance checkpoints. Outcomes and
/// command bindings have J1's 24-hour replay window. Beyond it, resume-only is
/// uncertain and never dispatches. Compaction retains totals for live issuances
/// and any issuance with a retained retry row, dropping revoked or provably
/// expired issuances only once no retry row needs them. Incomparable clocks keep
/// history. Grant commands and issuance checkpoints have separate 4096-entry
/// budgets and a separate byte reserve; they never occupy local operator slots.
/// Counted integrity watermarks live in A1 and survive deletion of all of J1.
/// Missing/behind history requires operator revoke/re-add, never counter erasure.
pub(in crate::web::wire) type Totals = BTreeMap<String, u64>;

/// Conservative retirement snapshot. Unknown A1 rows or storage failure preserve
/// history. A new issuance arriving after this snapshot is never discarded.
pub(super) fn retired(data: &std::path::Path, known: &BTreeSet<String>) -> BTreeSet<String> {
    let Ok(store) = Store::open(data) else {
        return BTreeSet::new();
    };
    store
        .with_listing(|listing| {
            if listing.entries.values().any(Option::is_none) {
                return Ok(BTreeSet::new());
            }
            let clock = gateway::lifecycle::retirement_clock();
            let live: BTreeSet<_> = listing
                .entries
                .values()
                .flatten()
                .filter_map(|g| g.lifecycle.as_ref())
                .filter(|p| !gateway::lifecycle::expired(p, clock.as_ref()))
                .filter_map(|p| p.issuance.as_ref().map(|i| i.id.clone()))
                .collect();
            Ok(known.difference(&live).cloned().collect())
        })
        .unwrap_or_default()
}
