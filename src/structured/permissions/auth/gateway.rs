//! Dormant ADR-0043 A1 store. M2 must supply authenticated home/token facts and
//! destination-owned membership, then perform the claim inside `admit`'s lease.
use super::tokens::Scope;
use crate::{
    api::{
        error_code::ErrorCode as E,
        gateway_grants::{ClockPair, Grant, Issuance, Lifecycle, Listing, Subject, SubjectKind},
    },
    app::LocalOperator,
    platform::{client_tokens::Namespace, file_lock::LockFile},
    terminal::AgentId,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::TryLockError,
    io::{Read, Write},
    path::Path,
};

pub(crate) struct Store(Namespace);
pub(crate) enum Change {
    Add(Grant),
    Revoke(String),
}
/// Membership is resolved from local state even for a newly requested workspace.
#[derive(Clone, Copy)]
pub(crate) enum Target<'a> {
    Agent(&'a str),
    Start(&'a str),
}
/// Only authenticated origin/current home-token facts may populate this context.
pub(crate) struct Request<'a> {
    pub home: &'a str,
    pub principal: &'a str,
    pub scopes: &'a BTreeSet<Scope>,
    pub verb: &'a str,
    pub target: Target<'a>,
}
fn storage(_: impl std::fmt::Debug) -> E {
    E::InternalError
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}
fn selector(s: &str) -> bool {
    s == "*"
        || s.strip_prefix("agent:")
            .is_some_and(AgentId::is_well_formed)
        || s.strip_prefix("workspace:")
            .is_some_and(|id| crate::workspace::public_workspace_number(id).is_some())
}
fn valid(g: &Grant) -> bool {
    name(&g.name)
        && g.home_node_id.len() == 64
        && g.home_node_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && g.principals
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 256 && !p.chars().any(char::is_control))
        && g.verbs.iter().all(|v| required_scope(v).is_some())
        && g.targets.iter().all(|s| selector(s))
        && g.lifecycle.as_ref().is_none_or(|p| {
            p.destination_node_id.len() == 64
                && p.destination_node_id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && p.max_uses != Some(0)
                && p.lifetime_ms != Some(0)
                && (!matches!(
                    p.subject,
                    Subject::Role {
                        role: crate::api::gateway_grants::Role::OwnerOfAgent,
                        ..
                    }
                ) || g.targets.iter().all(|s| s.starts_with("agent:")))
        })
}
fn sealed(p: &Lifecycle) -> bool {
    let Some(i) = &p.issuance else {
        return false;
    };
    if i.id.len() != 32
        || !i.id.bytes().all(|b| b.is_ascii_hexdigit())
        || i.issuer != format!("{}:local_operator", p.destination_node_id)
    {
        return false;
    }
    match (
        p.lifetime_ms,
        &i.issued,
        i.expires_at_ms,
        i.elapsed_deadline_ms,
    ) {
        (None, None, None, None) => true,
        (Some(l), Some(c), Some(w), Some(m)) => {
            !c.domain.is_empty()
                && c.wall_ms.checked_add(l) == Some(w)
                && c.elapsed_ms.checked_add(l) == Some(m)
        }
        _ => false,
    }
}
/// Closed mapping: unknown verbs cannot acquire authority through a grant.
pub(crate) fn required_scope(verb: &str) -> Option<Scope> {
    Some(match verb {
        "agent.read" | "agent.history" | "agent.status" | "agent.list" | "agent.result" => {
            Scope::Read
        }
        "agent.send" | "queue.add" | "queue.update" | "queue.gate" => Scope::Send,
        "question.answer" | "question.resolve" => Scope::Answer,
        "agent.decide" | "agent.interrupt" | "agent.fork" | "agent.reload" | "agent.start"
        | "agent.stop" | "agent.restart" | "agent.resume" => Scope::Operate,
        _ => return None,
    })
}
fn matches_target(g: &Grant, target: Target<'_>, workspace: &str) -> bool {
    g.targets.iter().any(|s| {
        s == "*"
            || s.strip_prefix("workspace:") == Some(workspace)
            || matches!(target, Target::Agent(id) if s.strip_prefix("agent:") == Some(id))
    })
}
fn lifecycle(verb: &str) -> bool {
    matches!(
        verb,
        "agent.start" | "agent.stop" | "agent.restart" | "agent.resume"
    )
}
impl Store {
    pub(crate) fn open(data: &Path) -> Result<Self, E> {
        Namespace::open_named(data, "gateway-grants")
            .map(Self)
            .map_err(storage)
    }
    /// Capture the fence before asynchronous preflight; `admit` checks it again.
    pub(crate) fn generation(&self) -> Result<u64, E> {
        self.snapshot()
            .map(|(_, value)| value["generation"].as_u64().unwrap_or(0))
    }
    fn snapshot(&self) -> Result<(LockFile, Value), E> {
        let file = match self.0.file(".lock", true) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.0.file(".lock", false).map_err(storage)?
            }
            Err(e) => return Err(storage(e)),
        };
        let lease = LockFile::new(file);
        lease.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => E::SessionBusy,
            _ => E::InternalError,
        })?;
        let mut bytes = Vec::new();
        let value = match self.0.file("grants.json", false) {
            Ok(file) => {
                file.take(1_048_577)
                    .read_to_end(&mut bytes)
                    .map_err(storage)?;
                if bytes.len() > 1_048_576 {
                    return Err(E::InternalError);
                }
                serde_json::from_slice(&bytes).map_err(storage)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                json!({"generation":0,"entries":{}})
            }
            Err(e) => return Err(storage(e)),
        };
        if value["generation"].as_u64().is_none() || !value["entries"].is_object() {
            return Err(E::InternalError);
        }
        Ok((lease, value))
    }
    fn listing(value: &Value) -> Listing {
        let entries = value["entries"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, raw)| {
                let grant = serde_json::from_value::<Grant>(raw.clone())
                    .ok()
                    .filter(|g| {
                        g.name == *key && valid(g) && g.lifecycle.as_ref().is_none_or(|p| sealed(p))
                    });
                if grant.is_none() {
                    tracing::warn!("gateway grant entry skipped: invalid or newer format");
                }
                (key.clone(), grant)
            })
            .collect();
        Listing {
            generation: value["generation"].as_u64().unwrap_or(0),
            entries,
        }
    }
    /// LocalOperator is sealed by the same socket admission as client tokens.
    pub(crate) fn manage(
        &self,
        authority: &LocalOperator<'_>,
        change: Option<Change>,
    ) -> Result<Listing, E> {
        self.manage_issued(authority, change, || {
            let destination = crate::mesh::identity::NodeIdentity::existing()
                .map_err(storage)?
                .node_id();
            Ok((destination, lifecycle::clock_pair()))
        })
    }
    fn manage_issued(
        &self,
        _: &LocalOperator<'_>,
        change: Option<Change>,
        environment: impl FnOnce() -> Result<(String, Option<ClockPair>), E>,
    ) -> Result<Listing, E> {
        let (_lease, mut value) = self.snapshot()?;
        if let Some(change) = change {
            match change {
                Change::Add(mut grant) => {
                    if !valid(&grant) || value["entries"].get(&grant.name).is_some() {
                        return Err(E::InvalidParams);
                    }
                    if let Some(p) = &mut grant.lifecycle {
                        if p.issuance.is_some() {
                            return Err(E::InvalidParams);
                        }
                        let (destination, clock) = environment()?;
                        if p.destination_node_id != destination {
                            return Err(E::InvalidParams);
                        }
                        let mut bytes = [0; 16];
                        getrandom::fill(&mut bytes).map_err(storage)?;
                        let mut issuance = Issuance {
                            id: format!("{:032x}", u128::from_ne_bytes(bytes)),
                            issuer: format!("{destination}:local_operator"),
                            issued: None,
                            expires_at_ms: None,
                            elapsed_deadline_ms: None,
                        };
                        if let Some(lifetime) = p.lifetime_ms {
                            let clock = clock.ok_or(E::InternalError)?;
                            issuance.expires_at_ms = Some(
                                clock
                                    .wall_ms
                                    .checked_add(lifetime)
                                    .ok_or(E::InvalidParams)?,
                            );
                            issuance.elapsed_deadline_ms = Some(
                                clock
                                    .elapsed_ms
                                    .checked_add(lifetime)
                                    .ok_or(E::InvalidParams)?,
                            );
                            clocks::initialize(&mut value, &issuance.id, &clock)?;
                            issuance.issued = Some(clock);
                        }
                        if p.max_uses.is_some() {
                            accounting::initialize(&mut value, &issuance.id)?;
                        }
                        p.issuance = Some(issuance);
                    }
                    value["entries"][&grant.name] =
                        serde_json::to_value(&grant).map_err(storage)?;
                }
                Change::Revoke(key) => {
                    let id = value["entries"][&key]["lifecycle"]["issuance"]["id"]
                        .as_str()
                        .map(str::to_owned);
                    if let Some(id) = &id {
                        clocks::remove(&mut value, id);
                    }
                    if let (Some(id), Some(evidence)) = (
                        id,
                        value.get_mut("accounting").and_then(Value::as_object_mut),
                    ) {
                        evidence.remove(&id);
                    }
                    value["entries"]
                        .as_object_mut()
                        .ok_or(E::InternalError)?
                        .remove(&key)
                        .ok_or(E::InvalidParams)?;
                }
            }
            value["generation"] = value["generation"]
                .as_u64()
                .and_then(|n| n.checked_add(1))
                .ok_or(E::InternalError)?
                .into();
            self.persist(&value)?;
        }
        Ok(Self::listing(&value))
    }
    fn persist(&self, value: &Value) -> Result<(), E> {
        let bytes = serde_json::to_vec(value).map_err(storage)?;
        if bytes.len() > 1_048_576 {
            return Err(E::InvalidParams);
        }
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(storage)?;
        let temporary = format!(".tmp-{:032x}", u128::from_ne_bytes(nonce));
        let result = (|| {
            let mut file = self.0.file(&temporary, true).map_err(storage)?;
            file.write_all(&bytes).map_err(storage)?;
            file.sync_all().map_err(storage)?;
            self.0
                .publish(&temporary, "grants.json", true)
                .map_err(storage)
        })();
        let _ = self.0.remove(&temporary);
        result
    }
    /// Evaluate again under the durable generation fence; claim cannot race revoke.
    /// `membership` MUST read destination state, never a workspace asserted for an agent.
    /// M2 must also recheck enrollment and apply each verb's existing local checks.
    pub(crate) fn admit<R>(
        &self,
        request: &Request<'_>,
        generation: u64,
        membership: impl FnOnce(Target<'_>) -> Option<String>,
        claim: impl FnOnce() -> Result<R, E>,
    ) -> Result<R, E> {
        // Remote config is unsupported before any storage access or claim.
        if request.verb.starts_with("config.") {
            return Err(E::Unsupported);
        }
        if (request.verb == "agent.start") != matches!(request.target, Target::Start(_)) {
            return Err(E::InvalidParams);
        }
        let scope = required_scope(request.verb).ok_or(E::Unsupported)?;
        if !request.scopes.contains(&scope) {
            return Err(E::Scope);
        }
        let (_lease, value) = self.snapshot()?;
        let listing = Self::listing(&value);
        if generation != listing.generation {
            return Err(E::AuthRevoked);
        }
        let workspace = membership(request.target).ok_or(E::NotAuthorized)?;
        let allowed = listing.entries.values().flatten().any(|g| {
            g.lifecycle.is_none()
                && g.home_node_id == request.home
                && g.principals.contains(request.principal)
                && g.scopes.contains(&scope)
                && g.verbs.contains(request.verb)
                && matches_target(g, request.target, &workspace)
        });
        if !allowed {
            return Err(E::NotAuthorized);
        }
        // ADR-0036 §1a / Q9: flip this single gate only after an owner ruling.
        if lifecycle(request.verb) {
            return Err(E::UnsupportedForAgent);
        }
        claim()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;

pub(crate) mod accounting;
mod clocks;
pub(crate) mod lifecycle;
#[cfg(test)]
use lifecycle::{Decision, Evaluation, PrincipalKind, Refusal};

#[cfg(test)]
pub(crate) fn claim_fixture() -> (
    crate::structured::launch::SessionConfigDir,
    Store,
    u64,
    String,
) {
    claim_fixture_limit(Some(1))
}

#[cfg(test)]
pub(crate) fn attested_fixture(
    agent: bool,
    max_uses: Option<u64>,
    timed: bool,
) -> (crate::structured::launch::SessionConfigDir, Store, u64) {
    let (root, store, mut app) = tests::fixture();
    let mut grant = tests::grant(&["*"]);
    grant.home_node_id = crate::mesh::identity::NodeIdentity::fixture([7; 32]).node_id();
    grant.principals = [if agent {
        "agent_atlas_subject"
    } else {
        "client-1"
    }
    .into()]
    .into();
    grant.lifecycle = Some(Box::new(Lifecycle {
        subject: if agent { Subject::Agent } else { Subject::User },
        destination_node_id: "b".repeat(64),
        max_uses,
        lifetime_ms: timed.then_some(10000),
        issuance: None,
    }));
    let listing = store
        .manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(grant)),
            || {
                Ok((
                    "b".repeat(64),
                    timed.then_some(ClockPair {
                        domain: "fixture-boot".into(),
                        wall_ms: 10000,
                        elapsed_ms: 20000,
                    }),
                ))
            },
        )
        .unwrap();
    (root, store, listing.generation)
}

#[cfg(test)]
pub(crate) fn claim_fixture_limit(
    max_uses: Option<u64>,
) -> (
    crate::structured::launch::SessionConfigDir,
    Store,
    u64,
    String,
) {
    let (root, store, mut app) = tests::fixture();
    let mut grant = tests::grant(&["*"]);
    grant.lifecycle = Some(
        Lifecycle {
            subject: Subject::User,
            destination_node_id: "b".repeat(64),
            max_uses,
            lifetime_ms: None,
            issuance: None,
        }
        .into(),
    );
    let generation = tests::add(&store, &mut app, grant);
    let listing = store
        .manage(&app.client_token_authority().unwrap(), None)
        .unwrap();
    let id = listing.entries["tablet"]
        .as_ref()
        .unwrap()
        .lifecycle
        .as_ref()
        .unwrap()
        .issuance
        .as_ref()
        .unwrap()
        .id
        .clone();
    (root, store, generation, id)
}

#[cfg(test)]
pub(crate) fn claim_revoke(store: &Store) -> Result<Listing, E> {
    store.manage(
        &tests::operator().client_token_authority().unwrap(),
        Some(Change::Revoke("tablet".into())),
    )
}

#[cfg(test)]
pub(crate) fn claim_install(store: &Store, name: &str, max_uses: Option<u64>) -> (u64, String) {
    let mut grant = tests::grant(&["*"]);
    grant.name = name.into();
    grant.lifecycle = Some(Box::new(Lifecycle {
        subject: Subject::User,
        destination_node_id: "b".repeat(64),
        max_uses,
        lifetime_ms: None,
        issuance: None,
    }));
    let listing = store
        .manage_issued(
            &tests::operator().client_token_authority().unwrap(),
            Some(Change::Add(grant)),
            || Ok(("b".repeat(64), None)),
        )
        .unwrap();
    let id = listing.entries[name]
        .as_ref()
        .unwrap()
        .lifecycle
        .as_ref()
        .unwrap()
        .issuance
        .as_ref()
        .unwrap()
        .id
        .clone();
    (listing.generation, id)
}

#[cfg(test)]
pub(crate) fn claim_timed_fixture(
    wall: u64,
    elapsed: u64,
) -> (
    crate::structured::launch::SessionConfigDir,
    Store,
    u64,
    String,
) {
    claim_timed_fixture_limit(wall, elapsed, Some(10))
}

#[cfg(test)]
pub(crate) fn claim_timed_fixture_limit(
    wall: u64,
    elapsed: u64,
    max_uses: Option<u64>,
) -> (
    crate::structured::launch::SessionConfigDir,
    Store,
    u64,
    String,
) {
    let (root, store, mut app) = tests::fixture();
    let mut grant = tests::grant(&["*"]);
    grant.lifecycle = Some(Box::new(Lifecycle {
        subject: Subject::User,
        destination_node_id: "b".repeat(64),
        max_uses,
        lifetime_ms: Some(10000),
        issuance: None,
    }));
    let listing = store
        .manage_issued(
            &app.client_token_authority().unwrap(),
            Some(Change::Add(grant)),
            || {
                Ok((
                    "b".repeat(64),
                    Some(ClockPair {
                        domain: "fixture-boot".into(),
                        wall_ms: wall,
                        elapsed_ms: elapsed,
                    }),
                ))
            },
        )
        .unwrap();
    let id = listing.entries["tablet"]
        .as_ref()
        .unwrap()
        .lifecycle
        .as_ref()
        .unwrap()
        .issuance
        .as_ref()
        .unwrap()
        .id
        .clone();
    (root, store, listing.generation, id)
}

#[cfg(test)]
pub(crate) fn claim_acknowledge(store: &Store, clock: ClockPair) {
    store
        .acknowledge_clock_at(
            &tests::operator().client_token_authority().unwrap(),
            Some(clock),
        )
        .unwrap();
}

#[cfg(test)]
pub(crate) fn attest_sibling(store: &Store, name: &str, home: &str) {
    let (_lease, mut value) = store.snapshot().unwrap();
    value["entries"][name]["home_node_id"] = json!(home);
    // Keep the timed issuance first so the untimed fallback is exercised every run.
    value["entries"][name]["lifecycle"]["issuance"]["id"] = json!("f".repeat(32));
    store.persist(&value).unwrap();
}

#[cfg(test)]
pub(crate) fn reserve_ahead_fixture(store: &Store, id: &str) {
    store
        .with_accounting(|_, fence| {
            fence.prepare(
                id,
                accounting::Stamp::default(),
                accounting::Stamp {
                    total: 1,
                    digest: [1; 32],
                },
            )
        })
        .unwrap();
}
