//! Structured JSON wire contracts, separate from the terminal's framed protocol.
//!
//! ADR-0036 §4/§5/§5a governs these types. The web-feature transport core consumes
//! these types; no authentication, timers or command execution lives in this module.
//! Frames tolerate additive fields; unknown `t` tags are protocol errors, rather
//! than session events. Only event payloads have §8's lossless unknown wrapper.
//! All u64 wire numbers are JSON numbers, exact in JavaScript below 2^53.
#![deny(missing_docs)]

use crate::structured::claude::KnownSessionEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
pub(crate) mod fixture;
mod minimal;
// Later wire slices consume these exact payload re-exports.
#[cfg_attr(feature = "web", allow(unused_imports))]
pub use minimal::{
    AgentDecide, AgentDecideErrorCode, AgentDecideResult, AgentSendResult, FleetAgentRow,
    FleetDelta, PermissionDecision,
};
#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;

/// Initial, unreleased structured wire version; independent of terminal/mesh versions.
pub const WIRE_JSON_VERSION: u32 = 1;
/// Per-attempt connect ceiling, including resolution and upgrade (ADR-0036 §5a).
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const CONNECT_DEADLINE_MS: u64 = 10_000;
/// WebSocket-open to authenticated welcome ceiling, on both peers.
pub const HANDSHAKE_DEADLINE_MS: u64 = 5_000;
/// Server application ping cadence, independent of other traffic.
pub const HEARTBEAT_INTERVAL_MS: u64 = 15_000;
/// Missing valid frame/pong ceiling; unrelated streams do not renew progress.
pub const LIVENESS_WINDOW_MS: u64 = 45_000;
/// Subscription progress ceiling, independent of unrelated traffic.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const SUBSCRIPTION_DEADLINE_MS: u64 = 30_000;
/// Command acknowledgement ceiling; expiry marks only that command uncertain.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const ACK_DEADLINE_MS: u64 = 30_000;
/// First reconnect backoff ceiling, before equal jitter.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const BACKOFF_BASE_MS: u64 = 500;
/// Exponential multiplier per unsuccessful attempt.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const BACKOFF_FACTOR: u32 = 2;
/// Maximum reconnect backoff ceiling.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const BACKOFF_CAP_MS: u64 = 30_000;
/// Equal jitter lower bound is the ceiling divided by this value.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const BACKOFF_JITTER_DIVISOR: u32 = 2;
/// Continuous healthy interval before backoff resets.
// Client timers and later wire slices consume this contract constant.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const BACKOFF_RESET_MS: u64 = 60_000;
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
        /// Fresh connection-local identity, never reused even after this sub closes.
        sub_id: String,
        /// ADR-0036 §5 stream path; an unknown/removed path gets unknown_stream.
        stream: String,
        /// JSON number; exact in JavaScript below 2^53.
        /// Omitted for bootstrap and cursorless `agent/<id>/live`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        after: Option<u64>,
    },
    /// Echo only a current connection's server ping nonce, immediately.
    Pong {
        /// Opaque outstanding ping identity; invalid/replayed values renew nothing.
        nonce: String,
    },
    /// Close this subscription; clients drop late frames for this identity.
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
    /// Subscription identity was already used on this connection; allocate a fresh one.
    SubIdReused,
}

/// Token removed or replaced: acknowledge `auth_revoked` before 4403 socket fan-out.
// Later command admission emits this revocation wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const AUTH_REVOKED: &str = "auth_revoked";
/// Valid token lacks the requested scope; refuse the operation without executing it.
// Later scoped command/subscription admission emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const SCOPE: &str = "scope";
/// Unsupported stream path: refuse this subscription until a supported path is selected.
// Later subscription routing emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const UNKNOWN_STREAM: &str = "unknown_stream";
/// Cursor exceeds the stream head: clear only that stream and bootstrap without `after`.
// Later replay admission emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const CURSOR_AHEAD: &str = "cursor_ahead";
/// Current principal cannot read an operator-only stream; require a different principal.
// Later operator-stream authorization emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const OPERATOR_ONLY: &str = "operator_only";
/// Subscription ID was already used on this connection: allocate a fresh ID.
// Later subscription identity tracking emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const SUB_ID_REUSED: &str = "sub_id_reused";
/// Original command outcome is uncertain; a missing resume-only claim never dispatches.
// Later idempotent command recovery emits this wire spelling.
#[cfg_attr(feature = "web", allow(dead_code))]
pub const SESSION_UNCONFIRMED: &str = "session_unconfirmed";

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
    pub code: String,
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

/// Untagged payload: explicit unknown wrapper, known catalogue, or S8b JSON.
///
/// JsonValue is reserved for non-agent streams until #1677. Agent payloads must
/// decode as KnownSessionEvent or use the sender's explicit unknown wrapper.
/// #1656 owns the remaining nested literal discriminators in the catalogue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
pub enum WireEvent {
    /// Exact §8 wrapper, recognized before other payloads.
    Unknown(UnknownEvent),
    /// Known catalogue projection, with no adapter raw fallback.
    Session(Box<KnownSessionEvent>),
    /// Deliberate non-agent JSON, not a fallback accepted on agent streams.
    Json(Value),
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
        /// JSON number; exact in JavaScript below 2^53.
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
        /// JSON number; exact in JavaScript below 2^53.
        /// Seq streams only; `agent/<id>/live` omits seq and never advances a cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        seq: Option<u64>,
        /// Server receive timestamp in milliseconds, not a liveness clock.
        /// JSON number; exact in JavaScript below 2^53.
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
        /// JSON number; exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        seq: u64,
    },
    /// Close only this subscription with a typed refusal; drop later frames for its ID.
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
        /// JSON number; exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        from: u64,
        /// Last pre-checkpoint seq; bootstrap may be an empty 1..0 range.
        /// JSON number; exact in JavaScript below 2^53.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        to: u64,
        /// JSON number; exact in JavaScript below 2^53.
        /// Required for snapshot streams; equal to to means immediate empty completion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        snapshot_to: Option<u64>,
    },
    /// Close this subscription after lost buffered frames; the server emits no more on its ID.
    /// Re-subscribe with a fresh ID from the applied cursor, without an unsub.
    Lagged {
        /// Subscription identity.
        sub_id: String,
        /// Diagnostic only; never a cursor to apply.
        /// JSON number; exact in JavaScript below 2^53.
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

#[cfg(test)]
mod round3_tests;
