//! Frozen S10b/S10c projection/render boundary (ADR-0036 §11, S10a2).
//! Markdown includes fenced code. Render borrows; intents carry no token/lease authority.
use super::Target;
use crate::structured::claude::ToolRequest;
/// Exact permission-book identity, not an authorization capability.
pub(crate) use crate::structured::permissions::grants::GrantHandle as PermissionHandle;
use crate::structured::queue::model::Item;
use crate::structured_wire::{PermissionDecision, PermissionRecord};
use std::collections::BTreeSet;

/// Coherent projection cut borrowed by a pure renderer (ADR-0036 §11).
/// Builders own subscriptions and I/O. Neither snapshots nor intents confer authority.
pub(crate) struct Snapshot {
    /// Generation-fenced identity matching `agent.agent_id`. Retire on generation change.
    pub target: Target,
    /// Canonical registry metadata and state. Its model is requested, not observed.
    pub agent: crate::api::structured_agent::StructuredAgentInfo,
    /// Harness-observed model, absent when unknown rather than inferred.
    pub reported_model: Option<String>,
    /// Explicit launch permission mode. A request is not an observed grant.
    pub requested_permissions: crate::structured::launch::PermissionMode,
    /// Harness-observed permissions, absent when unknown. Never operator authority.
    pub reported_permissions: Option<String>,
    /// Cards in original stream order, retaining stable source identities on updates.
    pub transcript: Vec<Card>,
    /// Highest contiguous original cursor actually applied, including explicit gaps.
    /// Resume after this cursor, never after a received or advertised head.
    pub applied_cursor: u64,
    /// Subscription state, separate from progress actually applied to the projection.
    pub replay: Replay,
    /// Observed counters only, without estimates or fabricated zeroes.
    pub usage: Option<Usage>,
    /// Permission-book records in durable publication order, never map order.
    /// A close may lack an earlier open. Displaying a record grants no authority.
    pub permissions: Vec<PermissionRecord>,
    /// Owner FIFO order and lifecycle, never inferred from transcript text.
    /// Admission does not imply delivery, start, or completion.
    pub queue: Vec<Item>,
    /// Submission admission state, independent of harness activity.
    pub admission: Admission,
    /// Controller-owned structured-send draft, never terminal input.
    pub composer: Composer,
    /// Source cursor anchoring paused scroll despite new output. `None` follows tail.
    pub scroll_anchor: Option<u64>,
    /// Unseen-output indicator while paused, without moving the anchor.
    pub new_output: bool,
    /// Folded card identities, independent of siblings at the same source cursor.
    pub collapsed: BTreeSet<CardId>,
    /// Exact selected book handle. Closed or stale handles cannot authorize a decision.
    pub selected_permission: Option<PermissionHandle>,
}
/// Card identity within one source record's projection.
/// Ordinals are deterministic for a given record and projection parser version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CardId {
    /// Original source cursor; never a synthetic per-card stream sequence.
    pub seq: u64,
    /// Position among cards projected from that source record.
    pub ordinal: u32,
}
/// Observed assistant tool use, separate from permission control and authority.
#[derive(serde::Deserialize)]
pub(crate) struct ObservedToolUse {
    /// Original tool-use identity, matched by tool results.
    pub id: String,
    /// Observed tool name, never a request to execute it.
    pub name: String,
    /// Typed tool input; exact spelling and future fields remain in `raw`.
    pub input: serde_json::Map<String, serde_json::Value>,
    /// Exact source event for inspection, including unknown fields and whitespace.
    #[serde(skip)]
    pub raw: String,
}
/// One projected card with a stable originating stream identity.
pub(crate) struct Card {
    /// Independent identity used by folding and inspection.
    pub id: CardId,
    /// Original source cursor, equal to `id.seq`.
    pub seq: u64,
    /// Untrusted display content. Renderers escape controls and never execute it.
    pub content: Content,
    /// Partial-card state, finalized by later updates. Not a liveness signal.
    pub streaming: bool,
}
/// Typed display payloads. Rendering performs no attachment lookup or other I/O.
pub(crate) enum Content {
    /// Plain text rendered literally.
    Text(String),
    /// Markdown including fenced code, with terminal controls escaped.
    Markdown(String),
    /// Permission-control payload, never fabricated from an observed invocation.
    ToolRequest(ToolRequest),
    /// Observed tool invocation, correlated with results by its original id.
    ToolUse(ObservedToolUse),
    /// Observed result correlated by tool-use identity.
    ToolResult {
        /// Identity of the originating tool invocation.
        tool_use_id: String,
        /// Untrusted result text.
        text: String,
        /// Whether the harness reported a tool error.
        is_error: bool,
    },
    /// Harness-reported thinking content, without local inference.
    Thinking(String),
    /// Content-addressed attachment reference, resolved outside render.
    Attachment(crate::structured::blobs::BlobRef),
    /// Exact raw event text and observed sender shape, never silently discarded.
    RawJson {
        /// Recorded classifier provenance, not a newer reclassification.
        known: Option<bool>,
        /// Observed sender shape when recorded.
        shape: Option<crate::structured::claude::ShapeKey>,
        /// Exact source payload, retained even when unsupported.
        raw: String,
    },
    /// Explicit inclusive missing range, applied as cursor progress.
    Gap {
        /// First missing original cursor.
        from: u64,
        /// Last missing original cursor.
        to: u64,
        /// History classifier reason for this range.
        reason: crate::structured::history::GapReason,
    },
}
/// Replay lifecycle. Advertised heads never replace `Snapshot.applied_cursor`.
pub(crate) enum Replay {
    /// Replaying toward the captured subscription head.
    Replaying {
        /// Captured head, not applied progress.
        through: u64,
    },
    /// Caught up through the advertised cursor.
    Synced {
        /// Subscription head observed by the producer.
        through: u64,
    },
    /// Recovery required: resubscribe after the applied cursor.
    Lagged {
        /// Notification head, never a recovery cursor.
        through: u64,
    },
    /// Producer ended. Drain queued items before stopping the consumer.
    Ended,
}
/// Observations only: absence is not a zero or a local token estimate.
pub(crate) struct Usage {
    /// Original cursor of the latest usage observation.
    pub observed_seq: u64,
    /// Observed input tokens, absent when unreported.
    pub input_tokens: Option<u64>,
    /// Observed output tokens, absent when unreported.
    pub output_tokens: Option<u64>,
    /// Observed cache-read tokens, absent when unreported.
    pub cache_read_tokens: Option<u64>,
    /// Observed cache-write tokens, absent when unreported.
    pub cache_write_tokens: Option<u64>,
}
/// Durable submission state used to preserve drafts and prevent duplicate sends.
pub(crate) enum Admission {
    /// No admission in flight. Does not guarantee an idle harness.
    Ready,
    /// Admission pending or uncertain. Retain the draft and do not retry blindly.
    Pending,
    /// Typed refusal. Retain the draft for correction.
    Refused(crate::api::error_code::ErrorCode),
}
/// Unicode draft submitted through structured send at the admitted idle boundary.
pub(crate) struct Composer {
    /// Draft text preserving Unicode and newlines.
    pub text: String,
    /// UTF-8 byte boundary at most `text.len()`, maintained by the input controller.
    pub cursor: usize,
}
/// Data-only controller intent. The dispatcher derives authenticated authority
/// and revalidates the target generation before any effect.
pub(crate) struct Intent {
    /// Target of the originating projection, revalidated against the live registry.
    pub target: Target,
    /// Requested action, subject to current runtime and permission-book validation.
    pub action: Action,
}
/// Frozen controller actions. Rendering emits no side effects.
pub(crate) enum Action {
    /// Detach the placement without stopping its runtime.
    Close,
    /// Explicitly request runtime stop under current registry policy.
    Stop,
    /// Explicitly request resume admission under current registry policy.
    Resume,
    /// Submit the draft durably, retaining it on refusal or uncertainty.
    Send(Composer),
    /// Toggle folding for a source card identity.
    ToggleFold(CardId),
    /// Inspect a source card without changing runtime state.
    InspectCard(CardId),
    /// Inspect the exact permission-book handle without deciding it.
    InspectPermission(PermissionHandle),
    /// Scroll older for negative lines, newer for positive lines, zero does nothing.
    ScrollLines(i16),
    /// Clear the paused anchor and unseen-output indicator.
    FollowTail,
    /// Request bounded history through the existing worker, never a second stdout reader.
    EarlierHistory,
    /// Select an exact permission-book handle for later explicit action.
    SelectPermission(PermissionHandle),
    /// Select older drafts for negative steps, newer for positive, zero does nothing.
    ComposerHistory(i16),
    /// Dispatcher revalidates the exact live handle and derives caller authority.
    Decide {
        /// Exact live request identity, revalidated for revocation and expiry.
        handle: PermissionHandle,
        /// Explicit operator choice. AllowAlways remains exact-input, session-only
        /// and confirmation-gated, ignoring suggested rules (S10e).
        decision: PermissionDecision,
        /// Optional operator message subject to the existing admission limits.
        message: Option<String>,
    },
}
