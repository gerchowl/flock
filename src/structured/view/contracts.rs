//! Frozen S10b/S10c projection/render boundary (ADR-0036 §11, S10a2).
//! Markdown includes fenced code. Render borrows; intents carry no token/lease authority.
use super::Target;
use crate::structured::claude::ToolRequest;
pub(crate) use crate::structured::permissions::grants::GrantHandle as PermissionHandle;
use crate::structured::queue::model::Item;
use crate::structured_wire::{PermissionDecision, PermissionRecord};
use std::collections::BTreeSet;

pub(crate) struct Snapshot {
    pub target: Target,
    pub agent: crate::api::structured_agent::StructuredAgentInfo,
    pub reported_model: Option<String>,
    pub requested_permissions: crate::structured::launch::PermissionMode,
    pub reported_permissions: Option<String>,
    pub transcript: Vec<Card>,
    pub applied_cursor: u64,
    pub replay: Replay,
    pub usage: Option<Usage>,
    pub permissions: Vec<PermissionRecord>,
    pub queue: Vec<Item>,
    pub admission: Admission,
    pub composer: Composer,
    pub scroll_anchor: Option<u64>,
    pub new_output: bool,
    pub collapsed: BTreeSet<u64>,
    pub selected_permission: Option<PermissionHandle>,
}
pub(crate) struct Card {
    pub seq: u64,
    pub content: Content,
    pub streaming: bool,
}
pub(crate) enum Content {
    Text(String),
    Markdown(String),
    ToolRequest(ToolRequest),
    ToolResult {
        tool_use_id: String,
        text: String,
        is_error: bool,
    },
    Thinking(String),
    Attachment(crate::structured::blobs::BlobRef),
    /// Exact raw event text and observed sender shape, never silently discarded.
    RawJson {
        known: Option<bool>,
        shape: Option<crate::structured::claude::ShapeKey>,
        raw: String,
    },
    Gap {
        from: u64,
        to: u64,
        reason: crate::structured::history::GapReason,
    },
}
pub(crate) enum Replay {
    Replaying { through: u64 },
    Synced { through: u64 },
    Lagged { through: u64 },
    Ended,
}
/// Observations only: absence is not a zero or a local token estimate.
pub(crate) struct Usage {
    pub observed_seq: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}
pub(crate) enum Admission {
    Ready,
    Pending,
    Refused(crate::api::error_code::ErrorCode),
}
pub(crate) struct Composer {
    pub text: String,
    /// UTF-8 byte boundary in text, maintained by the input controller.
    pub cursor: usize,
}
pub(crate) struct Intent {
    pub target: Target,
    pub action: Action,
}
pub(crate) enum Action {
    Close,
    Stop,
    Resume,
    Send(Composer),
    ToggleFold(u64),
    InspectCard(u64),
    InspectPermission(PermissionHandle),
    ScrollLines(i16),
    FollowTail,
    EarlierHistory,
    SelectPermission(PermissionHandle),
    ComposerHistory(i16),
    /// Dispatcher revalidates the exact live handle and derives caller authority.
    Decide {
        handle: PermissionHandle,
        decision: PermissionDecision,
        message: Option<String>,
    },
}
