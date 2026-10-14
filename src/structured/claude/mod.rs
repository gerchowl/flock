//! Claude stream-json output types (ADR-0036 §2, §8 and Appendix A.2).
//!
//! This is an adapter model, not flock's frontend-neutral event schema (§11).
//! Parsing is infallible, requires an observed harness version and retains the
//! exact original line for unknown shapes, including non-JSON diagnostics.
//!
//! Optional metadata uses `Option<Option<T>>`: absent is None, explicit null is
//! Some(None), and a value is Some(Some(value)). Under ADR-0036 §8, changing a
//! metadata value to null leaves the shape key unchanged; adding an absent key
//! changes its top-level key set. Required payloads and discriminators still
//! classify incompatible shapes as Unknown.
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

// A present JSON null must remain distinct from an absent optional field.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
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
    /// Required control discriminator; null is an unknown wire shape (§8).
    pub subtype: String,
    /// Harness `display_name` metadata from Appendix A.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub display_name: Option<Option<String>>,
    /// Harness `description` metadata from Appendix A.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub description: Option<Option<String>>,
    /// Harness `permission_suggestions` metadata from Appendix A.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub permission_suggestions: Option<Option<Vec<Value>>>,
    /// Harness `requires_user_interaction` metadata from Appendix A.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub requires_user_interaction: Option<Option<bool>>,
    /// Future control fields absent from the catalogue.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Provider limit observation, with provider-specific details kept intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Provider status, observed as allowed or a limit condition.
    pub status: String,
    /// Harness `resetsAt` metadata from Appendix A.
    #[serde(rename = "resetsAt")]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub resets_at: Option<Option<Value>>,
    /// Harness `rateLimitType` metadata from Appendix A.
    #[serde(rename = "rateLimitType")]
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub rate_limit_type: Option<Option<String>>,
    /// Future provider metadata absent from the catalogue.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// Preserve envelope discriminators separately from unknown fields, so extra
// does not report a new shape solely because its wire tags are present.
macro_rules! events {
    ($($(#[$doc:meta])* $name:ident ($kind:literal, $path:literal, $sub:literal) {
        $($(#[$field_doc:meta])* $field:ident: $ty:ty),* $(,)?
    } optional {
        $($(#[$optional_doc:meta])* $optional:ident: $optional_ty:ty),* $(,)?
    })*) => {
        $(
            $(#[$doc])*
            #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
            pub struct $name {
                #[serde(rename = "type")]
                kind: String,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                subtype: Option<String>,
                /// Harness session identifier, if emitted on this envelope.
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
                pub session_id: Option<Option<String>>,
                /// Transcript/event UUID, if emitted.
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
                pub uuid: Option<Option<String>>,
                /// Parent tool call for subagent output; explicit null is retained.
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
                pub parent_tool_use_id: Option<Option<String>>,
                /// Harness timestamp, in its original JSON representation.
                #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
                pub timestamp: Option<Option<Value>>,
                $($(#[$field_doc])* pub $field: $ty,)*
                $($(#[$optional_doc])*
                    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
                    pub $optional: Option<Option<$optional_ty>>,)*
                /// Fields not modelled by the Appendix A catalogue.
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
            $($(#[$doc])* $name(Box<$name>),)*
            /// An unrecognised or incompatible JSON shape, preserved in full.
            Unknown {
                /// Exact original input line, including whitespace and number spelling.
                raw: String,
                /// Parsed JSON, or the exact line as a JSON string for non-JSON.
                value: Value,
                /// Version-scoped identity for the shape registry in S2.
                shape: ShapeKey,
            },
        }

        impl SessionEvent {
            /// Parse one stdout line using the observed Claude version.
            ///
            /// Unknown pairs, incompatible payloads and non-JSON diagnostics all
            /// become Unknown. There is no versionless serde Deserialize path.
            pub fn parse(line: &str, version: &str) -> Self {
                let value = serde_json::from_str(line)
                    .unwrap_or_else(|_| Value::String(line.to_owned()));
                Self::from_value(value, line, version)
            }

            /// Unmodelled top-level fields, or None for an unknown event.
            /// A non-empty map marks a known event as a new shape (§8).
            pub fn extra(&self) -> Option<&Map<String, Value>> {
                match self {
                    $(Self::$name(event) => Some(&event.extra),)*
                    Self::Unknown { .. } => None,
                }
            }

            fn from_value(raw: Value, line: &str, version: &str) -> Self {
                $(if raw.get("type").and_then(Value::as_str) == Some($kind)
                    && (if $path.is_empty() { raw.get("subtype").is_none() }
                        else { raw.pointer($path).and_then(Value::as_str) == Some($sub) })
                    && [$(stringify!($field)),*].iter().all(|key| raw.get(*key).is_some()) {
                    if let Ok(event) = serde_json::from_value::<$name>(raw.clone()) {
                        return Self::$name(Box::new(event));
                    }
                })*
                let shape = ShapeKey::derive(&raw, version);
                Self::Unknown { raw: line.to_owned(), value: raw, shape }
            }
        }

        impl Serialize for SessionEvent {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(Self::$name(event) => event.serialize(serializer),)*
                    Self::Unknown { raw, .. } => match serde_json::value::RawValue::from_string(raw.clone()) {
                        Ok(json) => json.serialize(serializer),
                        Err(_) => raw.serialize(serializer),
                    },
                }
            }
        }

    };
}

events! {
    /// A complete assistant content block (A.2).
    Assistant("assistant", "", "") {
        /// The API message and its content blocks.
        message: Map<String, Value>,
    } optional {
        /// Harness `request_id` metadata from Appendix A.
        request_id: String,
        /// Harness `error` metadata from Appendix A.
        error: Value,
        /// Harness `is_api_error_message` metadata from Appendix A.
        is_api_error_message: bool,
        /// Harness `aborted` metadata from Appendix A.
        aborted: bool,
        /// Harness `thinking_display` metadata from Appendix A.
        thinking_display: Value,
        /// Harness `thinking_duration_ms` metadata from Appendix A.
        thinking_duration_ms: u64,
        /// Harness `tool_use_meta` metadata from Appendix A.
        tool_use_meta: Value,
        /// Harness `wire_tool_inputs` metadata from Appendix A.
        wire_tool_inputs: Value,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// A tool result or replayed user input (A.2, A.11-A.12).
    User("user", "", "") {
        /// The message, including tool results or replayed content.
        message: Map<String, Value>,
    } optional {
        /// Harness `isReplay` metadata from Appendix A.
        #[serde(rename = "isReplay")]
        is_replay: bool,
        /// Harness `isSynthetic` metadata from Appendix A.
        #[serde(rename = "isSynthetic")]
        is_synthetic: bool,
        /// Harness `tool_result_meta` metadata from Appendix A.
        tool_result_meta: Value,
        /// Harness `tool_use_result` metadata from Appendix A.
        tool_use_result: Value,
        /// Harness `origin` metadata from Appendix A.
        origin: Value,
    }
    /// Input acknowledgement and completion (A.5, A.14).
    CommandLifecycle("command_lifecycle", "", "") {
        /// The input message UUID supplied by the host.
        command_uuid: String,
        /// Observed lifecycle state; open to future harness states.
        state: String,
    } optional {
    }
    /// Per-turn harness configuration (A.2, A.11, A.15).
    Init("system", "/subtype", "init") {
        /// The resolved model name.
        model: String,
        /// Harness release reported by the process.
        claude_code_version: String,
        /// Tool names available to this session.
        tools: Vec<String>,
    } optional {
        /// Harness `agents` metadata from Appendix A.
        agents: Value,
        /// Harness `mcp_servers` metadata from Appendix A.
        mcp_servers: Value,
        /// Harness `memory_paths` metadata from Appendix A.
        memory_paths: Value,
        /// Harness `plugins` metadata from Appendix A.
        plugins: Value,
        /// Harness `skills` metadata from Appendix A.
        skills: Value,
        /// Harness `apiKeySource` metadata from Appendix A.
        #[serde(rename = "apiKeySource")]
        api_key_source: String,
        /// Harness `capabilities` metadata from Appendix A.
        capabilities: Vec<String>,
        /// Harness `cwd` metadata from Appendix A.
        cwd: String,
        /// Harness `messaging_socket_path` metadata from Appendix A.
        messaging_socket_path: String,
        /// Harness `output_style` metadata from Appendix A.
        output_style: String,
        /// Harness `permissionMode` metadata from Appendix A.
        #[serde(rename = "permissionMode")]
        permission_mode: String,
        /// Harness `slash_commands` metadata from Appendix A.
        slash_commands: Vec<String>,
    }
    /// Requesting, compacting or null status (A.2).
    Status("system", "/subtype", "status") {
        /// Current status; null means no active status.
        status: Option<String>,
    } optional {
        /// Harness `compact_result` metadata from Appendix A.
        compact_result: Value,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// Estimated reasoning token progress (A.2).
    ThinkingTokens("system", "/subtype", "thinking_tokens") {
        /// Estimated total thinking tokens.
        estimated_tokens: u64,
        /// Change in the estimate.
        estimated_tokens_delta: i64,
    } optional {
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
    }
    /// A completed compaction boundary (A.3).
    CompactBoundary("system", "/subtype", "compact_boundary") {
        /// Trigger, token counts and preserved-segment metadata.
        compact_metadata: Map<String, Value>,
    } optional {
        /// Harness `logical_parent_uuid` metadata from Appendix A.
        logical_parent_uuid: String,
    }
    /// A harness-reported tool denial (A.2, A.17).
    PermissionDenied("system", "/subtype", "permission_denied") {
        /// Tool refused by the harness.
        tool_name: String,
        /// Correlation with the assistant tool call.
        tool_use_id: String,
        /// Human-readable reason.
        message: String,
    } optional {
        /// Harness `decision_reason_type` metadata from Appendix A.
        decision_reason_type: String,
    }
    /// A hook began executing (A.1).
    HookStarted("system", "/subtype", "hook_started") {
        /// Hook event, such as Stop.
        hook_event: String,
        /// Hook execution identifier.
        hook_id: String,
        /// Configured hook name.
        hook_name: String,
    } optional {
    }
    /// A hook completed (A.1).
    HookResponse("system", "/subtype", "hook_response") {
        /// Hook event, such as Stop.
        hook_event: String,
        /// Hook execution identifier.
        hook_id: String,
        /// Configured hook name.
        hook_name: String,
    } optional {
        /// Harness `exit_code` metadata from Appendix A.
        exit_code: i32,
        /// Harness `outcome` metadata from Appendix A.
        outcome: String,
        /// Harness `output` metadata from Appendix A.
        output: Value,
        /// Harness `stdout` metadata from Appendix A.
        stdout: String,
        /// Harness `stderr` metadata from Appendix A.
        stderr: String,
    }
    /// A peer input was held or discarded (A.11).
    PeerMessageHold("system", "/subtype", "peer_message_hold") {
        /// Disposition of the held input.
        state: String,
        /// Input transport, observed as socket.
        lane: String,
        /// Policy reason for holding the input.
        cause: String,
    } optional {
    }
    /// Provider rate-limit status (A.2).
    RateLimit("rate_limit_event", "", "") {
        /// Status, reset time and provider limit metadata.
        rate_limit_info: RateLimitInfo,
    } optional {
    }
    /// Tool approval or AskUserQuestion request (A.6, A.12-A.13).
    CanUseTool("control_request", "/request/subtype", "can_use_tool") {
        /// Identifier the host must echo in its response.
        request_id: String,
        /// Tool name, input, suggestions and interaction requirements.
        request: ToolRequest,
    } optional {
    }
    /// Successful response to a host control request (A.5, A.14-A.15).
    ControlSuccess("control_response", "/response/subtype", "success") {
        /// Request identifier and verb-specific result.
        response: Map<String, Value>,
    } optional {
    }
    /// Rejected or unsupported host control request (A.14-A.15).
    ControlError("control_response", "/response/subtype", "error") {
        /// Request identifier, error text and optional error code.
        response: Map<String, Value>,
    } optional {
    }
    /// Start of an API message (A.2).
    MessageStart("stream_event", "/event/type", "message_start") {
        /// Raw SSE body, including the API message.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// Start of a text, thinking or tool block (A.2).
    ContentBlockStart("stream_event", "/event/type", "content_block_start") {
        /// Raw SSE body, including block index and content_block.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// Text, thinking, signature or tool-input delta (A.2).
    ContentBlockDelta("stream_event", "/event/type", "content_block_delta") {
        /// Raw SSE body, including block index and delta.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// End of one content block (A.2).
    ContentBlockStop("stream_event", "/event/type", "content_block_stop") {
        /// Raw SSE body with its block index.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// API message usage and stop metadata (A.2).
    MessageDelta("stream_event", "/event/type", "message_delta") {
        /// Raw SSE body, including usage and stop reason.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// End of an API message (A.2).
    MessageStop("stream_event", "/event/type", "message_stop") {
        /// Raw SSE body.
        event: Map<String, Value>,
    } optional {
        /// Harness `api_message_id` metadata from Appendix A.
        api_message_id: String,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `user_message_uuid` metadata from Appendix A.
        user_message_uuid: String,
        /// Harness `user_message_uuids` metadata from Appendix A.
        user_message_uuids: Vec<String>,
    }
    /// Turn result, which can still have is_error true (A.5, A.12).
    Success("result", "/subtype", "success") {
        /// Truthful outcome flag, independent of subtype and process exit.
        is_error: bool,
    } optional {
        /// Harness `result` metadata from Appendix A.
        result: String,
        /// Harness `stop_reason` metadata from Appendix A.
        stop_reason: String,
        /// Harness `terminal_reason` metadata from Appendix A.
        terminal_reason: String,
        /// Harness `api_error_status` metadata from Appendix A.
        api_error_status: u16,
        /// Harness `usage` metadata from Appendix A.
        usage: Map<String, Value>,
        /// Harness `modelUsage` metadata from Appendix A.
        #[serde(rename = "modelUsage")]
        model_usage: Map<String, Value>,
        /// Harness `total_cost_usd` metadata from Appendix A.
        total_cost_usd: f64,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `num_turns` metadata from Appendix A.
        num_turns: u64,
        /// Harness `permission_denials` metadata from Appendix A.
        permission_denials: Vec<Value>,
        /// Harness `queued_turn_count` metadata from Appendix A.
        queued_turn_count: u64,
    }
    /// Interrupted or failed execution (A.5, A.14).
    ExecutionError("result", "/subtype", "error_during_execution") {
        /// Truthful outcome flag.
        is_error: bool,
        /// Harness error descriptions.
        errors: Vec<String>,
    } optional {
        /// Harness `result` metadata from Appendix A.
        result: String,
        /// Harness `stop_reason` metadata from Appendix A.
        stop_reason: String,
        /// Harness `terminal_reason` metadata from Appendix A.
        terminal_reason: String,
        /// Harness `api_error_status` metadata from Appendix A.
        api_error_status: u16,
        /// Harness `usage` metadata from Appendix A.
        usage: Map<String, Value>,
        /// Harness `modelUsage` metadata from Appendix A.
        #[serde(rename = "modelUsage")]
        model_usage: Map<String, Value>,
        /// Harness `total_cost_usd` metadata from Appendix A.
        total_cost_usd: f64,
        /// Harness `ttft_ms` metadata from Appendix A.
        ttft_ms: u64,
        /// Harness `num_turns` metadata from Appendix A.
        num_turns: u64,
        /// Harness `permission_denials` metadata from Appendix A.
        permission_denials: Vec<Value>,
        /// Harness `queued_turn_count` metadata from Appendix A.
        queued_turn_count: u64,
    }
}

#[cfg(test)]
mod tests;
