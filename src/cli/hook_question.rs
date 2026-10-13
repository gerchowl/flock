#![expect(
    clippy::print_stdout,
    reason = "the hook's verdict is written to stdout as the harness's hook contract"
)]
//! `flk hook claude question`: the question point of ADR-0034 (§3, §4).
//!
//! Claude Code runs it as a `PreToolUse` hook on its question tools. It asks
//! the server for a verdict with `hook.decide` and prints it in Claude's own
//! form. When the server cannot decide (stopped, silent, too old for
//! `hook.decide`, or unable to attest the pane), the pane's `FLOCK_QUESTIONS`
//! hint picks the verdict instead (§4): a hinted pane is refused with a fixed
//! fallback sentence, because a dialog there draws in a pane nobody watches,
//! and an unhinted one passes, so a root's dialog draws as it always did.
//!
//! The installed stub repeats the hinted half of that table on its own for the
//! failures this process cannot report: a missing binary, a crash, bad output
//! or a hang (§4, "When the hook itself fails").

use std::time::Duration;

use crate::api::client::ApiClient;
use crate::api::schema::{
    HookDecideParams, HookHarness, HookPoint, HookVerdict, Method, Request, ResponseResult,
};

/// How long a hinted pane waits for `hook.decide`: the hold plus 5 s
/// (ADR-0034 §4). The hold is 0 until answers are held for (#1183). The stub's
/// watchdog (10 s) and the hook entry's `timeout` (30 s,
/// `CLAUDE_QUESTION_HOOK_TIMEOUT`) sit above it, so a slow server is reported
/// here, by this process, before either fires.
pub(crate) const QUESTION_DEADLINE: Duration = Duration::from_secs(5);

/// The fallback for `FLOCK_QUESTIONS=route` (ADR-0034 §4). The stub prints the
/// same sentence.
pub(crate) const ROUTE_FALLBACK: &str = "flk could not reach your architect. Do not ask the \
     human. Choose the most conservative option, state the assumption in your result, or end \
     your turn with a blocked report.";

/// The fallback for `FLOCK_QUESTIONS=ask_operator` (ADR-0034 §4).
pub(crate) const ASK_OPERATOR_FALLBACK: &str = "flk could not reach the operator's question \
     queue, and nobody is watching this pane. Choose the most conservative option, state the \
     assumption in your result, or end your turn with a blocked report.";

/// The verdict to print, with the reason a `deny` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    Pass,
    Deny(String),
}

/// Ask the server, print the verdict, exit 0. The hook never fails: every
/// failure is a verdict from [`fallback`].
pub(super) fn run(input: &serde_json::Value, seq: u64) -> i32 {
    let hint = std::env::var(crate::integration::FLOCK_QUESTIONS_ENV_VAR).ok();
    // Roots never wait on the question deadline (ADR-0034 §3): an unhinted
    // pane keeps every other hook's 500 ms.
    let deadline = if fallback(hint.as_deref()) == Verdict::Pass {
        super::hook::HOOK_TIMEOUT
    } else {
        QUESTION_DEADLINE
    };
    let field = |key| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let request = Request {
        id: format!("flock:hook:{seq}"),
        method: Method::HookDecide(HookDecideParams {
            point: HookPoint::Question,
            harness: HookHarness::Claude,
            session_id: field("session_id"),
            tool_name: field("tool_name"),
        }),
    };
    let response = ApiClient::local()
        .request_value_with_timeout(&request, deadline)
        .ok();
    println!(
        "{}",
        claude_output(&verdict(response.as_ref(), hint.as_deref()))
    );
    super::exit::SUCCESS
}

/// The server's verdict, or the §4 table's when it gave none: an error
/// (`no_caller_pane`, or an older server's unknown method) is "cannot decide"
/// exactly as no reply is.
pub(super) fn verdict(response: Option<&serde_json::Value>, hint: Option<&str>) -> Verdict {
    let decided = response
        .and_then(|response| response.get("result"))
        .and_then(|result| serde_json::from_value::<ResponseResult>(result.clone()).ok());
    match decided {
        Some(ResponseResult::HookDecide {
            verdict: HookVerdict::Pass,
            ..
        }) => Verdict::Pass,
        Some(ResponseResult::HookDecide {
            verdict: HookVerdict::Deny,
            reason,
        }) => {
            Verdict::Deny(reason.unwrap_or_else(|| crate::app::questions::DENY_REASON.to_string()))
        }
        _ => fallback(hint),
    }
}

/// ADR-0034 §4 when the server cannot decide: `ask_human` or no hint passes,
/// any other hint is refused with its fixed sentence.
pub(super) fn fallback(hint: Option<&str>) -> Verdict {
    match hint {
        None | Some("" | "ask_human") => Verdict::Pass,
        Some(_) => Verdict::Deny(fallback_reason(hint).to_string()),
    }
}

/// The fixed sentence for a hint. One that is not a policy names nobody, so it
/// gets the `deny` sentence, as in the stub.
fn fallback_reason(hint: Option<&str>) -> &'static str {
    match hint {
        Some("route") => ROUTE_FALLBACK,
        Some("ask_operator") => ASK_OPERATOR_FALLBACK,
        _ => crate::app::questions::DENY_REASON,
    }
}

/// Claude Code's `PreToolUse` output. `pass` is an empty object rather than
/// nothing, because the stub refuses a hinted pane's empty output as a failure.
pub(super) fn claude_output(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Pass => "{}".to_string(),
        Verdict::Deny(reason) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::questions::DENY_REASON;

    fn decided(verdict: HookVerdict, reason: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": "t",
            "result": serde_json::to_value(ResponseResult::HookDecide {
                verdict,
                reason: reason.map(str::to_string),
            })
            .expect("serialize"),
        })
    }

    /// ADR-0034 §4, first row: when the server answers, its verdict stands,
    /// whatever the hint says.
    #[test]
    fn the_servers_verdict_wins_over_the_hint() {
        for hint in [None, Some("ask_human"), Some("route"), Some("deny")] {
            assert_eq!(
                verdict(Some(&decided(HookVerdict::Pass, None)), hint),
                Verdict::Pass
            );
            assert_eq!(
                verdict(Some(&decided(HookVerdict::Deny, Some("why"))), hint),
                Verdict::Deny("why".into())
            );
        }
    }

    /// The other rows: no reply, an attestation refusal and an older server's
    /// unknown method are all "the server cannot decide". A hinted pane is
    /// refused with its own sentence, and a root passes.
    #[test]
    fn a_server_that_cannot_decide_falls_back_to_the_hint() {
        let no_pane = serde_json::json!({"id": "t", "error": {
            "code": "no_caller_pane", "message": "not a pane"}});
        let older = serde_json::json!({"id": "", "error": {"code": "invalid_request",
            "message": "invalid request: unknown variant `hook.decide`, expected one of ..."}});
        for response in [None, Some(&no_pane), Some(&older)] {
            assert_eq!(verdict(response, None), Verdict::Pass);
            assert_eq!(verdict(response, Some("")), Verdict::Pass);
            assert_eq!(verdict(response, Some("ask_human")), Verdict::Pass);
            assert_eq!(
                verdict(response, Some("route")),
                Verdict::Deny(ROUTE_FALLBACK.into())
            );
            assert_eq!(
                verdict(response, Some("ask_operator")),
                Verdict::Deny(ASK_OPERATOR_FALLBACK.into())
            );
            assert_eq!(
                verdict(response, Some("deny")),
                Verdict::Deny(DENY_REASON.into())
            );
            // A hint that is not a policy names nobody.
            assert_eq!(
                verdict(response, Some("bogus")),
                Verdict::Deny(DENY_REASON.into())
            );
        }
    }

    /// The stub prints the same fixed sentences when `flk` itself fails
    /// (ADR-0034 §4), so the two can never tell the agent different things.
    #[test]
    fn the_stub_carries_the_same_fallback_sentences() {
        let stub = include_str!("../integration/assets/claude/flock-agent-state.sh");
        for (hint, sentence) in [
            ("route", ROUTE_FALLBACK),
            ("ask_operator", ASK_OPERATOR_FALLBACK),
            ("*", DENY_REASON),
        ] {
            assert!(
                stub.contains(&format!("{hint}) reason=\"{sentence}\" ;;")),
                "{hint}: {sentence}"
            );
            // printf '%s' into a JSON string: nothing that needs escaping.
            assert!(!sentence.contains(['"', '\\', '%']), "{sentence}");
        }
    }

    #[test]
    fn claude_reads_a_deny_and_an_empty_object() {
        assert_eq!(claude_output(&Verdict::Pass), "{}");
        let out: serde_json::Value =
            serde_json::from_str(&claude_output(&Verdict::Deny("no".into()))).expect("json");
        assert_eq!(
            out,
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                "permissionDecision": "deny", "permissionDecisionReason": "no"}})
        );
    }

    /// The deadline ladder of ADR-0034 §4: `flk hook` gives up before the
    /// stub's watchdog kills it, and the watchdog fires before Claude's own
    /// timeout, so each failure is reported by the layer that knows most.
    #[test]
    fn the_deadlines_nest() {
        let stub = include_str!("../integration/assets/claude/flock-agent-state.sh");
        let watchdog = stub
            .lines()
            .find_map(|line| line.trim().strip_prefix("sleep ")?.strip_suffix(" &"))
            .and_then(|secs| secs.parse::<u64>().ok())
            .expect("the stub's watchdog");
        assert!(super::super::hook::HOOK_TIMEOUT < QUESTION_DEADLINE);
        assert!(QUESTION_DEADLINE.as_secs() < watchdog);
        assert!(watchdog < crate::integration::CLAUDE_QUESTION_HOOK_TIMEOUT);
    }
}
