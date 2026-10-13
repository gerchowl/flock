//! What a registered wait waits for (ADR-0028 §5, #1031), as a typed value
//! with one canonical text form.
//!
//! `state.waiting` and `wait.expired` report the canonical form this module
//! renders. A held `events.read` registers its own filter as
//! `events after <seq> [kinds <k>,…] [subjects <id>,…]`.

use std::fmt;

use crate::api::schema::EventKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitPredicate {
    Events {
        kinds: Vec<EventKind>,
        subjects: Vec<String>,
        after: u64,
    },
}

fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl fmt::Display for WaitPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Events {
                kinds,
                subjects,
                after,
            } => {
                write!(f, "events after {after}")?;
                if !kinds.is_empty() {
                    let kinds: Vec<String> = kinds.iter().map(wire_name).collect();
                    write!(f, " kinds {}", kinds.join(","))?;
                }
                if !subjects.is_empty() {
                    write!(f, " subjects {}", subjects.join(","))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_reads_filter_renders_in_canonical_form() {
        let events = |kinds: Vec<EventKind>, subjects: Vec<&str>| WaitPredicate::Events {
            kinds,
            subjects: subjects.into_iter().map(str::to_owned).collect(),
            after: 7,
        };
        assert_eq!(events(Vec::new(), Vec::new()).to_string(), "events after 7");
        assert_eq!(
            events(
                vec![EventKind::MessageQueued, EventKind::PrMerged],
                vec!["ws_1:p2", "agent_b"]
            )
            .to_string(),
            "events after 7 kinds message_queued,pr.merged subjects ws_1:p2,agent_b"
        );
    }
}
