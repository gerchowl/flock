//! Durable destination-wide clock refusal, held under A1's generation lease.
//! Raw per-issuance evidence survives newer formats and malformed entries.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    latched: bool,
    last: Option<ClockPair>,
    entries: BTreeMap<String, Value>,
    #[serde(default)]
    unrecoverable: BTreeSet<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    anchor: ClockPair,
    dead: bool,
}
fn read(value: &Value) -> Result<State, E> {
    match value.get("clocks") {
        None => Ok(State::default()),
        Some(raw) => serde_json::from_value(raw.clone()).map_err(|_| {
            tracing::warn!("grant clock evidence preserved and fenced: invalid or newer format");
            E::SessionUnconfirmed
        }),
    }
}
fn comparable(previous: &ClockPair, now: &ClockPair) -> bool {
    previous.domain == now.domain
        && now
            .wall_ms
            .checked_sub(previous.wall_ms)
            .zip(now.elapsed_ms.checked_sub(previous.elapsed_ms))
            .is_some_and(|(w, m)| w.abs_diff(m) <= 1000)
}
fn save(value: &mut Value, state: State) -> Result<(), E> {
    value["clocks"] = serde_json::to_value(state).map_err(storage)?;
    Ok(())
}
pub(super) fn initialize(value: &mut Value, id: &str, pair: &ClockPair) -> Result<(), E> {
    let mut state = read(value)?;
    state.entries.insert(
        id.into(),
        serde_json::to_value(Evidence {
            anchor: pair.clone(),
            dead: false,
        })
        .map_err(storage)?,
    );
    if state.last.is_none() {
        state.last = Some(pair.clone());
    }
    save(value, state)
}
pub(super) fn remove(value: &mut Value, id: &str) {
    if let Ok(mut state) = read(value) {
        state.unrecoverable.remove(id);
        let _ = save(value, state);
    }
    if let Some(entries) = value
        .get_mut("clocks")
        .and_then(|v| v.get_mut("entries"))
        .and_then(Value::as_object_mut)
    {
        entries.remove(id);
    }
}
impl State {
    fn observe(&mut self, listing: &Listing, now: Option<&ClockPair>, acknowledge: bool) {
        if !acknowledge
            && !self
                .last
                .as_ref()
                .zip(now)
                .is_some_and(|(p, n)| comparable(p, n))
        {
            self.latched = true;
        }
        for p in listing
            .entries
            .values()
            .flatten()
            .filter_map(|g| g.lifecycle.as_ref())
            .filter(|p| p.lifetime_ms.is_some())
        {
            let Some(i) = &p.issuance else { continue };
            if self.unrecoverable.contains(&i.id) {
                continue;
            }
            let Some(raw) = self.entries.get_mut(&i.id) else {
                if acknowledge {
                    self.unrecoverable.insert(i.id.clone());
                }
                self.latched = true;
                continue;
            };
            let Ok(mut evidence) = serde_json::from_value::<Evidence>(raw.clone()) else {
                tracing::warn!("grant clock entry preserved and fenced: invalid or newer format");
                if acknowledge {
                    self.unrecoverable.insert(i.id.clone());
                }
                self.latched = true;
                continue;
            };
            // Losing the original elapsed domain is irreversible, including after acknowledgement.
            let same_boot = i
                .issued
                .as_ref()
                .zip(now)
                .is_some_and(|(p, n)| p.domain == n.domain && n.elapsed_ms >= p.elapsed_ms);
            evidence.dead |= !same_boot || !lifecycle::unexpired(p, now);
            if !acknowledge && !now.is_some_and(|n| comparable(&evidence.anchor, n)) {
                self.latched = true;
            }
            if acknowledge {
                if let Some(now) = now {
                    evidence.anchor = now.clone();
                }
            }
            if let Ok(updated) = serde_json::to_value(evidence) {
                *raw = updated;
            }
        }
        if acknowledge {
            self.latched = false;
        }
        self.last = now.cloned();
    }
    fn allows(&self, id: &str) -> bool {
        !self.latched
            && !self.unrecoverable.contains(id)
            && self
                .entries
                .get(id)
                .and_then(|raw| serde_json::from_value::<Evidence>(raw.clone()).ok())
                .is_some_and(|e| !e.dead)
    }
}
impl accounting::Fence<'_> {
    /// Persist before J1 can claim. A crash may refuse early, never extend authority.
    pub(crate) fn clock_check(
        &mut self,
        listing: &Listing,
        id: &str,
        now: Option<&ClockPair>,
    ) -> Result<bool, E> {
        let mut state = match read(&self.value) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        state.observe(listing, now, false);
        let allowed = state.allows(id);
        save(&mut self.value, state)?;
        self.store.persist(&self.value)?;
        Ok(allowed)
    }
}
impl Store {
    /// Explicit dormant management operation; only a sealed local operator can call it.
    /// The operator must validate the destination clock before acknowledging.
    pub(crate) fn acknowledge_clock(&self, authority: &LocalOperator<'_>) -> Result<(), E> {
        self.acknowledge_clock_at(authority, lifecycle::clock_pair())
    }
    pub(super) fn acknowledge_clock_at(
        &self,
        _: &LocalOperator<'_>,
        now: Option<ClockPair>,
    ) -> Result<(), E> {
        let now = now.ok_or(E::SessionUnconfirmed)?;
        let (_lease, mut value) = self.snapshot()?;
        let mut state = read(&value)?;
        state.observe(&Self::listing(&value), Some(&now), true);
        save(&mut value, state)?;
        self.persist(&value)
    }
}

/// A detected preflight refusal is durable too; allowed preflight spends nothing.
pub(super) fn preflight(
    store: &Store,
    value: &mut Value,
    listing: &Listing,
    id: &str,
    now: Option<&ClockPair>,
) -> Result<bool, E> {
    let mut state = match read(value) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    state.observe(listing, now, false);
    let allowed = state.allows(id);
    if !allowed {
        save(value, state)?;
        store.persist(value)?;
    }
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structured::permissions::auth::gateway::tests::{fixture, grant};

    fn pair(wall: u64, elapsed: u64) -> ClockPair {
        ClockPair {
            domain: "fixture-boot".into(),
            wall_ms: wall,
            elapsed_ms: elapsed,
        }
    }
    fn install(store: &Store, app: &mut crate::app::App, name: &str, pair: ClockPair) -> String {
        let mut g = grant(&["*"]);
        g.name = name.into();
        g.lifecycle = Some(Box::new(Lifecycle {
            subject: Subject::User,
            destination_node_id: "b".repeat(64),
            max_uses: None,
            lifetime_ms: Some(10000),
            issuance: None,
        }));
        let listing = store
            .manage_issued(
                &app.client_token_authority().unwrap(),
                Some(Change::Add(g)),
                || Ok(("b".repeat(64), Some(pair))),
            )
            .unwrap();
        listing.entries[name]
            .as_ref()
            .unwrap()
            .lifecycle
            .as_ref()
            .unwrap()
            .issuance
            .as_ref()
            .unwrap()
            .id
            .clone()
    }
    fn check(store: &Store, id: &str, pair: Option<&ClockPair>) -> bool {
        store
            .with_accounting(|listing, fence| fence.clock_check(listing, id, pair))
            .unwrap()
    }
    fn ack(store: &Store, app: &mut crate::app::App, pair: ClockPair) {
        store
            .acknowledge_clock_at(&app.client_token_authority().unwrap(), Some(pair))
            .unwrap();
    }
    #[test]
    fn destination_latch_covers_other_issuances_until_operator_acknowledges() {
        let (_root, store, mut app) = fixture();
        let one = install(&store, &mut app, "first", pair(10000, 20000));
        assert!(check(&store, &one, Some(&pair(11000, 21000))));
        assert!(!check(&store, &one, Some(&pair(10500, 21500))));
        let two = install(&store, &mut app, "second", pair(12000, 22000));
        assert!(!check(&store, &two, Some(&pair(12500, 22500))));
        ack(&store, &mut app, pair(13000, 23000));
        assert!(check(&store, &two, Some(&pair(13500, 23500))));
        assert!(check(&store, &one, Some(&pair(13500, 23500))));
    }
    #[test]
    fn lost_or_newer_entry_is_preserved_and_never_restored_by_acknowledgement() {
        for raw in [None, Some(json!({"future_clock":true}))] {
            let (_root, store, mut app) = fixture();
            let one = install(&store, &mut app, "first", pair(10000, 20000));
            {
                let (_lease, mut value) = store.snapshot().unwrap();
                if let Some(raw) = &raw {
                    value["clocks"]["entries"][&one] = raw.clone();
                } else {
                    value["clocks"]["entries"]
                        .as_object_mut()
                        .unwrap()
                        .remove(&one);
                }
                store.persist(&value).unwrap();
            }
            assert!(!check(&store, &one, Some(&pair(11000, 21000))));
            ack(&store, &mut app, pair(12000, 22000));
            assert!(!check(&store, &one, Some(&pair(12500, 22500))));
            let two = install(&store, &mut app, "second", pair(13000, 23000));
            assert!(check(&store, &two, Some(&pair(13500, 23500))));
            let (_lease, value) = store.snapshot().unwrap();
            assert_eq!(value["clocks"]["entries"].get(&one).cloned(), raw);
        }
    }
    #[test]
    fn equality_overflow_missing_and_elapsed_regression_refuse() {
        for now in [
            None,
            Some(pair(u64::MAX, 21000)),
            Some(pair(19000, 29000)),
            Some(pair(11000, 19999)),
        ] {
            let (_root, store, mut app) = fixture();
            let one = install(&store, &mut app, "first", pair(10000, 20000));
            assert!(!check(&store, &one, now.as_ref()));
        }
    }
    #[test]
    fn unavailable_acknowledgement_cannot_clear_a_latch() {
        let (_root, store, mut app) = fixture();
        let one = install(&store, &mut app, "first", pair(10000, 20000));
        assert!(!check(&store, &one, Some(&pair(13000, 21000))));
        assert_eq!(
            store.acknowledge_clock_at(&app.client_token_authority().unwrap(), None),
            Err(E::SessionUnconfirmed)
        );
        assert!(!check(&store, &one, Some(&pair(12000, 22000))));
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    #[test]
    fn preflight_discontinuity_latches_before_a_later_valid_reading() {
        let (_root, store, generation, issuance) = super::super::claim_timed_fixture(10000, 20000);
        let scopes = [Scope::Operate].into();
        let request = Request {
            home: &"a".repeat(64),
            principal: "client-1",
            scopes: &scopes,
            verb: "agent.stop",
            target: Target::Agent("agent_atlas_1"),
        };
        let consumed = BTreeMap::new();
        let decision = store
            .evaluate(
                &lifecycle::Evaluation {
                    request: &request,
                    source: lifecycle::PrincipalKind::User,
                    destination: &"b".repeat(64),
                    clock: Some(ClockPair {
                        domain: "fixture-boot".into(),
                        wall_ms: 13000,
                        elapsed_ms: 21000,
                    }),
                    consumed: &consumed,
                },
                generation,
                |_| Some("w1".into()),
            )
            .unwrap();
        assert_eq!(
            decision,
            lifecycle::Decision::Refused(lifecycle::Refusal::Expired)
        );
        assert!(!store
            .with_accounting(|listing, fence| fence.clock_check(
                listing,
                &issuance,
                Some(&ClockPair {
                    domain: "fixture-boot".into(),
                    wall_ms: 12000,
                    elapsed_ms: 22000,
                })
            ))
            .unwrap());
    }
}
