//! What a registered wait waits for (ADR-0028 §5, #1031), as a typed value
//! with one canonical text form.
//!
//! `wait.park` takes the predicate as text (`until`) and the server parses
//! it here, so a client never ships a predicate the server cannot evaluate.
//! `state.waiting` and `wait.expired` report the canonical form this module
//! renders, which parses back to the same value.
//!
//! Supported for a park, matched strictly (tokens separated by whitespace,
//! one bracketed list, no empty items):
//!
//! - `delegate:<agent id> phase in [<phase>, …]`, phases from the `state`
//!   vocabulary (ADR-0028 §1) except `unknown`, which no agent settles in;
//!   deduplicated and rendered in table order;
//! - `reply:<correlation id>`.
//!
//! A held `events.read` registers its own filter as
//! `events after <seq> [kinds <k>,…] [subjects <id>,…]`. That form is
//! rendered, never parsed: a park cannot take it.
//!
//! The ADR's other predicates, `pr:<n> ci_completed` and
//! `any-delegate-of-mine changed`, are recognised and refused as unsupported
//! until #1250.

use std::fmt;

use crate::api::agent_state::Phase;
use crate::api::schema::EventKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitPredicate {
    DelegatePhase {
        delegate: String,
        phases: Vec<Phase>,
    },
    Reply {
        correlation_id: String,
    },
    Events {
        kinds: Vec<EventKind>,
        subjects: Vec<String>,
        after: u64,
    },
}

/// Why a predicate text was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredicateError {
    /// Not a predicate of the grammar at all.
    Invalid(String),
    /// A predicate the ADR names that this server cannot register yet.
    Unsupported(String),
}

fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// One id: non-empty, no whitespace, no list punctuation.
fn id(raw: &str, what: &str) -> Result<String, PredicateError> {
    if raw.is_empty() || raw.contains(|c: char| c.is_whitespace() || "[],".contains(c)) {
        return Err(PredicateError::Invalid(format!(
            "{what} {raw:?} is not an id"
        )));
    }
    Ok(raw.to_owned())
}

impl WaitPredicate {
    /// Parse a park's predicate text.
    pub fn parse(text: &str) -> Result<Self, PredicateError> {
        let text = text.trim();
        let unsupported = |name: &str| {
            PredicateError::Unsupported(format!(
                "`{name}` predicates are not supported yet (#1250); park on \
                 `delegate:<id> phase in [...]` or `reply:<correlation id>`"
            ))
        };
        if text.starts_with("pr:") {
            return Err(unsupported("pr:<n> ci_completed"));
        }
        if text.starts_with("any-delegate-of-mine") {
            return Err(unsupported("any-delegate-of-mine changed"));
        }
        if let Some(rest) = text.strip_prefix("reply:") {
            return Ok(Self::Reply {
                correlation_id: id(rest, "correlation id")?,
            });
        }
        if let Some(rest) = text.strip_prefix("delegate:") {
            return Self::parse_delegate(rest);
        }
        Err(PredicateError::Invalid(format!(
            "{text:?} is not a wait predicate; expected `delegate:<id> phase in [...]` \
             or `reply:<correlation id>`"
        )))
    }

    /// `<id> phase in [<phase>, …]`: three whitespace-separated tokens, then
    /// one list in one pair of brackets.
    fn parse_delegate(rest: &str) -> Result<Self, PredicateError> {
        let invalid = || {
            PredicateError::Invalid("expected `delegate:<id> phase in [<phase>, ...]`".to_owned())
        };
        // The next whitespace-separated token, and what follows it.
        let token = |text: &str| -> Option<(String, String)> {
            let (token, rest) = text.trim_start().split_once(char::is_whitespace)?;
            Some((token.to_owned(), rest.to_owned()))
        };
        let parsed = token(rest).and_then(|(delegate, rest)| {
            let (phase, rest) = token(&rest)?;
            let (in_, list) = token(&rest)?;
            (phase == "phase" && in_ == "in").then_some((delegate, list))
        });
        let Some((delegate, list)) = parsed else {
            return Err(invalid());
        };
        let inner = list
            .trim()
            .strip_prefix('[')
            .and_then(|list| list.strip_suffix(']'))
            .filter(|inner| !inner.contains(['[', ']']))
            .ok_or_else(invalid)?;
        let mut phases = inner
            .split(',')
            .map(|name| {
                let name = name.trim();
                serde_json::from_value::<Phase>(serde_json::Value::String(name.to_owned()))
                    .ok()
                    .filter(|phase| *phase != Phase::Unknown)
                    .ok_or_else(|| PredicateError::Invalid(format!("{name:?} is not a phase")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        phases.sort();
        phases.dedup();
        Ok(Self::DelegatePhase {
            delegate: id(&delegate, "delegate")?,
            phases,
        })
    }
}

impl fmt::Display for WaitPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DelegatePhase { delegate, phases } => {
                let phases: Vec<String> = phases.iter().map(wire_name).collect();
                write!(f, "delegate:{delegate} phase in [{}]", phases.join(", "))
            }
            Self::Reply { correlation_id } => write!(f, "reply:{correlation_id}"),
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

    fn canonical(text: &str) -> String {
        WaitPredicate::parse(text).unwrap().to_string()
    }

    #[test]
    fn supported_predicates_parse_to_one_canonical_form() {
        assert_eq!(
            canonical("delegate:agent_d1   phase in [gone,finished, needs_human, finished]"),
            "delegate:agent_d1 phase in [gone, needs_human, finished]"
        );
        assert_eq!(canonical(" reply:c_42 "), "reply:c_42");
        for text in [
            "delegate:agent_d1 phase in [gone, needs_human, finished]",
            "reply:c_42",
        ] {
            assert_eq!(canonical(&canonical(text)), canonical(text), "{text}");
        }
    }

    #[test]
    fn a_held_reads_filter_renders_in_canonical_form() {
        let events = WaitPredicate::Events {
            kinds: vec![EventKind::MessageQueued, EventKind::PrMerged],
            subjects: vec!["ws_1:p2".into(), "agent_b".into()],
            after: 7,
        };
        assert_eq!(
            events.to_string(),
            "events after 7 kinds message_queued,pr.merged subjects ws_1:p2,agent_b"
        );
    }

    #[test]
    fn the_adrs_other_predicates_are_unsupported() {
        for text in ["pr:812 ci_completed", "any-delegate-of-mine changed"] {
            assert!(
                matches!(
                    WaitPredicate::parse(text),
                    Err(PredicateError::Unsupported(_))
                ),
                "{text}"
            );
        }
    }

    /// Review nit (#1248): the grammar is matched strictly. Each line is a
    /// form a looser parser accepted or could accept.
    #[test]
    fn malformed_or_unknown_forms_are_invalid() {
        for text in [
            "",
            "agent_d1 finished",
            "events after 0",
            "delegate:agent_d1",
            "delegate:agent_d1 phase in []",
            "delegate:agent_d1 phase in [unknown]",
            "delegate:agent_d1 phase in [stalled]",
            "delegate:agent_d1 phase in finished",
            "delegate:agent_d1 phasein[finished]",
            "delegate:agent_d1 phase in[finished]",
            "delegate:agent_d1 phase in [[finished]]",
            "delegate:agent_d1 phase in [finished]]",
            "delegate:agent_d1 phase in [finished,,gone]",
            "delegate: phase in [finished]",
            "reply:",
            "reply:a b",
        ] {
            assert!(
                matches!(WaitPredicate::parse(text), Err(PredicateError::Invalid(_))),
                "{text:?}"
            );
        }
    }
}
