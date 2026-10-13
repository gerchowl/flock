//! What a registered wait waits for (ADR-0028 §5, #1031), as a typed value
//! with one canonical text form.
//!
//! `wait.park` takes the predicate as text (`until`) and the server parses
//! it here, so a client never ships a predicate the server cannot evaluate.
//! `state.waiting` and `wait.expired` report the canonical form this module
//! renders, which parses back to the same value.
//!
//! Supported for a park:
//!
//! - `delegate:<agent id> phase in [<phase>, …]`, phases from the `state`
//!   vocabulary (ADR-0028 §1), deduplicated and rendered in table order;
//! - `reply:<correlation id>`.
//!
//! A held `events.read` registers `events after <seq> [kinds <k>,…]
//! [subjects <id>,…]`, its own filter. A park cannot take that form: the read
//! is how to wait on raw events.
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

impl fmt::Display for PredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) | Self::Unsupported(reason) => f.write_str(reason),
        }
    }
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

/// `[a, b]` or `a,b` into its items.
fn list(raw: &str) -> Vec<&str> {
    raw.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect()
}

impl WaitPredicate {
    /// Parse a predicate text. Whitespace between tokens is free.
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
                correlation_id: id(rest.trim(), "correlation id")?,
            });
        }
        if let Some(rest) = text.strip_prefix("delegate:") {
            return Self::parse_delegate(rest);
        }
        if let Some(rest) = text.strip_prefix("events ") {
            return Self::parse_events(rest);
        }
        Err(PredicateError::Invalid(format!(
            "{text:?} is not a wait predicate; expected `delegate:<id> phase in [...]` \
             or `reply:<correlation id>`"
        )))
    }

    fn parse_delegate(rest: &str) -> Result<Self, PredicateError> {
        let invalid = || {
            PredicateError::Invalid("expected `delegate:<id> phase in [<phase>, ...]`".to_owned())
        };
        let (delegate, rest) = rest.split_once(char::is_whitespace).ok_or_else(invalid)?;
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("phase").ok_or_else(invalid)?.trim_start();
        let rest = rest.strip_prefix("in").ok_or_else(invalid)?.trim();
        if !(rest.starts_with('[') && rest.ends_with(']')) {
            return Err(invalid());
        }
        let mut phases = list(rest)
            .into_iter()
            .map(|name| {
                serde_json::from_value::<Phase>(serde_json::Value::String(name.to_owned()))
                    .map_err(|_| PredicateError::Invalid(format!("{name:?} is not a phase")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if phases.is_empty() {
            return Err(PredicateError::Invalid(
                "a phase list cannot be empty".to_owned(),
            ));
        }
        phases.sort();
        phases.dedup();
        Ok(Self::DelegatePhase {
            delegate: id(delegate, "delegate")?,
            phases,
        })
    }

    fn parse_events(rest: &str) -> Result<Self, PredicateError> {
        let invalid = || {
            PredicateError::Invalid(
                "expected `events after <seq> [kinds <k>,...] [subjects <id>,...]`".to_owned(),
            )
        };
        let mut words = rest.split_whitespace();
        if words.next() != Some("after") {
            return Err(invalid());
        }
        let after = words
            .next()
            .and_then(|seq| seq.parse().ok())
            .ok_or_else(invalid)?;
        let (mut kinds, mut subjects) = (Vec::new(), Vec::new());
        while let Some(key) = words.next() {
            let items = list(words.next().ok_or_else(invalid)?);
            match key {
                "kinds" => {
                    for name in items {
                        kinds.push(
                            serde_json::from_value(serde_json::Value::String(name.to_owned()))
                                .map_err(|_| {
                                    PredicateError::Invalid(format!(
                                        "{name:?} is not an event kind"
                                    ))
                                })?,
                        );
                    }
                }
                "subjects" => {
                    for subject in items {
                        subjects.push(id(subject, "subject")?);
                    }
                }
                _ => return Err(invalid()),
            }
        }
        Ok(Self::Events {
            kinds,
            subjects,
            after,
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
        assert_eq!(
            canonical("events after 7 kinds message_queued,pr.merged subjects ws_1:p2"),
            "events after 7 kinds message_queued,pr.merged subjects ws_1:p2"
        );
        assert_eq!(canonical("events after 0"), "events after 0");
        for text in [
            "delegate:agent_d1 phase in [gone, needs_human, finished]",
            "reply:c_42",
            "events after 7 kinds message_queued subjects a,b",
        ] {
            assert_eq!(canonical(&canonical(text)), canonical(text), "{text}");
        }
    }

    #[test]
    fn the_adrs_other_predicates_are_unsupported_and_the_rest_invalid() {
        let err = |text: &str| WaitPredicate::parse(text).unwrap_err();
        assert!(matches!(
            err("pr:812 ci_completed"),
            PredicateError::Unsupported(_)
        ));
        assert!(matches!(
            err("any-delegate-of-mine changed"),
            PredicateError::Unsupported(_)
        ));
        for text in [
            "",
            "delegate:agent_d1",
            "delegate:agent_d1 phase in []",
            "delegate:agent_d1 phase in [stalled]",
            "delegate:agent_d1 phase in finished",
            "delegate: phase in [finished]",
            "reply:",
            "reply:a b",
            "events after x",
            "events after 1 kinds no_such_kind",
            "agent_d1 finished",
        ] {
            assert!(matches!(err(text), PredicateError::Invalid(_)), "{text:?}");
        }
    }
}
