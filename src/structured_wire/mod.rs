//! Structured JSON wire contracts, separate from the terminal's framed protocol.
//!
//! ADR-0036 §4/§5/§5a governs these types. S7a is test-only until S7 installs
//! an endpoint: no transport, authentication, timers or command execution lives here.
//! Frames tolerate additive fields; unknown `t` tags are protocol errors, rather
//! than session events. Only event payloads have §8's lossless unknown wrapper.
//! All u64 wire numbers are JSON numbers, exact in JavaScript below 2^53.
#![deny(missing_docs)]

use crate::structured::claude::SessionEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) mod fixture;
#[cfg(test)]
mod tests;

/// Initial, unreleased structured wire version; independent of terminal/mesh versions.
pub const WIRE_JSON_VERSION: u32 = 1;
/// Per-attempt connect ceiling, including resolution and upgrade (ADR-0036 §5a).
pub const CONNECT_DEADLINE_MS: u64 = 10_000;
/// WebSocket-open to authenticated welcome ceiling, on both peers.
pub const HANDSHAKE_DEADLINE_MS: u64 = 5_000;
/// Server application ping cadence, independent of other traffic.
pub const HEARTBEAT_INTERVAL_MS: u64 = 15_000;
/// Missing valid frame/pong ceiling; unrelated streams do not renew progress.
pub const LIVENESS_WINDOW_MS: u64 = 45_000;
/// Upgrade must decline permessage-deflate, including for native clients in S7.
pub const PERMESSAGE_DEFLATE: bool = false;
/// Hello authentication refusal: hold outbox until a new token.
pub const AUTH_REFUSED_CLOSE: u16 = 4401;
/// Open-socket token revocation: hold outbox until a new token.
pub const AUTH_REVOKED_CLOSE: u16 = 4403;

/// Client identification, never an authorization claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WireClient {
    /// Frontend name.
    pub name: String,
    /// Frontend build version.
    pub version: String,
}

/// One client JSON text frame. Unknown stream strings reach subfail, not decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    /// First frame after open; the server validates token, version and deadline.
    Hello {
        /// Compared against WIRE_JSON_VERSION, not PROTOCOL_VERSION.
        protocol: u32,
        /// Diagnostic identity.
        client: WireClient,
        /// Missing token is an authentication refusal; never log this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// Subscribe by identity; after is the last applied cursor, never skipped_to.
    Sub {
        /// Connection-local subscription identity.
        sub_id: String,
        /// ADR-0036 §5 stream path; an unknown/removed path gets unknown_stream.
        stream: String,
        /// Omitted for bootstrap and cursorless agent/<id>/live.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        after: Option<u64>,
    },
    /// Echo only a current connection's server ping nonce, immediately.
    Pong {
        /// Opaque outstanding ping identity; invalid/replayed values renew nothing.
        nonce: String,
    },
    /// End just this subscription.
    Unsub {
        /// Identity supplied in sub.
        sub_id: String,
    },
    /// Durable idempotent command admission; remote-agent commands remain refused.
    Cmd {
        /// Stable idempotency key, unchanged across reconnect.
        cmd_id: String,
        /// Existing socket API verb.
        verb: String,
        /// Opaque until S8b #1677 supplies per-verb types.
        args: Value,
        /// False only for first send; true for uncertain-delivery lookups.
        resume_only: bool,
    },
}

/// Subscription-only refusals (ADR-0036 §5a); sibling streams stay connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SubfailCode {
    /// Clear only this stream's state/cursor and bootstrap without after.
    CursorAhead,
    /// Stop until the principal changes.
    OperatorOnly,
    /// Stop until the user selects an existing supported stream.
    UnknownStream,
    /// Stop until read authorization changes.
    Scope,
}

/// Transport/recovery command codes; per-verb additions belong to S8b #1677.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    /// Token removed/replaced; ack precedes close 4403.
    AuthRevoked,
    /// Valid token lacks command scope; keep allowed operations connected.
    Scope,
    /// Original effect is uncertain; never dispatch a missing resume-only claim.
    SessionUnconfirmed,
    /// Unknown command on an older server, exit PROTOCOL_GAP.
    ServerVersionGap,
    /// Same command id reused with different arguments.
    InvalidParams,
    /// Cross-node command refused until #1380.
    UnsupportedForAgent,
    /// Old writer temporarily refuses admission during handoff preparation.
    RestartPending,
}

/// ADR-0029 retry spellings; these do not override §5a's resume-only safety rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WireRetry {
    /// No retryable operation.
    #[serde(rename = "n/a")]
    NotApplicable,
    /// Correct the call or selection.
    Never,
    /// Unclassified failure.
    Maybe,
    /// Safe to repeat unchanged.
    Yes,
    /// Retry once the condition clears, keeping the original command identity.
    Later,
    /// Inspect subject state before a new action.
    AfterInspecting,
    /// Human intervention required.
    AfterHuman,
    /// Upgrade/handoff required.
    AfterHandoff,
}

/// ADR-0029 §4 error envelope, unchanged inside an unsuccessful ack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct WireError {
    /// Registered API code, including auth_revoked, scope and session_unconfirmed.
    pub code: WireErrorCode,
    /// Human diagnosis, without bearer values or secret command content.
    pub message: String,
    /// cli::exit status; session_unconfirmed uses UNCONFIRMED for this surface.
    pub exit: i32,
    /// Existing ADR-0029 retry spelling; resume_only remains mandatory for recovery.
    pub retry: WireRetry,
    /// Optional structured details follow the existing error.data convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "present_json")]
    pub data: Option<Value>,
}

/// The §8 wire wrapper, distinct from the adapter's bare raw serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct UnknownEvent {
    /// Literal discriminator so clients can show a dump without interpreting raw.
    pub r#type: UnknownTag,
    /// Exact adapter line, including non-JSON diagnostics and numeric spelling.
    pub raw: String,
    /// Sender's harness/version-scoped shape key; clients never recompute it.
    pub shape: String,
}

/// Unknown payload discriminator; this is not an unknown transport frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum UnknownTag {
    /// Render raw and shape as ADR-0036 §8's collapsible dump.
    Unknown,
}

/// Untagged stream payload: known S8a agent event, §8 wrapper, or S8b opaque JSON.
///
/// Opaque is for non-agent streams only (#1677). SessionEvent's raw Unknown is
/// never sent directly: the sender wraps it with its shape. S8a's TS union still
/// includes unknown; narrowability is tracked separately in #1656.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum WireEvent {
    /// Unknown wrapper is recognized before known/opaque payloads.
    Unknown(UnknownEvent),
    /// Known Claude adapter event, serialized with its existing S8a shape.
    Session(#[serde(deserialize_with = "known_session")] Box<SessionEvent>),
    /// Other stream payload, deliberately uninterpreted until #1677.
    Opaque(Value),
}

fn known_session<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Box<SessionEvent>, D::Error> {
    let raw = Value::deserialize(d)?;
    // Only known events use this path; unknown shape identity comes from the sender.
    let event = SessionEvent::parse(&raw.to_string(), "wire");
    if matches!(event, SessionEvent::Unknown { .. }) {
        return Err(serde::de::Error::custom(
            "agent payload requires the unknown wrapper",
        ));
    }
    Ok(Box::new(event))
}

/// One server JSON text frame. Stateful ordering/authorization belongs to S7/G3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Authenticated handshake; a newer client receives read-only capabilities.
    Welcome {
        /// This server's structured wire version.
        protocol: u32,
        /// Server build version.
        server_version: String,
        /// Advertised operations; no command capability in ReadOnly.
        capabilities: Vec<String>,
        /// Token's principal name, never its address or bearer value.
        principal: String,
        /// Fresh random diagnostic epoch; never resets cursors or dedupe.
        server_epoch: String,
        /// Effective first-admission retention window, in milliseconds.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        idempotency_window_ms: u64,
    },
    /// Independent application heartbeat, prioritized ahead of replay messages.
    Ping {
        /// Unique bounded nonce echoed by pong.
        nonce: String,
    },
    /// Apply through the stream reducer before persisting a cursor.
    Ev {
        /// Subscription identity.
        sub_id: String,
        /// Same stream identity supplied in sub.
        stream: String,
        /// Seq streams only; agent/<id>/live omits seq and never advances a cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        seq: Option<u64>,
        /// Server receive timestamp in milliseconds, not a liveness clock.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ts_ms: u64,
        /// Known agent event, explicit unknown wrapper, or opaque S8b payload.
        ev: WireEvent,
    },
    /// Once per subscription on every seq stream, after its captured replay head.
    Synced {
        /// Subscription identity.
        sub_id: String,
        /// Captured head; zero only for a genuinely new empty stream.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        seq: u64,
    },
    /// End only this subscription with a typed recovery/refusal reason.
    Subfail {
        /// Subscription identity.
        sub_id: String,
        /// Defined client action.
        code: SubfailCode,
        /// Visible diagnosis.
        message: String,
    },
    /// Missing history, or inline snapshot announcement (ADR-0036 §4).
    Gap {
        /// Subscription identity.
        sub_id: String,
        /// First missing seq; bootstrap starts at one.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        from: u64,
        /// Last pre-checkpoint seq; bootstrap may be an empty 1..0 range.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        to: u64,
        /// Required for snapshot streams; equal to to means immediate empty completion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        snapshot_to: Option<u64>,
    },
    /// Slow subscriber lost buffered frames; re-subscribe from the applied cursor.
    Lagged {
        /// Subscription identity.
        sub_id: String,
        /// Diagnostic only; never a cursor to apply.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        skipped_to: u64,
    },
    /// Command admission/outcome, not asynchronous work completion.
    Ack {
        /// Original stable idempotency key.
        cmd_id: String,
        /// True with result, false with error; fixture validation checks exclusivity.
        ok: bool,
        /// Opaque until S8b #1677 supplies per-verb results.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "present_json")]
        result: Option<Value>,
        /// ADR-0029 envelope, including uncertain session_unconfirmed outcomes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
    },
}

fn present_json<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
