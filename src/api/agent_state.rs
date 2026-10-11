//! The cross-harness `state` object on `agent.get` / `agent.list`
//! (ADR-0028 §1, #1019).
//!
//! `agent_status` stays for compatibility and presentation; supervisors read
//! `state`. Each axis is an independent fact with its own `source`, and
//! `phase` is derived from the axes in one fixed order, the ADR's phase
//! table, which is part of the contract and drift-locked by the tests below.
//!
//! This slice fills `turn` and `lifecycle` from today's detectors only.
//! `background` and `attention` are always `unknown` until E1b (#1022) and
//! E1c (#1023), so `finished` cannot be reached yet: it is never inferred
//! from an unknown background.

use serde::{Deserialize, Deserializer, Serialize};

/// How long `turn: idle` must hold before an agent counts as settled.
/// One server constant (ADR-0028 §1, owner decision 3).
pub const SETTLE_MS: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Turn {
    Working,
    Idle,
    #[serde(other)]
    Unknown,
}

/// This slice emits only `unknown` here and for [`Attention`]. The other
/// values exist so the phase table can be derived and locked whole; E1b and
/// E1c give them their ADR payloads (`running { count, … }`, reasons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    None,
    Running,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    None,
    NeedsHuman,
    ProviderLimit,
    Blocked,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Live,
    Hibernated,
    Offline,
    Gone,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(other)]
    Unknown,
}

/// Where an axis's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    Screen,
    Hook,
    Plugin,
    Server,
    #[serde(other)]
    Unknown,
}

/// Per-axis sources. An axis nothing reports has no entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<StateSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<StateSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<StateSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<StateSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub turn: Turn,
    pub background: Background,
    pub attention: Attention,
    pub lifecycle: Lifecycle,
    pub phase: Phase,
    /// How long `phase` has held. Absent when the agent never reported a
    /// state transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_ms: Option<u64>,
    #[serde(default)]
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

    /// Derive the state from the axes. `turn_held_ms` is how long `turn`
    /// (or, off `live`, the lifecycle) has held.
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
            _ if settled && background == Background::Running => Phase::FinishedWithBackground,
            _ if settled && background == Background::None => Phase::Finished,
            _ if turn == Turn::Idle && !settled && turn_held_ms.is_some() => Phase::Settling,
            _ => Phase::Unknown,
        };
        // The settle boundary is itself a phase change: a settled phase has
        // held only since the window closed.
        let held_ms = turn_held_ms.map(|ms| if settled { ms - SETTLE_MS } else { ms });
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

/// Deserialize `state` so that a missing, malformed or future-shaped value
/// reads as absent, and the client falls back to `agent_status` rather than
/// failing the whole record (ADR-0028 §3a).
pub fn lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<AgentState>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Attention as A;
    use Background as B;
    use Lifecycle as L;

    /// Drift lock on the ADR-0028 §1 phase table, one row per phase in the
    /// ADR's order. Each row's inputs also satisfy the rows below it, so a
    /// derivation that tested them in another order fails here. Changing
    /// this table is a contract change: update the ADR and its supervisors.
    #[rustfmt::skip]
    const TABLE: [(&str, L, A, Turn, B, bool, u64); 13] = [
        ("gone", L::Gone, A::ProviderLimit, Turn::Working, B::Running, true, SETTLE_MS),
        ("hibernated", L::Hibernated, A::ProviderLimit, Turn::Working, B::Running, true, SETTLE_MS),
        ("offline", L::Offline, A::ProviderLimit, Turn::Working, B::Running, true, SETTLE_MS),
        ("limit_reached", L::Live, A::ProviderLimit, Turn::Working, B::Running, true, SETTLE_MS),
        ("blocked", L::Live, A::Blocked, Turn::Working, B::Running, true, SETTLE_MS),
        ("needs_human", L::Live, A::NeedsHuman, Turn::Working, B::Running, true, SETTLE_MS),
        ("working", L::Live, A::None, Turn::Working, B::Running, true, SETTLE_MS),
        ("waiting_on_event", L::Live, A::None, Turn::Idle, B::Running, true, SETTLE_MS),
        ("finished_with_background", L::Live, A::None, Turn::Idle, B::Running, false, SETTLE_MS),
        ("finished", L::Live, A::None, Turn::Idle, B::None, false, SETTLE_MS),
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
    fn a_missing_or_future_state_falls_back_instead_of_failing() {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default, deserialize_with = "lenient")]
            state: Option<AgentState>,
        }
        let parse = |json: &str| serde_json::from_str::<Row>(json).unwrap().state;
        assert_eq!(parse("{}"), None);
        assert_eq!(
            parse(r#"{"state":{"turn":"idle","background":{"running":{}}}}"#),
            None
        );
        let renamed = parse(
            r#"{"state":{"turn":"paused","background":"unknown","attention":"unknown",
                "lifecycle":"live","phase":"stalled"}}"#,
        )
        .unwrap();
        assert_eq!(
            (renamed.turn, renamed.phase),
            (Turn::Unknown, Phase::Unknown)
        );
    }
}
