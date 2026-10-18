//! Durable queue data, independent of scheduling and runtime (ADR-0036 §3).
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Authenticated origin, supplied by the server rather than prompt text.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Producer {
    /// Operator composer.
    Human,
    /// Sending agent identity.
    Agent(String),
    /// Authenticated channel binding and user.
    Channel {
        /// Configured binding identity.
        binding: String,
        /// Authenticated user identity.
        user: String,
    },
    /// Server work source.
    System(String),
    /// Returning fork identity.
    Fork(String),
    /// Future variant name retained verbatim for positional replay (ADR-0028 §3a).
    #[serde(untagged)]
    Unknown(Value),
}

/// Queue input purpose; scheduling is implemented in #1802.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Human input.
    Prompt,
    /// Agent or channel input.
    Message,
    /// Answer to a question.
    Reply,
    /// Delegate completion.
    DelegateResult,
    /// Registered wait wakeup.
    Wake,
    /// Runtime reload notice.
    ReloadNotice,
    /// Fork finding.
    Finding,
    /// Isolated harness command.
    Command,
    /// Input after an anchored command's result.
    Continuation,
    /// Future variant name retained verbatim for positional replay (ADR-0028 §3a).
    #[serde(untagged)]
    Unknown(Value),
}

/// Persisted queue state; a completed write alone never means delivered.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Awaiting delivery.
    Queued,
    /// Requires explicit release.
    Held,
    /// In a named stash.
    Stashed,
    /// Harness reported started.
    Delivered,
    /// Turn result observed.
    Done,
    /// Harness cancelled the input.
    Cancelled,
    /// Policy or operator dropped it.
    Dropped,
    /// Future variant name retained verbatim for positional replay (ADR-0028 §3a).
    #[serde(untagged)]
    Unknown(Value),
}

/// Ordering, independent of delivery timing.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Default FIFO order.
    Normal,
    /// Before normal items at the same delivery point.
    Urgent,
    /// Future variant name retained verbatim for positional replay (ADR-0028 §3a).
    #[serde(untagged)]
    Unknown(Value),
}

/// Timing only; the MVP scheduler accepts at_idle (#1802).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Wait for the current result.
    AtIdle,
    /// Explicit future send-now mode.
    Steer,
    /// Explicit future interrupt mode.
    Interrupt,
    /// Future variant name retained verbatim for positional replay (ADR-0028 §3a).
    #[serde(untagged)]
    Unknown(Value),
}

// Unknown names are forward compatible; malformed known payloads are corruption.
// Inspect the externally tagged representation before invoking a strict decoder.
fn variant_name(value: &Value) -> Option<&str> {
    match value {
        Value::String(name) => Some(name),
        Value::Object(fields) if fields.len() == 1 => fields.keys().next().map(String::as_str),
        _ => None,
    }
}

impl<'de> Deserialize<'de> for Producer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Known {
            Human,
            Agent(String),
            Channel { binding: String, user: String },
            System(String),
            Fork(String),
        }
        let value = Value::deserialize(deserializer)?;
        let name = variant_name(&value)
            .ok_or_else(|| serde::de::Error::custom("queue producer requires a variant name"))?;
        if !matches!(name, "human" | "agent" | "channel" | "system" | "fork") {
            return Ok(Self::Unknown(value));
        }
        let known = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(match known {
            Known::Human => Self::Human,
            Known::Agent(id) => Self::Agent(id),
            Known::Channel { binding, user } => Self::Channel { binding, user },
            Known::System(source) => Self::System(source),
            Known::Fork(id) => Self::Fork(id),
        })
    }
}

macro_rules! deserialize_named_enum {
    ($enum:ident { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl<'de> Deserialize<'de> for $enum {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(Deserialize)]
                #[serde(rename_all = "snake_case")]
                enum Known { $($variant),+ }
                let value = Value::deserialize(deserializer)?;
                let name = variant_name(&value).ok_or_else(|| {
                    serde::de::Error::custom("queue enum requires a variant name")
                })?;
                if !matches!(name, $($name)|+) {
                    return Ok(Self::Unknown(value));
                }
                let known = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(match known { $(Known::$variant => Self::$variant),+ })
            }
        }
    };
}

deserialize_named_enum!(Kind {
    Prompt => "prompt", Message => "message", Reply => "reply",
    DelegateResult => "delegate_result", Wake => "wake", ReloadNotice => "reload_notice",
    Finding => "finding", Command => "command", Continuation => "continuation",
});
deserialize_named_enum!(State {
    Queued => "queued", Held => "held", Stashed => "stashed", Delivered => "delivered",
    Done => "done", Cancelled => "cancelled", Dropped => "dropped",
});
deserialize_named_enum!(Priority { Normal => "normal", Urgent => "urgent" });
deserialize_named_enum!(Delivery { AtIdle => "at_idle", Steer => "steer", Interrupt => "interrupt" });

/// Entire item state, used identically for appends and checkpoint snapshots.
/// Extension metadata preserves anchors, leases, gate decisions and future
/// optional fields across an older writer's checkpoint without interpreting them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Server-minted message UUID.
    pub item_id: String,
    /// Trusted origin.
    pub producer: Producer,
    /// Purpose of this input.
    pub kind: Kind,
    /// Scheduling state.
    pub state: State,
    /// Delivery point.
    pub delivery: Delivery,
    /// Ordering class.
    pub priority: Priority,
    /// Prompt or message text.
    pub body: String,
    /// Content-addressed blob references.
    pub attachments: Vec<String>,
    /// Optional metadata retained losslessly; interpreted by later slices.
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

/// A complete item change; position is its zero-based position after replacement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    /// Add, edit, reorder or transition one item atomically.
    Upsert {
        /// Complete replacement state.
        item: Box<Item>,
        /// Final zero-based queue position.
        position: usize,
    },
    /// Remove retained state, leaving a durable removal for subscribers.
    Remove {
        /// Identity whose retained state is removed.
        item_id: String,
    },
}

/// Dense per-agent sequence plus one durable change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Cursor within `queue/<agent>`, unrelated to the global event log.
    pub seq: u64,
    /// Item change.
    #[serde(flatten)]
    pub change: Change,
}

impl Change {
    pub(super) fn has_unknown(&self) -> bool {
        match self {
            Self::Upsert { item, .. } => {
                matches!(item.producer, Producer::Unknown(_))
                    || matches!(item.kind, Kind::Unknown(_))
                    || matches!(item.state, State::Unknown(_))
                    || matches!(item.priority, Priority::Unknown(_))
                    || matches!(item.delivery, Delivery::Unknown(_))
            }
            Self::Remove { .. } => false,
        }
    }

    pub(super) fn apply(&self, items: &mut Vec<Item>) -> std::io::Result<()> {
        if let Self::Upsert { item, .. } = self {
            if item.metadata.keys().any(|k| {
                matches!(
                    k.as_str(),
                    "item_id"
                        | "producer"
                        | "kind"
                        | "state"
                        | "delivery"
                        | "priority"
                        | "body"
                        | "attachments"
                )
            }) {
                return Err(std::io::Error::other("reserved queue metadata key"));
            }
        }
        let id = match self {
            Self::Upsert { item, .. } => &item.item_id,
            Self::Remove { item_id } => item_id,
        };
        items.retain(|item| &item.item_id != id);
        if let Self::Upsert { item, position } = self {
            if *position > items.len() {
                return Err(std::io::Error::other("invalid queue position"));
            }
            items.insert(*position, *item.clone());
        }
        Ok(())
    }
}
