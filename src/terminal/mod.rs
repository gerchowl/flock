mod agent_id;
mod id;
mod runtime;
mod runtime_registry;
pub mod state;

pub use agent_id::AgentId;
pub use id::TerminalId;
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    compact_header_fields, middle_truncate_chars, validate_header_field, AgentMetadataReport,
    EffectivePresentation, EffectiveStateChange, HeaderFieldError, PromptHistoryEntry,
    PromptHistoryKind, TerminalState, TerminalStateMutation,
};
