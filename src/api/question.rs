//! The structured open question on `needs_human{question}` (ADR-0028 §8a,
//! #1038).
//!
//! A harness's choice dialog travels as data, not as screen text. This build
//! reads it from the screen (Claude Code's `AskUserQuestion` form, Codex's
//! `request_user_input`). ADR-0034 §9 makes the harness hook the primary
//! source (Q16, #1194), with this screen reading as the fallback, so the
//! field names follow the hook path's: `prompt`, `header`, `options` of
//! `{label, description}`, `multi_select`, `allow_other`.
//!
//! `question_id` is stable for one dialog instance (§8a). It is derived from
//! the agent, the turn cursor and the event-log sequence at the moment the
//! dialog opened, and a fingerprint of what it asks, with whitespace collapsed
//! so a resize that rewraps the prompt is the same question. A redraw (the
//! cursor moving, a checkbox toggled, a rewrap) keeps it, and every new open
//! gets an id never used before. Screen position never enters it. The open question is logged with `agent.question_opened`, and a server
//! that starts (a restart or a live handoff) takes it back from the log, so a
//! dialog still on screen keeps its id.
//!
//! Known limits of the screen source. It sees the one question a form shows:
//! a form with several questions is one question per tab, and moving to the
//! next tab closes the previous one. Moving back opens the earlier one again
//! under a new id. A header is
//! read only when the form has a single question tab, since the current tab is
//! marked by styling alone. `default` and `timeout_ms` are never set by a
//! screen: they belong to message-sourced questions (#1242).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One open question, as `needs_human{question}` and `agent.question_opened`
/// carry it: the form it asks, flattened, under its identity. Fields read
/// leniently (ADR-0028 §3a).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct Question {
    pub question_id: String,
    #[serde(default, deserialize_with = "crate::api::agent_state::or_unknown")]
    pub source: QuestionSource,
    /// The harness that asked: `claude`, `codex`.
    pub harness: String,
    #[serde(default, deserialize_with = "crate::api::agent_state::or_unknown")]
    pub state: QuestionState,
    #[serde(flatten)]
    pub form: QuestionForm,
}

/// Where a question is in its life (§8a): `open` while it waits, `withdrawn`
/// once it left the screen unanswered, `expired` for a message question past
/// its deadline (#1242). `answered { by, option_ids | text, at }` comes with
/// `question.answer` (#1039) and reads `unknown` in this build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum QuestionState {
    Open,
    Withdrawn,
    Expired,
    #[default]
    Unknown,
}

/// What a question asks, before it has an id: what a screen reads, and what
/// a harness hook's `tool_input` maps onto (ADR-0034, #1194).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct QuestionForm {
    /// The harness's short tag for the question (`AskUserQuestion`'s
    /// `header`), when the source can say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// The question text, wrapped lines joined with one space.
    pub prompt: String,
    /// The choices in the order drawn. A harness's own free-text entry
    /// ("Type something.", "None of the above") is `allow_other`, not an
    /// option, and so is Claude's "Chat about this".
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    /// Free text is accepted in place of an option.
    #[serde(default)]
    pub allow_other: bool,
}

/// One choice. `id` is the number the dialog draws beside it, which is also
/// the key that selects it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(optional_fields))]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum QuestionSource {
    Screen,
    Message,
    #[default]
    Unknown,
}

/// Why `agent.question_closed` closed a question (ADR-0028 §8a).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum QuestionCloseReason {
    /// The dialog left the screen without a `question.answer`. An answer
    /// typed at the pane closes it this way too: on screen it cannot be told
    /// from a cancel. The hook source reports those answers (#1194).
    Withdrawn,
    /// A message question's deadline passed (#1242). Never set for a dialog.
    Expired,
    #[default]
    Unknown,
}

impl QuestionForm {
    /// The question this form is: `open` itself when it asks the same, so a
    /// redraw keeps its id, otherwise a new question whose id derives from
    /// `agent_id`, the `turn_cursor` and event-log `log_sequence` it opened
    /// at, and the form's fingerprint. The log sequence is durable and only
    /// grows, and every close is logged, so no id that was ever closed is
    /// derived again: not after a restart, not for a tab revisited.
    pub fn carry(
        self,
        open: Option<&Question>,
        agent_id: &str,
        harness: &str,
        turn_cursor: &str,
        log_sequence: u64,
    ) -> Question {
        let fingerprint = self.fingerprint();
        if let Some(open) =
            open.filter(|open| open.harness == harness && open.form.fingerprint() == fingerprint)
        {
            return open.clone();
        }
        let digest = Sha256::new()
            .chain_update(format!(
                "{agent_id}\0{harness}\0{turn_cursor}\0{log_sequence}\0"
            ))
            .chain_update(fingerprint)
            .finalize();
        let hex: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Question {
            question_id: format!("q_{hex}"),
            source: QuestionSource::Screen,
            harness: harness.to_string(),
            state: QuestionState::Open,
            form: self,
        }
    }

    /// What the form asks with every whitespace run collapsed to one space,
    /// so a rewrap at another width reads the same.
    fn fingerprint(&self) -> Vec<u8> {
        let fold = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let options: Vec<_> = self
            .options
            .iter()
            .map(|option| {
                (
                    &option.id,
                    fold(&option.label),
                    option.description.as_deref().map(fold),
                )
            })
            .collect();
        let header = self.header.as_deref().map(fold);
        let asks = (
            header,
            fold(&self.prompt),
            options,
            self.multi_select,
            self.allow_other,
        );
        serde_json::to_vec(&asks).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                "../../tests/fixtures/questions-1038/",
                $name,
                ".txt"
            ))
        };
    }

    fn read(agent: Agent, screen: &str) -> Option<QuestionForm> {
        let detection = crate::detect::detect_agent(Some(agent), screen);
        assert_eq!(
            detection.state,
            crate::detect::AgentState::Blocked,
            "{screen}"
        );
        detection.question
    }

    fn form(value: serde_json::Value) -> QuestionForm {
        serde_json::from_value(value).unwrap()
    }

    /// Each captured screen (tests/fixtures/questions-1038/README.md) reads
    /// as the documented payload: single and multi-select, Other, a wrapped
    /// prompt and description, and Codex's form.
    #[test]
    fn captured_question_forms_read_as_their_documented_payload() {
        let colour = form(serde_json::json!({
            "header": "Colour",
            "prompt": "Which colour should the probe use?",
            "options": [
                {"id": "1", "label": "Red", "description": "A red probe"},
                {"id": "2", "label": "Blue", "description": "A blue probe"}
            ],
            "multi_select": false,
            "allow_other": true
        }));
        assert_eq!(
            read(Agent::Claude, fixture!("claude-single")),
            Some(colour.clone())
        );
        // The same question drawn by 2.1.296 (#1023's capture).
        let older = include_str!("../../tests/fixtures/attention-1023/claude-question.txt");
        assert_eq!(read(Agent::Claude, older), Some(colour));
        let checks = form(serde_json::json!({
            "header": "Checks",
            "prompt": "Which checks should the probe run?",
            "options": [
                {"id": "1", "label": "Lint", "description": "Run clippy with warnings denied"},
                {"id": "2", "label": "Tests", "description": "Run the nextest suite"},
                {"id": "3", "label": "Format", "description": "Check rustfmt"}
            ],
            "multi_select": true,
            "allow_other": true
        }));
        assert_eq!(read(Agent::Claude, fixture!("claude-multi")), Some(checks));
        // Two question tabs: the current one is marked by styling alone.
        let first = read(Agent::Claude, fixture!("claude-two-q1")).unwrap();
        assert_eq!(
            (first.header.as_deref(), first.prompt.as_str()),
            (None, "Which colour should the probe use?")
        );
        let size = form(serde_json::json!({
            "prompt": "Which size should the probe have? Pick the one that fits the narrow pane best, considering the wrapping of long text in a small terminal",
            "options": [
                {"id": "1", "label": "Small", "description": "A small probe that fits in any pane, even a very narrow one with long descriptions that wrap"},
                {"id": "2", "label": "Large", "description": "A large probe"}
            ],
            "multi_select": false,
            "allow_other": true
        }));
        assert_eq!(read(Agent::Claude, fixture!("claude-two-q2")), Some(size));
        assert_eq!(read(Agent::Claude, fixture!("claude-two-review")), None);
        let codex = form(serde_json::json!({
            "prompt": "Which colour should the probe use?",
            "options": [
                {"id": "1", "label": "Red (Recommended)", "description": "A red probe"},
                {"id": "2", "label": "Blue", "description": "A blue probe"}
            ],
            "multi_select": false,
            "allow_other": true
        }));
        let codex_screen = include_str!("../../tests/fixtures/attention-1023/codex-question.txt");
        assert_eq!(read(Agent::Codex, codex_screen), Some(codex));
    }

    /// The forms no question was read from yet: the permission prompt and
    /// the plan exit dialog are not questions.
    #[test]
    fn a_dialog_that_is_not_a_question_carries_none() {
        for screen in [
            include_str!("../../tests/fixtures/attention-1023/claude-permission.txt"),
            include_str!("../../tests/fixtures/attention-1023/claude-plan-exit.txt"),
        ] {
            assert_eq!(read(Agent::Claude, screen), None, "{screen}");
        }
    }

    /// A redraw (the cursor moving, a checkbox toggled, Other focused, a
    /// resize that rewraps the prompt) keeps the `question_id`, and so does a
    /// question screen whose form did not read. Every new open gets an id
    /// never used before: another dialog, a tab revisited, the same question
    /// in a later turn. `log` stands in for the event log, whose sequence the
    /// opened and closed events move.
    #[test]
    fn a_redraw_keeps_the_question_id_and_a_new_dialog_changes_it() {
        use crate::api::agent_state::HumanReason;
        use crate::detect::AgentState;
        let mut terminal = crate::terminal::TerminalState::new(
            crate::terminal::TerminalId::alloc(),
            std::env::temp_dir(),
        );
        let log = std::cell::Cell::new(1);
        let mut seen = std::collections::HashSet::new();
        let mut show = |terminal: &mut crate::terminal::TerminalState, screen: &str| {
            let detection = crate::detect::detect_agent(Some(Agent::Claude), screen);
            let before = terminal.question.as_ref().map(|q| q.question_id.clone());
            terminal.observe_question(
                Some(Agent::Claude),
                detection.needs_human,
                detection.question,
                log.get(),
            );
            let id = terminal.question.as_ref().map(|q| q.question_id.clone());
            if id != before {
                // An open, and the close before it, are logged.
                log.set(log.get() + 2);
                assert!(seen.insert(id.clone().unwrap()), "{id:?} repeated");
            }
            id
        };
        let single = show(&mut terminal, fixture!("claude-single")).unwrap();
        assert!(single.starts_with("q_") && single.len() == 18, "{single}");
        let redrawn = [
            fixture!("claude-single-redraw"),
            fixture!("claude-single-other-focused"),
        ];
        for screen in redrawn {
            assert_eq!(show(&mut terminal, screen), Some(single.clone()));
        }
        let multi = show(&mut terminal, fixture!("claude-multi")).unwrap();
        assert_eq!(
            show(&mut terminal, fixture!("claude-multi-toggled")),
            Some(multi)
        );
        let q1 = show(&mut terminal, fixture!("claude-two-q1")).unwrap();
        let q2 = show(&mut terminal, fixture!("claude-two-q2")).unwrap();
        // A narrower pane wraps the prompt and the description elsewhere.
        let rewrapped = fixture!("claude-two-q2")
            .replace(
                "the narrow pane best,\n\u{2502} considering",
                "the\n\u{2502} narrow pane best, considering",
            )
            .replace(
                "one with long\n     descriptions",
                "one\n     with long descriptions",
            );
        assert_ne!(rewrapped, fixture!("claude-two-q2"));
        assert_eq!(show(&mut terminal, &rewrapped), Some(q2.clone()));
        // The review page is a question screen whose form does not read.
        assert_eq!(
            show(&mut terminal, fixture!("claude-two-review")),
            Some(q2.clone())
        );
        let revisited = show(&mut terminal, fixture!("claude-two-q1")).unwrap();
        assert_ne!(revisited, q1, "a tab revisited is a new open");
        terminal.observe_question(Some(Agent::Claude), None, None, log.get());
        assert_eq!(
            terminal.question, None,
            "a screen that asks nothing closes it"
        );
        terminal.observe_question(
            Some(Agent::Claude),
            Some(HumanReason::Question),
            None,
            log.get(),
        );
        assert_eq!(terminal.question, None);
        // The agent works and asks the same question again.
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Working);
        terminal.set_detected_state(Some(Agent::Claude), AgentState::Blocked);
        let again = show(&mut terminal, fixture!("claude-single")).unwrap();
        assert_ne!(again, single, "asked again in a later turn");
    }

    /// A whitespace run inside a line is kept by the parser, which joins
    /// wrapped lines with one space, so the same prompt rewrapped at another
    /// width reads differently where a run falls on a line break. The
    /// fingerprint folds it, so the rewrap keeps the id.
    #[test]
    fn a_rewrap_across_a_double_space_keeps_the_question_id() {
        let wide = fixture!("claude-single").replace(
            "Which colour should the probe use?",
            "Which colour should  the probe use?",
        );
        let narrow = fixture!("claude-single").replace(
            "Which colour should the probe use?",
            "Which colour should\nthe probe use?",
        );
        let wide = read(Agent::Claude, &wide).unwrap();
        let narrow = read(Agent::Claude, &narrow).unwrap();
        assert_ne!(wide.prompt, narrow.prompt, "the parser keeps the run");
        let open = wide.carry(None, "agent_a", "claude", "t1:0:0:1:i", 1);
        let redrawn = narrow.carry(Some(&open), "agent_a", "claude", "t1:0:0:2:i", 9);
        assert_eq!(redrawn.question_id, open.question_id);
    }

    /// The wire shape on `needs_human`: the form flattened beside the id,
    /// and a field this build does not know is ignored (ADR-0028 §3a).
    #[test]
    fn the_question_rides_on_needs_human_and_reads_leniently() {
        use crate::api::agent_state::{Attention, HumanReason};
        let question = read(Agent::Claude, fixture!("claude-single"))
            .unwrap()
            .carry(None, "agent_a", "claude", "t1:0:0:1:i", 1);
        let attention = Attention::NeedsHuman {
            reason: HumanReason::Question,
            question: Some(Box::new(question.clone())),
        };
        let wire = serde_json::to_value(&attention).unwrap();
        let payload = &wire["needs_human"]["question"];
        assert_eq!(payload["question_id"], question.question_id.as_str());
        assert_eq!(payload["source"], "screen");
        assert_eq!(payload["state"], "open");
        assert_eq!(payload["harness"], "claude");
        assert_eq!(payload["prompt"], "Which colour should the probe use?");
        assert_eq!(payload["options"][1]["label"], "Blue");
        let mut newer = wire.clone();
        newer["needs_human"]["question"]["source"] = "hook".into();
        newer["needs_human"]["question"]["timeout_ms"] = 5.into();
        let back: Attention = serde_json::from_value(newer).unwrap();
        let Attention::NeedsHuman {
            question: Some(back),
            ..
        } = back
        else {
            panic!("{back:?}");
        };
        assert_eq!(
            (back.source, back.form),
            (QuestionSource::Unknown, question.form)
        );
        let old: Attention =
            serde_json::from_value(serde_json::json!({"needs_human": {"reason": "question"}}))
                .unwrap();
        assert!(matches!(old, Attention::NeedsHuman { question: None, .. }));
    }
}
