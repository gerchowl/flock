//! Q9 non-consuming preflight, rechecked under the atomic J1 admission fence.
use super::*;

/// Authenticated provenance supplied by P1, never inferred from principal text.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PrincipalKind {
    User,
    Agent,
    File,
}
/// P3 supplies recovered use totals under the same generation lease.
pub(crate) struct Evaluation<'a> {
    pub request: &'a Request<'a>,
    pub source: PrincipalKind,
    pub destination: &'a str,
    pub clock: Option<ClockPair>,
    pub consumed: &'a BTreeMap<String, u64>,
}
/// Closed reasons for refusing a lifecycle preflight or fenced admission.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Refusal {
    Provenance,
    Binding,
    Enrollment,
    Budget,
    Revoked,
    Principal,
    Destination,
    Scope,
    Verb,
    Target,
    Expired,
    Exhausted,
    NoGrant,
    AccountingLost,
}
/// Preflight only: an allowed decision does not consume or authorize dispatch.
#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    Allowed {
        issuance_id: String,
        remaining: Option<u64>,
    },
    Refused(Refusal),
}
/// Destination-only paired clock with a boot identity stable across process restarts.
pub(super) fn clock_pair() -> Option<ClockPair> {
    let domain = crate::platform::boot_identity::read().ok()?;
    Some(ClockPair {
        domain,
        wall_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?,
        elapsed_ms: crate::platform::suspend_clock::now(&crate::platform::suspend_clock::Kernel)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?,
    })
}
pub(super) fn unexpired(policy: &Lifecycle, now: Option<&ClockPair>) -> bool {
    if policy.lifetime_ms.is_none() {
        return true;
    }
    let Some(issuance) = &policy.issuance else {
        return false;
    };
    let (Some(issued), Some(now), Some(wall), Some(elapsed)) = (
        &issuance.issued,
        now,
        issuance.expires_at_ms,
        issuance.elapsed_deadline_ms,
    ) else {
        return false;
    };
    issued.domain == now.domain
        && now.elapsed_ms >= issued.elapsed_ms
        && now.wall_ms.checked_add(1000).is_some_and(|n| n < wall)
        && now.elapsed_ms < elapsed
}
impl Store {
    /// Non-consuming dormant evaluation. M2 must recheck inside atomic J1 admission.
    pub(crate) fn evaluate(
        &self,
        context: &Evaluation<'_>,
        generation: u64,
        membership: impl FnOnce(Target<'_>) -> Option<String>,
    ) -> Result<Decision, E> {
        let (_lease, mut value) = self.snapshot()?;
        let listing = Self::listing(&value);
        let decision = evaluate_listing(context, generation, &listing, None, membership);
        if let Decision::Allowed { issuance_id, .. } = &decision {
            let timed = listing
                .entries
                .values()
                .flatten()
                .filter_map(|g| g.lifecycle.as_ref())
                .any(|p| {
                    p.lifetime_ms.is_some()
                        && p.issuance.as_ref().is_some_and(|i| &i.id == issuance_id)
                });
            if timed
                && !super::clocks::preflight(
                    self,
                    &mut value,
                    &listing,
                    issuance_id,
                    context.clock.as_ref(),
                )?
            {
                return Ok(Decision::Refused(Refusal::Expired));
            }
        }
        Ok(decision)
    }
    /// Hold the A1 lease through the caller's single durable J1 commit.
    pub(crate) fn with_listing<R>(
        &self,
        action: impl FnOnce(&Listing) -> Result<R, E>,
    ) -> Result<R, E> {
        let (_lease, value) = self.snapshot()?;
        action(&Self::listing(&value))
    }
}
/// Caller must hold the A1 lease until admission is durably committed.
pub(crate) fn evaluate_listing(
    context: &Evaluation<'_>,
    generation: u64,
    listing: &Listing,
    selected: Option<&str>,
    membership: impl FnOnce(Target<'_>) -> Option<String>,
) -> Decision {
    if listing.generation != generation {
        return Decision::Refused(Refusal::Revoked);
    }
    let r = context.request;
    if context.source == PrincipalKind::File {
        return Decision::Refused(Refusal::Principal);
    }
    let Some(scope) = required_scope(r.verb) else {
        return Decision::Refused(Refusal::Verb);
    };
    if !r.scopes.contains(&scope) {
        return Decision::Refused(Refusal::Scope);
    }
    if (r.verb == "agent.start") != matches!(r.target, Target::Start(_)) {
        return Decision::Refused(Refusal::Target);
    }
    let Some(workspace) = membership(r.target) else {
        return Decision::Refused(Refusal::Target);
    };
    let mut grants: Vec<_> = listing
        .entries
        .values()
        .flatten()
        .filter_map(|g| {
            g.lifecycle
                .as_ref()
                .and_then(|p| p.issuance.as_ref().map(|i| (g, p, i)))
        })
        .filter(|(_, _, issuance)| selected.is_none_or(|id| id == issuance.id))
        .collect();
    grants.sort_by(|a, b| a.2.id.cmp(&b.2.id));
    let mut reason = Refusal::NoGrant;
    for (g, p, i) in grants {
        if g.home_node_id != r.home {
            continue;
        }
        reason = if !g.principals.contains(r.principal)
            || !match &p.subject {
                Subject::User => context.source == PrincipalKind::User,
                Subject::Agent => context.source == PrincipalKind::Agent,
                Subject::Role { kind, .. } => matches!(
                    (kind, context.source),
                    (SubjectKind::User, PrincipalKind::User)
                        | (SubjectKind::Agent, PrincipalKind::Agent)
                ),
            } {
            Refusal::Principal
        } else if p.destination_node_id != context.destination {
            Refusal::Destination
        } else if !g.verbs.contains(r.verb) {
            Refusal::Verb
        } else if !g.scopes.contains(&scope) {
            Refusal::Scope
        } else if !matches_target(g, r.target, &workspace) {
            Refusal::Target
        } else if !unexpired(p, context.clock.as_ref()) {
            Refusal::Expired
        } else {
            let used = context.consumed.get(&i.id).copied().unwrap_or(0);
            let remaining = p.max_uses.map(|max| max.saturating_sub(used));
            if remaining == Some(0) {
                reason = Refusal::Exhausted;
                continue;
            }
            return Decision::Allowed {
                issuance_id: i.id.clone(),
                remaining,
            };
        };
    }
    Decision::Refused(reason)
}

/// Canonical destination target for a lifecycle preflight or transaction.
pub(crate) fn target<'a>(verb: &str, id: &'a str) -> Target<'a> {
    if verb == "agent.start" {
        Target::Start(id)
    } else {
        Target::Agent(id)
    }
}

/// Only comparable elapsed evidence can prove that timed authority cannot recur.
pub(crate) fn expired(policy: &Lifecycle, now: Option<&ClockPair>) -> bool {
    let Some(i) = &policy.issuance else {
        return false;
    };
    let Some(now) = now else { return false };
    i.issued
        .as_ref()
        .is_some_and(|issued| issued.domain == now.domain)
        && i.expires_at_ms
            .is_some_and(|deadline| now.wall_ms >= deadline)
        && i.elapsed_deadline_ms
            .is_some_and(|deadline| now.elapsed_ms >= deadline)
}
pub(crate) fn retirement_clock() -> Option<ClockPair> {
    clock_pair()
}
