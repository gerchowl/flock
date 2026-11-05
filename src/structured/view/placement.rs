//! Placement persistence retains unsupported records without interpreting authority.
use super::Target;
use crate::structured::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::HashSet;

/// A layout placement, independent of runtime ownership or resume authority.
#[derive(Clone, Debug)]
pub(crate) enum View {
    /// A known agent bound to its current cold-registry generation.
    Ready(Target),
    /// A visible inert tile retained until explicit close.
    Unavailable {
        /// Original record retained without normalization or authority.
        saved: Saved,
        /// First failed validation in restore order.
        reason: UnavailableReason,
    },
}
/// Why a saved placement cannot acquire an attach target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    /// Malformed record, wrong field types, or unknown fields.
    InvalidRecord,
    /// Structurally valid record for an unsupported view kind.
    UnknownKind,
    /// Structured placement with a schema other than version one.
    UnsupportedSchema,
    /// Agent identity is absent from the restored registry.
    MissingAgent,
    /// An earlier placement already claimed this agent.
    DuplicateAgent,
}
/// Placement-only persistence: no credentials, generation, PID, or restart plan.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Saved {
    /// Canonical record produced by saving a ready placement.
    Current(Placement),
    /// Deserialized JSON retained byte-for-byte inside the record boundary.
    Opaque(Box<RawValue>),
}
/// Version-one placement identity. Extra fields are retained as invalid raw records.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    /// Discriminator, currently `structured`.
    kind: String,
    /// Placement schema version, currently one.
    schema: u32,
    /// Stable local registry identity, without runtime authority.
    agent_id: String,
}
impl<'de> Deserialize<'de> for Saved {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(d).map(Self::Opaque)
    }
}
impl Saved {
    /// Validate and bind without starting a process. Validation checks shape, kind,
    /// schema, registry membership, then duplication. The caller shares `seen`
    /// across workspaces and tabs in layout traversal order: first valid claim wins.
    pub fn restore(&self, registry: &Registry, seen: &mut HashSet<String>) -> View {
        let parsed = match self {
            Self::Current(p) => Some(p.clone()),
            Self::Opaque(raw) => serde_json::from_str::<Placement>(raw.get()).ok(),
        };
        let reason = match parsed {
            None => UnavailableReason::InvalidRecord,
            Some(p) if p.kind != "structured" => UnavailableReason::UnknownKind,
            Some(p) if p.schema != 1 => UnavailableReason::UnsupportedSchema,
            Some(p) => match registry.get(&p.agent_id) {
                None => UnavailableReason::MissingAgent,
                Some(_) if !seen.insert(p.agent_id.clone()) => UnavailableReason::DuplicateAgent,
                Some((_, _, generation)) => {
                    return View::Ready(Target {
                        agent_id: p.agent_id,
                        generation,
                    })
                }
            },
        };
        tracing::warn!(
            ?reason,
            "native placement unavailable; saved record retained"
        );
        View::Unavailable {
            saved: self.clone(),
            reason,
        }
    }
}
impl View {
    /// Save ready identity only, or clone an unavailable record verbatim.
    pub fn saved(&self) -> Saved {
        match self {
            Self::Ready(target) => Saved::Current(Placement {
                kind: "structured".into(),
                schema: 1,
                agent_id: target.agent_id.clone(),
            }),
            Self::Unavailable { saved, .. } => saved.clone(),
        }
    }
    /// Return an attach target only for ready placements. Dispatch must revalidate it.
    pub fn target(&self) -> Option<&Target> {
        match self {
            Self::Ready(target) => Some(target),
            Self::Unavailable { .. } => None,
        }
    }
    /// Draw an unavailable tile without I/O or mutation, returning whether drawn.
    pub fn render_unavailable(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
    ) -> bool {
        let Self::Unavailable { reason, .. } = self else {
            return false;
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!("Unavailable native view: {reason:?}"))
                .block(ratatui::widgets::Block::bordered()),
            area,
        );
        true
    }
}
