//! Placement persistence retains unsupported records without interpreting authority.
use super::Target;
use crate::structured::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::HashSet;

#[derive(Clone, Debug)]
pub(crate) enum View {
    Ready(Target),
    Unavailable {
        saved: Saved,
        reason: UnavailableReason,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    InvalidRecord,
    UnknownKind,
    UnsupportedSchema,
    MissingAgent,
    DuplicateAgent,
}
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Saved {
    Current(Placement),
    Opaque(Box<RawValue>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    kind: String,
    schema: u32,
    agent_id: String,
}
impl<'de> Deserialize<'de> for Saved {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(d).map(Self::Opaque)
    }
}
impl Saved {
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
    pub fn target(&self) -> Option<&Target> {
        match self {
            Self::Ready(target) => Some(target),
            Self::Unavailable { .. } => None,
        }
    }
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
