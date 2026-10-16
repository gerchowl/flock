//! The cross-harness `state` object on `agent.get` / `agent.list`
//! (ADR-0028 §1, #1019).
//!
//! `agent_status` stays for compatibility and presentation; supervisors read
//! `state`. Each axis is an independent fact with its own `source`, and
//! `phase` is derived from the axes in one fixed order, the ADR's phase
//! table, which is part of the contract and drift-locked by the tests below.
//!
//! `turn` and `lifecycle` come from today's detectors, `background` from
//! Claude's footer counter or the pane's process tree (#1022), `attention`
//! from the detector's `blocked` split by what the screen shows (#1023),
//! `waiting` from the server's wait registry (#1031).
//! `finished` is never inferred from an unknown background.

use serde::{Deserialize, Deserializer, Serialize};

/// How long `turn: idle` must hold before an agent counts as settled.
/// One server constant (ADR-0028 §1, owner decision 3).
pub const SETTLE_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Turn {
    Working,
    Idle,
    #[default]
    Unknown,
}

/// Work the agent left running outside its turn: `"none"`,
/// `{"running": {"count": N}}` or `"unknown"` on the wire (ADR-0028 §1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Background {
    None,
    Running {
        count: u32,
    },
    #[default]
    Unknown,
}

impl Background {
    /// A count of background tasks, where `None` is "not observable".
    pub fn from_count(count: Option<u32>) -> Self {
        match count {
            Some(0) => Self::None,
            Some(count) => Self::Running { count },
            None => Self::Unknown,
        }
    }
}

/// Whether the agent waits on someone (ADR-0028 §1): `"none"`,
/// `{"needs_human": {"reason": …}}`, `{"blocked": {"reason": …,
/// "retry_after_ms"?: N}}` or `"unknown"` on the wire. A reason this build
/// does not know reads as `unknown` and keeps its variant (§3a).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    None,
    /// A prompt at the pane that a human answers: today's `blocked`.
    #[cfg_attr(feature = "ts", ts(optional_fields))]
    NeedsHuman {
        #[serde(default, deserialize_with = "or_unknown")]
        reason: HumanReason,
        /// The structured question, for `reason: question` when its source
        /// could read one (ADR-0028 §8a, #1038).
        #[serde(
            default,
            deserialize_with = "or_unknown",
            skip_serializing_if = "Option::is_none"
        )]
        question: Option<Box<crate::api::question::Question>>,
    },
    /// A state no quick answer at the pane resolves.
    #[cfg_attr(feature = "ts", ts(optional_fields))]
    Blocked {
        #[serde(default, deserialize_with = "or_unknown")]
        reason: BlockedReason,
        #[serde(
            default,
            deserialize_with = "or_unknown",
            skip_serializing_if = "Option::is_none"
        )]
        /// JSON number; JavaScript integer precision requires a magnitude below 2^53.
        #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
        retry_after_ms: Option<u64>,
    },
    #[default]
    Unknown,
}

/// What a `needs_human` prompt asks for. `unknown` is a prompt whose kind
/// its source cannot name: a hook report, or a harness without matchers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum HumanReason {
    Question,
    Permission,
    Dialog,
    Trust,
    Login,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    ProviderLimit,
    #[default]
    Unknown,
}

/// A registered wait the agent holds open (ADR-0028 §5): `"none"`,
/// `{"on": {"wait_id": …, "predicate": {…}, "since": …, "deadline": …}}`
/// or `"unknown"` on the wire. `since` and `deadline` are Unix ms. With
/// several waits open, the oldest is reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Waiting {
    None,
    On {
        wait_id: String,
        /// Canonical text (`crate::api::wait_predicate`). Kept as text on
        /// read, so a newer server's predicate survives an older client.
        #[serde(default, deserialize_with = "or_unknown")]
        predicate: String,
        #[serde(default, deserialize_with = "or_unknown")]
        /// JSON number; JavaScript integer precision requires a magnitude below 2^53.
        #[cfg_attr(feature = "ts", ts(as = "f64"))]
        since: u64,
        #[serde(default, deserialize_with = "or_unknown")]
        /// JSON number; JavaScript integer precision requires a magnitude below 2^53.
        #[cfg_attr(feature = "ts", ts(as = "f64"))]
        deadline: u64,
    },
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Live,
    Hibernated,
    Offline,
    Gone,
    #[default]
    Unknown,
}

/// Ordered as the ADR's phase table, which is the order a predicate's phase
/// list is rendered in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Gone,
    Hibernated,
    Offline,
    LimitReached,
    Blocked,
    NeedsHuman,
    Working,
    WaitingOnEvent,
    FinishedWithBackground,
    Finished,
    Settling,
    #[default]
    Unknown,
}

/// Where an axis's value came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    Screen,
    Hook,
    Plugin,
    ProcessTree,
    WaitRegistry,
    #[default]
    Unknown,
}

/// Per-axis sources for the observed axes (ADR-0028 §1's source table).
/// An axis nothing reports has no entry. `lifecycle` has none: it is the
/// server's own record of the process, not an observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct StateSources {
    #[serde(
        default,
        deserialize_with = "or_unknown",
        skip_serializing_if = "Option::is_none"
    )]
    pub turn: Option<StateSource>,
    #[serde(
        default,
        deserialize_with = "or_unknown",
        skip_serializing_if = "Option::is_none"
    )]
    pub background: Option<StateSource>,
    #[serde(
        default,
        deserialize_with = "or_unknown",
        skip_serializing_if = "Option::is_none"
    )]
    pub attention: Option<StateSource>,
    #[serde(
        default,
        deserialize_with = "or_unknown",
        skip_serializing_if = "Option::is_none"
    )]
    pub waiting: Option<StateSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
///
/// Every field reads leniently: a missing, renamed or reshaped value (a
/// newer server's `background: running { … }`) becomes `unknown` on its own
/// and the other axes survive (ADR-0028 §3a).
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct AgentState {
    #[serde(default, deserialize_with = "or_unknown")]
    pub turn: Turn,
    #[serde(default, deserialize_with = "or_unknown")]
    pub background: Background,
    #[serde(default, deserialize_with = "or_unknown")]
    pub attention: Attention,
    #[serde(default, deserialize_with = "or_unknown")]
    pub waiting: Waiting,
    #[serde(default, deserialize_with = "or_unknown")]
    pub lifecycle: Lifecycle,
    #[serde(default, deserialize_with = "or_unknown")]
    pub phase: Phase,
    /// How long `phase` has held. Absent when the agent never reported a
    /// state transition, and for `hibernated` / `offline`: their entry is not
    /// timestamped yet, and the screen's last change predates it.
    #[serde(
        default,
        deserialize_with = "or_unknown",
        skip_serializing_if = "Option::is_none"
    )]
    /// JSON number; JavaScript integer precision requires a magnitude below 2^53.
    #[cfg_attr(feature = "ts", ts(as = "Option<f64>"))]
    pub held_ms: Option<u64>,
    #[serde(default, deserialize_with = "or_unknown")]
    pub source: StateSources,
}

impl AgentState {
    /// What a row whose server reported no state reads as: every axis
    /// `unknown` (ADR-0028 §3a). Remote mesh rows use it too, since the mesh
    /// summary carries no state.
    pub fn unknown() -> Self {
        Self {
            turn: Turn::Unknown,
            background: Background::Unknown,
            attention: Attention::Unknown,
            waiting: Waiting::Unknown,
            lifecycle: Lifecycle::Unknown,
            phase: Phase::Unknown,
            held_ms: None,
            source: StateSources::default(),
        }
    }

    /// Derive the state from the axes. `turn_held_ms` is how long `turn` has
    /// held, and is dropped off `live` (see `held_ms`).
    pub fn derive(
        turn: Turn,
        background: Background,
        attention: Attention,
        lifecycle: Lifecycle,
        waiting: Waiting,
        turn_held_ms: Option<u64>,
        source: StateSources,
    ) -> Self {
        let wait_open = matches!(waiting, Waiting::On { .. });
        let settled = turn == Turn::Idle && turn_held_ms.is_some_and(|ms| ms >= SETTLE_MS);
        let phase = match (lifecycle, &attention) {
            (Lifecycle::Gone, _) => Phase::Gone,
            (Lifecycle::Hibernated, _) => Phase::Hibernated,
            (Lifecycle::Offline, _) => Phase::Offline,
            (
                _,
                Attention::Blocked {
                    reason: BlockedReason::ProviderLimit,
                    ..
                },
            ) => Phase::LimitReached,
            (_, Attention::Blocked { .. }) => Phase::Blocked,
            (_, Attention::NeedsHuman { .. }) => Phase::NeedsHuman,
            _ if turn == Turn::Working => Phase::Working,
            _ if settled && wait_open => Phase::WaitingOnEvent,
            _ if settled && matches!(background, Background::Running { .. }) => {
                Phase::FinishedWithBackground
            }
            _ if settled && background == Background::None => Phase::Finished,
            _ if turn == Turn::Idle && !settled && turn_held_ms.is_some() => Phase::Settling,
            _ => Phase::Unknown,
        };
        // The settle boundary is itself a phase change: a settled phase has
        // held only since the window closed.
        let held_ms = turn_held_ms
            .filter(|_| matches!(lifecycle, Lifecycle::Live))
            .map(|ms| if settled { ms - SETTLE_MS } else { ms });
        Self {
            turn,
            background,
            attention,
            waiting,
            lifecycle,
            phase,
            held_ms,
            source,
        }
    }
}

/// Deserialize `state` so that a value that is not an object at all reads as
/// absent, and the client falls back to `agent_status` (ADR-0028 §3a).
pub fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<AgentState>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// One field read on its own: anything it cannot parse is its `unknown`.
pub(crate) fn or_unknown<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Attention as A;
    use Background as B;
    use Lifecycle as L;

    const RUN: B = B::Running { count: 1 };
    const LIMIT: A = A::Blocked {
        reason: BlockedReason::ProviderLimit,
        retry_after_ms: None,
    };
    const BLOCKED: A = A::Blocked {
        reason: BlockedReason::Unknown,
        retry_after_ms: None,
    };
    const HUMAN: A = A::NeedsHuman {
        reason: HumanReason::Permission,
        question: None,
    };

    fn on() -> Waiting {
        Waiting::On {
            wait_id: "w_1".into(),
            predicate: "events after 3".into(),
            since: 1,
            deadline: 2,
        }
    }

    /// Drift lock on the ADR-0028 §1 phase table, one row per phase in the
    /// ADR's order. Each row's inputs also satisfy the rows below it, so a
    /// derivation that tested them in another order fails here. Changing
    /// this table is a contract change: update the ADR and its supervisors.
    #[rustfmt::skip]
    const TABLE: [(&str, L, A, Turn, B, bool, u64); 14] = [
        ("gone", L::Gone, LIMIT, Turn::Working, RUN, true, SETTLE_MS),
        ("hibernated", L::Hibernated, LIMIT, Turn::Working, RUN, true, SETTLE_MS),
        ("offline", L::Offline, LIMIT, Turn::Working, RUN, true, SETTLE_MS),
        ("limit_reached", L::Live, LIMIT, Turn::Working, RUN, true, SETTLE_MS),
        ("blocked", L::Live, BLOCKED, Turn::Working, RUN, true, SETTLE_MS),
        ("needs_human", L::Live, HUMAN, Turn::Working, RUN, true, SETTLE_MS),
        ("working", L::Live, A::None, Turn::Working, RUN, true, SETTLE_MS),
        ("waiting_on_event", L::Live, A::None, Turn::Idle, RUN, true, SETTLE_MS),
        ("finished_with_background", L::Live, A::None, Turn::Idle, RUN, false, SETTLE_MS),
        ("finished", L::Live, A::None, Turn::Idle, B::None, false, SETTLE_MS),
        ("settling", L::Live, A::None, Turn::Idle, RUN, true, SETTLE_MS - 1),
        ("settling", L::Live, A::None, Turn::Idle, B::None, true, SETTLE_MS - 1),
        ("unknown", L::Live, A::None, Turn::Unknown, B::None, false, SETTLE_MS),
        // `finished` is never inferred from an unknown background.
        ("unknown", L::Live, A::Unknown, Turn::Idle, B::Unknown, false, SETTLE_MS),
    ];

    #[test]
    fn phase_derivation_matches_the_adr_table() {
        for (want, life, att, turn, bg, wait, held) in TABLE {
            let waiting = if wait { on() } else { Waiting::None };
            let state =
                AgentState::derive(turn, bg, att, life, waiting, Some(held), Default::default());
            assert_eq!(serde_json::to_value(state.phase).unwrap(), want, "{want}");
        }
        let settled = AgentState::derive(
            Turn::Idle,
            B::Unknown,
            A::Unknown,
            L::Live,
            Waiting::None,
            Some(SETTLE_MS + 1_500),
            Default::default(),
        );
        assert_eq!(
            settled.held_ms,
            Some(1_500),
            "a settled phase holds from the window's close"
        );
    }

    #[test]
    fn waiting_wire_shape_and_a_reshaped_future_read_leniently() {
        assert_eq!(serde_json::to_value(Waiting::None).unwrap(), "none");
        assert_eq!(
            serde_json::to_value(on()).unwrap(),
            serde_json::json!({"on": {"wait_id": "w_1", "predicate": "events after 3",
                "since": 1, "deadline": 2}})
        );
        let state: AgentState = serde_json::from_str(
            r#"{"waiting":{"on":{"wait_id":"w_9","predicate":{"pr":7},"since":"x"}},
                "source":{"waiting":"wait_registry"}}"#,
        )
        .unwrap();
        let Waiting::On {
            wait_id,
            predicate,
            since,
            ..
        } = state.waiting
        else {
            panic!("an open wait keeps its variant");
        };
        assert_eq!((wait_id.as_str(), since), ("w_9", 0));
        assert_eq!(predicate, "", "a reshaped predicate reads as no text");
        assert_eq!(state.source.waiting, Some(StateSource::WaitRegistry));
        let state: AgentState = serde_json::from_str(r#"{"waiting":"parked"}"#).unwrap();
        assert_eq!(state.waiting, Waiting::Unknown);
    }

    #[test]
    fn an_unparseable_axis_reads_unknown_and_the_others_survive() {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default, deserialize_with = "lenient")]
            state: Option<AgentState>,
        }
        let parse = |json: &str| serde_json::from_str::<Row>(json).unwrap().state;
        assert_eq!(parse("{}"), None);
        assert_eq!(parse(r#"{"state":"working"}"#), None);
        let future = parse(
            r#"{"state":{"turn":"idle","background":{"running":{"count":2}},"phase":"stalled",
                "lifecycle":"live","held_ms":"soon","source":{"turn":"hook","background":{"pid":1}}}}"#,
        )
        .unwrap();
        assert_eq!(
            (future.turn, future.lifecycle),
            (Turn::Idle, Lifecycle::Live)
        );
        assert_eq!(
            (future.background, future.attention),
            (B::Running { count: 2 }, A::Unknown)
        );
        assert_eq!((future.phase, future.held_ms), (Phase::Unknown, None));
        assert_eq!(future.source.turn, Some(StateSource::Hook));
        assert_eq!(
            future.source.background, None,
            "an unreadable source is no source"
        );
    }

    #[test]
    fn background_wire_shape_and_its_reshaped_future_read_leniently() {
        let wire = |bg: B| serde_json::to_value(bg).unwrap();
        assert_eq!(wire(B::from_count(Some(0))), "none");
        assert_eq!(wire(B::from_count(None)), "unknown");
        assert_eq!(
            wire(B::from_count(Some(2))),
            serde_json::json!({"running": {"count": 2}})
        );
        let state: AgentState = serde_json::from_str(
            r#"{"turn":"idle","background":{"running":{"count":"two"}},"source":{"background":"process_tree"}}"#,
        )
        .unwrap();
        assert_eq!(state.background, B::Unknown);
        assert_eq!(state.source.background, Some(StateSource::ProcessTree));
    }

    #[test]
    fn attention_wire_shape_and_an_unknown_reason_keep_their_variant() {
        let wire = |a: A| serde_json::to_value(a).unwrap();
        assert_eq!(wire(A::None), "none");
        assert_eq!(
            wire(HUMAN),
            serde_json::json!({"needs_human": {"reason": "permission"}})
        );
        assert_eq!(
            wire(A::Blocked {
                reason: BlockedReason::ProviderLimit,
                retry_after_ms: Some(1_500)
            }),
            serde_json::json!({"blocked": {"reason": "provider_limit", "retry_after_ms": 1_500}})
        );
        let state: AgentState = serde_json::from_str(
            r#"{"attention":{"needs_human":{"reason":"captcha"}},"source":{"attention":"screen"}}"#,
        )
        .unwrap();
        assert_eq!(
            state.attention,
            A::NeedsHuman {
                reason: HumanReason::Unknown,
                question: None,
            }
        );
        let state: AgentState = serde_json::from_str(
            r#"{"attention":{"blocked":{"reason":"provider_limit","retry_after_ms":"soon"}}}"#,
        )
        .unwrap();
        assert_eq!(state.attention, LIMIT);
        let state: AgentState = serde_json::from_str(r#"{"attention":"stalled"}"#).unwrap();
        assert_eq!(state.attention, A::Unknown);
    }
}
