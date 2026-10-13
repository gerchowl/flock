//! `--questions` and `--unattended` on `agent start`, `agent fork` and
//! `delegate start` (#1179, ADR-0034 §2).
//!
//! Both are sent only to a server that advertises `agent_questions`. An older
//! server would drop them without a word, so the start goes ahead without them
//! and says so on stderr (the #868 skew rule, as for the edge label).

use crate::api::schema::{Method, PingParams, QuestionPolicy, Request};

/// The `--questions` value, or the line to print.
pub(super) fn parse_policy(raw: Option<&String>) -> Result<QuestionPolicy, String> {
    let raw = raw.ok_or("missing value for --questions")?;
    QuestionPolicy::parse(raw).ok_or_else(|| {
        format!("invalid value for --questions: {raw} (ask_human|ask_operator|route|deny)")
    })
}

/// What a start asked for, after the skew gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Asked {
    pub(super) questions: Option<QuestionPolicy>,
    pub(super) unattended: bool,
}

impl Asked {
    fn is_empty(self) -> bool {
        self.questions.is_none() && !self.unattended
    }

    /// The fields this server may be sent. `advertised` is asked only when
    /// something was asked for, so a plain start costs no extra round trip.
    pub(super) fn gate(self, who: &str, advertised: impl FnOnce() -> bool) -> Self {
        if self.is_empty() || advertised() {
            return self;
        }
        eprintln!(
            "{who}: the server does not record a question policy (it predates #1179); \
             starting without --questions and --unattended"
        );
        Self::default()
    }
}

/// Does the local server advertise `agent_questions`? A ping that fails reads
/// as no, so the start goes ahead without the fields rather than failing late.
pub(super) fn server_takes_questions() -> bool {
    super::send_request(&Request {
        id: "cli:ping:questions".into(),
        method: Method::Ping(PingParams::default()),
    })
    .is_ok_and(|pong| advertises_questions(&pong))
}

pub(crate) fn advertises_questions(pong: &serde_json::Value) -> bool {
    pong.pointer("/result/capabilities/agent_questions")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_parses_from_its_wire_spelling_and_nothing_else_does() {
        for policy in [
            QuestionPolicy::AskHuman,
            QuestionPolicy::AskOperator,
            QuestionPolicy::Route,
            QuestionPolicy::Deny,
        ] {
            assert_eq!(parse_policy(Some(&policy.as_str().into())), Ok(policy));
        }
        assert!(parse_policy(Some(&"human".into())).is_err());
        assert!(parse_policy(None).is_err());
    }

    /// #868: the fields go only to a server that advertises them; an older
    /// one gets a start without them, and a plain start never asks.
    #[test]
    fn the_policy_is_sent_only_to_a_server_that_advertises_it() {
        let asked = Asked {
            questions: Some(QuestionPolicy::Deny),
            unattended: true,
        };
        assert_eq!(asked.gate("t", || true), asked);
        assert_eq!(asked.gate("t", || false), Asked::default());
        assert_eq!(
            Asked::default().gate("t", || panic!("a plain start must not ping")),
            Asked::default()
        );

        let current = serde_json::json!({"result": {"type": "pong", "capabilities": {
            "live_handoff": true, "agent_start_label": true, "agent_questions": true
        }}});
        let older = serde_json::json!({"result": {"type": "pong", "capabilities": {
            "live_handoff": true, "agent_start_label": true
        }}});
        assert!(advertises_questions(&current));
        assert!(!advertises_questions(&older));
    }
}
