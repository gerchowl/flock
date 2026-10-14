//! Claude stream-json output types (ADR-0036 §2, §8 and Appendix A.2).
//!
//! This is an adapter model, not flock's frontend-neutral event schema (§11).
//! JSON values round-trip semantically, not byte-for-byte: whitespace and key
//! order belong to the original stdout line, which the later S3 pump must retain.
//! Malformed JSON returns `serde_json::Error`, as required by S1's brief.
#![deny(missing_docs)]

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// A collision-free, version-scoped identity for once-per-shape logging (§8).
///
/// The string is a compact JSON array of harness, version, discriminator values
/// (`type`, top-level `subtype`, `event.type`, `request.subtype`, `response.subtype`)
/// and sorted top-level keys. Missing discriminators are null. Values, field
/// order and nested payload keys do not affect it. Non-objects have no keys.
/// The caller supplies the observed harness version, never flock's version.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShapeKey(
    /// Canonical JSON encoding of the shape identity.
    pub String,
);

impl ShapeKey {
    /// Derive a stable key for a JSON value and the observed Claude version.
    pub fn derive(raw: &Value, version: &str) -> Self {
        let mut keys: Vec<_> = raw.as_object().into_iter().flat_map(|o| o.keys()).collect();
        keys.sort_unstable();
        Self(
            serde_json::json!([
                "claude",
                version,
                [
                    raw.get("type"),
                    raw.get("subtype"),
                    raw.pointer("/event/type"),
                    raw.pointer("/request/subtype"),
                    raw.pointer("/response/subtype")
                ],
                keys
            ])
            .to_string(),
        )
    }
}

/// A stdio permission request, including AskUserQuestion (A.6, A.12-A.13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Harness tool name; interpretation belongs to the state adapter.
    pub tool_name: String,
    /// Tool-specific input, including question options when applicable.
    pub input: Map<String, Value>,
    /// Correlation with the assistant tool call.
    pub tool_use_id: String,
    /// Discriminator, suggestions and future control fields.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Provider limit observation, with provider-specific details kept intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Provider status, observed as allowed or a limit condition.
    pub status: String,
    /// Reset times, limit types and future provider metadata.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// Preserve envelope discriminators separately from unknown fields, so extra
// does not report a new shape solely because its wire tags are present.
macro_rules! events {
    ($($(#[$doc:meta])* $name:ident ($kind:literal, $path:literal, $sub:literal) {
        $($(#[$field_doc:meta])* $field:ident: $ty:ty),* $(,)?
    })*) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
            pub struct $name {
                #[serde(rename = "type")]
                kind: String,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                subtype: Option<String>,
                $($(#[$field_doc])* pub $field: $ty,)*
                /// Unmodelled envelope fields, including optional identity metadata.
                #[serde(flatten)]
                pub extra: Map<String, Value>,
            }
        )*

        /// One Claude stdout event, with lossless JSON fallback for new shapes.
        ///
        /// A known discriminator with incompatible fields also becomes Unknown.
        /// Heterogeneous API messages, SSE bodies and SDK metadata stay JSON;
        /// state interpretation belongs to S4, not this wire adapter.
        #[derive(Debug, Clone, PartialEq)]
        pub enum SessionEvent {
            $($(#[$doc])* $name($name),)*
            /// An unrecognised or incompatible JSON shape, preserved in full.
            Unknown {
                /// Original JSON value, without an invented event envelope.
                raw: Value,
                /// Version-scoped identity for the shape registry in S2.
                shape: ShapeKey,
            },
        }

        impl SessionEvent {
            /// Parse one stdout line using the observed Claude version.
            ///
            /// Only invalid JSON fails. Unknown pairs and incompatible known
            /// payloads return Unknown rather than losing the original value.
            pub fn parse(line: &str, version: &str) -> Result<Self, serde_json::Error> {
                Ok(Self::from_value(serde_json::from_str(line)?, version))
            }

            fn from_value(raw: Value, version: &str) -> Self {
                $(if raw.get("type").and_then(Value::as_str) == Some($kind)
                    && (if $path.is_empty() { raw.get("subtype").is_none() }
                        else { raw.pointer($path).and_then(Value::as_str) == Some($sub) })
                    && [$(stringify!($field)),*].iter().all(|key| raw.get(*key).is_some()) {
                    if let Ok(event) = serde_json::from_value::<$name>(raw.clone()) {
                        return Self::$name(event);
                    }
                })*
                let shape = ShapeKey::derive(&raw, version);
                Self::Unknown { raw, shape }
            }
        }

        impl Serialize for SessionEvent {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(Self::$name(event) => event.serialize(serializer),)*
                    Self::Unknown { raw, .. } => raw.serialize(serializer),
                }
            }
        }

        impl<'de> Deserialize<'de> for SessionEvent {
            /// Deserialize without version context; prefer `parse` for logging.
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(Self::from_value(Value::deserialize(deserializer)?, ""))
            }
        }
    };
}

events! {
    /// A complete assistant content block (A.2).
    Assistant("assistant", "", "") {
        /// The API message and its content blocks.
        message: Map<String, Value>,
    }
    /// A tool result or replayed user input (A.2, A.11-A.12).
    User("user", "", "") {
        /// The message, including tool results or replayed content.
        message: Map<String, Value>,
    }
    /// Input acknowledgement and completion (A.5, A.14).
    CommandLifecycle("command_lifecycle", "", "") {
        /// The input message UUID supplied by the host.
        command_uuid: String,
        /// Observed lifecycle state; open to future harness states.
        state: String,
    }
    /// Per-turn harness configuration (A.2, A.11, A.15).
    Init("system", "/subtype", "init") {
        /// The resolved model name.
        model: String,
        /// Harness release reported by the process.
        claude_code_version: String,
        /// Tool names available to this session.
        tools: Vec<String>,
    }
    /// Requesting, compacting or null status (A.2).
    Status("system", "/subtype", "status") {
        /// Current status; null means no active status.
        status: Option<String>,
    }
    /// Estimated reasoning token progress (A.2).
    ThinkingTokens("system", "/subtype", "thinking_tokens") {
        /// Estimated total thinking tokens.
        estimated_tokens: u64,
        /// Change in the estimate.
        estimated_tokens_delta: i64,
    }
    /// A completed compaction boundary (A.3).
    CompactBoundary("system", "/subtype", "compact_boundary") {
        /// Trigger, token counts and preserved-segment metadata.
        compact_metadata: Map<String, Value>,
    }
    /// A harness-reported tool denial (A.2, A.17).
    PermissionDenied("system", "/subtype", "permission_denied") {
        /// Tool refused by the harness.
        tool_name: String,
        /// Correlation with the assistant tool call.
        tool_use_id: String,
        /// Human-readable reason.
        message: String,
    }
    /// A hook began executing (A.1).
    HookStarted("system", "/subtype", "hook_started") {
        /// Hook event, such as Stop.
        hook_event: String,
        /// Hook execution identifier.
        hook_id: String,
        /// Configured hook name.
        hook_name: String,
    }
    /// A hook completed (A.1).
    HookResponse("system", "/subtype", "hook_response") {
        /// Hook event, such as Stop.
        hook_event: String,
        /// Hook execution identifier.
        hook_id: String,
        /// Configured hook name.
        hook_name: String,
    }
    /// A peer input was held or discarded (A.11).
    PeerMessageHold("system", "/subtype", "peer_message_hold") {
        /// Disposition of the held input.
        state: String,
        /// Input transport, observed as socket.
        lane: String,
        /// Policy reason for holding the input.
        cause: String,
    }
    /// Provider rate-limit status (A.2).
    RateLimit("rate_limit_event", "", "") {
        /// Status, reset time and provider limit metadata.
        rate_limit_info: RateLimitInfo,
    }
    /// Tool approval or AskUserQuestion request (A.6, A.12-A.13).
    CanUseTool("control_request", "/request/subtype", "can_use_tool") {
        /// Identifier the host must echo in its response.
        request_id: String,
        /// Tool name, input, suggestions and interaction requirements.
        request: ToolRequest,
    }
    /// Successful response to a host control request (A.5, A.14-A.15).
    ControlSuccess("control_response", "/response/subtype", "success") {
        /// Request identifier and verb-specific result.
        response: Map<String, Value>,
    }
    /// Rejected or unsupported host control request (A.14-A.15).
    ControlError("control_response", "/response/subtype", "error") {
        /// Request identifier, error text and optional error code.
        response: Map<String, Value>,
    }
    /// Start of an API message (A.2).
    MessageStart("stream_event", "/event/type", "message_start") {
        /// Raw SSE body, including the API message.
        event: Map<String, Value>,
    }
    /// Start of a text, thinking or tool block (A.2).
    ContentBlockStart("stream_event", "/event/type", "content_block_start") {
        /// Raw SSE body, including block index and content_block.
        event: Map<String, Value>,
    }
    /// Text, thinking, signature or tool-input delta (A.2).
    ContentBlockDelta("stream_event", "/event/type", "content_block_delta") {
        /// Raw SSE body, including block index and delta.
        event: Map<String, Value>,
    }
    /// End of one content block (A.2).
    ContentBlockStop("stream_event", "/event/type", "content_block_stop") {
        /// Raw SSE body with its block index.
        event: Map<String, Value>,
    }
    /// API message usage and stop metadata (A.2).
    MessageDelta("stream_event", "/event/type", "message_delta") {
        /// Raw SSE body, including usage and stop reason.
        event: Map<String, Value>,
    }
    /// End of an API message (A.2).
    MessageStop("stream_event", "/event/type", "message_stop") {
        /// Raw SSE body.
        event: Map<String, Value>,
    }
    /// Turn result, which can still have is_error true (A.5, A.12).
    Success("result", "/subtype", "success") {
        /// Truthful outcome flag, independent of subtype and process exit.
        is_error: bool,
    }
    /// Interrupted or failed execution (A.5, A.14).
    ExecutionError("result", "/subtype", "error_during_execution") {
        /// Truthful outcome flag.
        is_error: bool,
        /// Harness error descriptions.
        errors: Vec<String>,
    }
}

#[cfg(test)]
mod tests;
