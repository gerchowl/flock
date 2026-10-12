//! The cross-harness `state` object on `agent.get` / `agent.list`
//! (ADR-0028 §1, #1019).
//!
//! `agent_status` stays for compatibility and presentation; supervisors read
//! `state`. Each axis is an independent fact with its own `source`, and
//! `phase` is derived from the axes in one fixed order, the ADR's phase
//! table, which is part of the contract and drift-locked by the tests below.
//!
//! `turn` and `lifecycle` come from today's detectors, `background` from
//! Claude's footer counter or the pane's process tree (#1022). `attention`
//! stays `unknown` until E1c (#1023). `finished` is never inferred from an
//! unknown background.

use serde::{Deserialize, Deserializer, Serialize};

/// How long `turn: idle` must hold before an agent counts as settled.
/// One server constant (ADR-0028 §1, owner decision 3).
pub const SETTLE_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// This slice emits only `unknown` here. The other values exist so the phase
/// table can be derived and locked whole; E1c gives them their reasons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    None,
    NeedsHuman,
    ProviderLimit,
    Blocked,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Live,
    Hibernated,
    Offline,
    Gone,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    Screen,
    Hook,
    Plugin,
    ProcessTree,
    #[default]
    Unknown,
}

/// Per-axis sources for the observed axes (ADR-0028 §1's source table).
/// An axis nothing reports has no entry. `lifecycle` has none: it is the
/// server's own record of the process, not an observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
///
/// Every field reads leniently: a missing, renamed or reshaped value (a
/// newer server's `background: running { … }`) becomes `unknown` on its own
/// and the other axes survive (ADR-0028 §3a).
pub struct AgentState {
    #[serde(default, deserialize_with = "or_unknown")]
    pub turn: Turn,
    #[serde(default, deserialize_with = "or_unknown")]
    pub background: Background,
    #[serde(default, deserialize_with = "or_unknown")]
    pub attention: Attention,
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
        wait_open: bool,
        turn_held_ms: Option<u64>,
        source: StateSources,
    ) -> Self {
        let settled = turn == Turn::Idle && turn_held_ms.is_some_and(|ms| ms >= SETTLE_MS);
        let phase = match (lifecycle, attention) {
            (Lifecycle::Gone, _) => Phase::Gone,
            (Lifecycle::Hibernated, _) => Phase::Hibernated,
            (Lifecycle::Offline, _) => Phase::Offline,
            (_, Attention::ProviderLimit) => Phase::LimitReached,
            (_, Attention::Blocked) => Phase::Blocked,
            (_, Attention::NeedsHuman) => Phase::NeedsHuman,
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
fn or_unknown<'de, D, T>(deserializer: D) -> Result<T, D::Error>
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

    /// Drift lock on the ADR-0028 §1 phase table, one row per phase in the
    /// ADR's order. Each row's inputs also satisfy the rows below it, so a
    /// derivation that tested them in another order fails here. Changing
    /// this table is a contract change: update the ADR and its supervisors.
    #[rustfmt::skip]
    const TABLE: [(&str, L, A, Turn, B, bool, u64); 14] = [
        ("gone", L::Gone, A::ProviderLimit, Turn::Working, RUN, true, SETTLE_MS),
        ("hibernated", L::Hibernated, A::ProviderLimit, Turn::Working, RUN, true, SETTLE_MS),
        ("offline", L::Offline, A::ProviderLimit, Turn::Working, RUN, true, SETTLE_MS),
        ("limit_reached", L::Live, A::ProviderLimit, Turn::Working, RUN, true, SETTLE_MS),
        ("blocked", L::Live, A::Blocked, Turn::Working, RUN, true, SETTLE_MS),
        ("needs_human", L::Live, A::NeedsHuman, Turn::Working, RUN, true, SETTLE_MS),
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
            let state =
                AgentState::derive(turn, bg, att, life, wait, Some(held), Default::default());
            assert_eq!(serde_json::to_value(state.phase).unwrap(), want, "{want}");
        }
        let settled = AgentState::derive(
            Turn::Idle,
            B::Unknown,
            A::Unknown,
            L::Live,
            false,
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
}
