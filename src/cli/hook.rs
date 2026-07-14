#![expect(
    clippy::print_stdout,
    reason = "the stop nudge is written to stdout as the agent-facing hook contract"
)]
//! `flk hook <agent> <action>` — the single-source-of-truth agent hook body.
//!
//! The per-agent shim assets (claude `sh`+`python`, opencode `js`, pi/omp `ts`,
//! …) each reimplement the same wire protocol — parse the hook JSON, open the
//! flock socket, speak `pane.report_*`, and (for Claude's Stop) scrape the
//! transcript and emit a self-heal nudge. That logic belongs in the binary
//! exactly once. This module ports it to Rust behind the same lean, pre-init
//! CLI dispatch the `flk pane report-*` verbs already use (`ApiClient` over a
//! blocking `std` UnixStream — no tokio, no logging). See #158.
//!
//! Contract carried over verbatim from the shims: a hook must NEVER block or
//! fail the parent agent. Every socket error is swallowed; a malformed payload
//! degrades to a no-op. The only stdout this ever writes is Claude's
//! `decision:block` nudge on Stop.
//!
//! Testability: each action plans a [`HookOutcome`] (the `pane.report_*`
//! methods to send + any stdout) as a pure function of the parsed input. The
//! socket send / print happen only at the edge in [`emit`], so parity with the
//! shims is unit-tested without a running server.

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::api::client::ApiClient;
use crate::api::schema::{
    Method, PaneReportAgentSessionParams, PaneReportPromptParams, PaneReportRecapParams,
    PaneReportReplyParams, Request,
};

/// Read/write deadline on the report socket, matching the shims' 0.5s: a
/// server that accepts but stalls must never wedge the agent's turn. The
/// `connect` leg is not covered (std `UnixStream` has no connect timeout), but
/// the failure mode #158 cares about — no server listening — fails `connect`
/// immediately with ENOENT/ECONNREFUSED, so it stays fire-and-forget.
const HOOK_TIMEOUT: Duration = Duration::from_millis(500);

/// Harness-internal markers that arrive through the same prompt/reply pipe as
/// real content. Dropped at the source so they never reach flock's history.
const SYSTEM_REMINDER_PREFIXES: [&str; 8] = [
    "<task-notification>",
    "<system-reminder>",
    "<command-name>",
    "<command-message>",
    "<local-command-",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];

/// A supported agent and its wire identity (`source` / `agent` fields on every
/// `pane.report_*`). The dispatch key is the first CLI arg; adding an agent is
/// a new variant plus its lifecycle-event mapping — the protocol is untouched.
#[derive(Clone, Copy)]
enum Agent {
    Claude,
    Opencode,
}

impl Agent {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Claude => "flock:claude",
            Self::Opencode => "flock:opencode",
        }
    }

    fn agent(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    Session,
    Prompt,
    Stop,
}

impl Action {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "session" => Some(Self::Session),
            "prompt" => Some(Self::Prompt),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// The planned effect of a hook invocation: the reports to fire (in order) and
/// an optional stdout payload (Claude's Stop nudge). Pure — see module docs.
#[derive(Default)]
struct HookOutcome {
    reports: Vec<Method>,
    stdout: Option<String>,
}

pub(super) fn run_hook_command(args: &[String]) -> std::io::Result<i32> {
    let (Some(agent), Some(action)) = (args.first(), args.get(1)) else {
        eprintln!("usage: flk hook <agent> <session|prompt|stop>");
        return Ok(2);
    };

    // Guard on the pane-env contract, mirroring the shims. Outside a flock pane
    // (or a nested `claude -p` that inherited nothing) this is a clean no-op.
    if std::env::var("FLOCK_ENV").ok().as_deref() != Some("1") {
        return Ok(0);
    }
    let (Some(pane_id), Some(_socket)) = (
        env_nonempty("FLOCK_PANE_ID"),
        env_nonempty("FLOCK_SOCKET_PATH"),
    ) else {
        return Ok(0);
    };

    let (Some(agent), Some(action)) = (Agent::parse(agent), Action::parse(action)) else {
        return Ok(0);
    };

    let input = read_stdin_json();
    let hook_event_name = str_field(&input, "hook_event_name").unwrap_or_default();

    // SubagentStop is a completion event Claude can emit after the main turn
    // already stopped; never let it revive an idle pane.
    if hook_event_name == "SubagentStop" {
        return Ok(0);
    }

    let outcome = plan(agent, action, &input, &hook_event_name, &pane_id);
    emit(outcome);
    Ok(0)
}

/// Pure: map a parsed hook invocation to the reports + stdout it should
/// produce. No IO — the socket send / print live in [`emit`].
fn plan(
    agent: Agent,
    action: Action,
    input: &serde_json::Value,
    hook_event_name: &str,
    pane_id: &str,
) -> HookOutcome {
    match action {
        Action::Session => plan_session(agent, input, hook_event_name, pane_id),
        Action::Prompt => plan_prompt(agent, input, pane_id),
        // Only Claude carries a scrapable transcript + nudge protocol on Stop.
        Action::Stop => match agent {
            Agent::Claude => plan_stop(agent, input, pane_id),
            Agent::Opencode => HookOutcome::default(),
        },
    }
}

fn plan_session(
    agent: Agent,
    input: &serde_json::Value,
    hook_event_name: &str,
    pane_id: &str,
) -> HookOutcome {
    let Some(session_id) = str_field(input, "session_id") else {
        return HookOutcome::default();
    };
    // Only a genuine SessionStart may forward `source` (startup/resume/clear/
    // compact); other lifecycle actions can't spoof an identity change. Claude
    // is the only agent that reports it today.
    let session_start_source = (matches!(agent, Agent::Claude)
        && hook_event_name == "SessionStart")
        .then(|| str_field(input, "source"))
        .flatten();

    HookOutcome {
        reports: vec![Method::PaneReportAgentSession(
            PaneReportAgentSessionParams {
                pane_id: pane_id.to_string(),
                source: agent.source().to_string(),
                agent: agent.agent().to_string(),
                seq: Some(seq()),
                agent_session_id: Some(session_id),
                agent_session_path: None,
                session_start_source,
            },
        )],
        stdout: None,
    }
}

fn plan_prompt(agent: Agent, input: &serde_json::Value, pane_id: &str) -> HookOutcome {
    let Some(prompt) = str_field(input, "prompt").filter(|p| !p.trim().is_empty()) else {
        return HookOutcome::default();
    };
    if is_system_reminder(&prompt) {
        return HookOutcome::default();
    }
    HookOutcome {
        reports: vec![Method::PaneReportPrompt(PaneReportPromptParams {
            pane_id: pane_id.to_string(),
            source: agent.source().to_string(),
            agent: agent.agent().to_string(),
            prompt: cap(&prompt, 16384),
            seq: Some(seq()),
        })],
        stdout: None,
    }
}

fn plan_stop(agent: Agent, input: &serde_json::Value, pane_id: &str) -> HookOutcome {
    // Parity with the shim's `bool(hook_input.get("agent_id"))`: a non-empty
    // string agent_id marks a subagent. `str_field` already rejects empty
    // strings, so an `agent_id: ""` is (correctly) not treated as a subagent.
    let is_subagent = str_field(input, "agent_id").is_some();
    let last_assistant = str_field(input, "transcript_path")
        .and_then(|path| last_assistant_text(&path))
        .unwrap_or_default();

    let mut outcome = HookOutcome::default();

    if !last_assistant.is_empty() && !is_system_reminder(&last_assistant) {
        outcome
            .reports
            .push(Method::PaneReportReply(PaneReportReplyParams {
                pane_id: pane_id.to_string(),
                source: agent.source().to_string(),
                agent: agent.agent().to_string(),
                reply: cap(&last_assistant, 4096),
                seq: Some(seq()),
            }));
    }

    // Lift the `※ recap:` sentinel line verbatim if present.
    if let Some(recap) = last_assistant
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('※'))
    {
        outcome
            .reports
            .push(Method::PaneReportRecap(PaneReportRecapParams {
                pane_id: pane_id.to_string(),
                source: agent.source().to_string(),
                agent: agent.agent().to_string(),
                recap: cap(recap, 4096),
                seq: Some(seq()),
            }));
        return outcome;
    }

    // No sentinel: nudge for one more turn (self-heal, never user-facing).
    // Skip when we saw no assistant text or this is a subagent, so we don't
    // loop on nothing.
    if !last_assistant.is_empty() && !is_subagent {
        outcome.stdout = Some(
            serde_json::json!({
                "decision": "block",
                "reason": "End your turn with a single sentinel line: `※ recap: \
                           <current state>. Next: <one concrete step>.` Then stop.",
            })
            .to_string(),
        );
    }
    outcome
}

/// Apply a planned outcome: fire each report (swallowing errors) then print any
/// stdout. The only place this module touches the socket or stdout.
fn emit(outcome: HookOutcome) {
    for method in outcome.reports {
        let request = Request {
            id: format!("flock:hook:{}", seq()),
            method,
        };
        let _ = ApiClient::local().request_value_with_timeout(&request, HOOK_TIMEOUT);
    }
    if let Some(stdout) = outcome.stdout {
        println!("{stdout}");
    }
}

fn read_stdin_json() -> serde_json::Value {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    serde_json::from_str(raw.trim())
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

/// Walk a Claude JSONL transcript backwards for the last assistant message's
/// text. Transcript shapes vary by version: role is on the top-level object or
/// the nested `message` object; content is a string or a list of `text` blocks.
fn last_assistant_text(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let message = obj.get("message").filter(|m| m.is_object());
        let role = message
            .and_then(|m| str_field(m, "role"))
            .or_else(|| str_field(&obj, "role"))
            .or_else(|| str_field(&obj, "type"));
        if role.as_deref() != Some("assistant") {
            continue;
        }
        let content = message.unwrap_or(&obj).get("content");
        let text = match content {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string(),
            _ => String::new(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// Monotonic report sequence. Seeded from wall-clock nanos (so the server sees
/// a sensible ordering across separate hook invocations) but bumped by a
/// process-local counter so two reports emitted in the same invocation — the
/// Stop path's reply then recap — can never collide.
fn seq() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_add(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_system_reminder(text: &str) -> bool {
    let text = text.trim_start();
    SYSTEM_REMINDER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Char-bounded truncation (the shims' `[:N]` is a codepoint slice, not bytes).
fn cap(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn method_name(method: &Method) -> &'static str {
        match method {
            Method::PaneReportAgentSession(_) => "report_agent_session",
            Method::PaneReportPrompt(_) => "report_prompt",
            Method::PaneReportReply(_) => "report_reply",
            Method::PaneReportRecap(_) => "report_recap",
            _ => "other",
        }
    }

    // --- session ---------------------------------------------------------

    #[test]
    fn session_reports_id_and_forwards_source_only_on_sessionstart() {
        let input =
            json!({"hook_event_name": "SessionStart", "session_id": "sid-1", "source": "resume"});
        let out = plan(
            Agent::Claude,
            Action::Session,
            &input,
            "SessionStart",
            "p_1",
        );
        assert_eq!(out.reports.len(), 1);
        let Method::PaneReportAgentSession(params) = &out.reports[0] else {
            panic!("expected report_agent_session");
        };
        assert_eq!(params.source, "flock:claude");
        assert_eq!(params.agent, "claude");
        assert_eq!(params.agent_session_id.as_deref(), Some("sid-1"));
        assert_eq!(params.session_start_source.as_deref(), Some("resume"));
    }

    #[test]
    fn session_without_id_is_a_noop() {
        let input = json!({"hook_event_name": "SessionStart"});
        let out = plan(
            Agent::Claude,
            Action::Session,
            &input,
            "SessionStart",
            "p_1",
        );
        assert!(out.reports.is_empty());
    }

    #[test]
    fn session_source_suppressed_when_not_sessionstart() {
        // A non-SessionStart lifecycle action can't spoof an identity change.
        let input =
            json!({"hook_event_name": "SessionEnd", "session_id": "sid", "source": "resume"});
        let out = plan(Agent::Claude, Action::Session, &input, "SessionEnd", "p_1");
        let Method::PaneReportAgentSession(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.session_start_source, None);
    }

    #[test]
    fn opencode_session_maps_to_report_with_opencode_identity() {
        let input = json!({"session_id": "os-1"});
        let out = plan(Agent::Opencode, Action::Session, &input, "", "p_1");
        let Method::PaneReportAgentSession(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.source, "flock:opencode");
        assert_eq!(params.agent, "opencode");
        assert_eq!(params.agent_session_id.as_deref(), Some("os-1"));
    }

    // --- prompt ----------------------------------------------------------

    #[test]
    fn prompt_reports_text() {
        let input = json!({"prompt": "fix the bug"});
        let out = plan(Agent::Claude, Action::Prompt, &input, "", "p_1");
        let Method::PaneReportPrompt(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.prompt, "fix the bug");
    }

    #[test]
    fn prompt_drops_system_reminders_and_blanks() {
        for p in ["  <task-notification>done", "<system-reminder>x", "   "] {
            let out = plan(
                Agent::Claude,
                Action::Prompt,
                &json!({ "prompt": p }),
                "",
                "p_1",
            );
            assert!(out.reports.is_empty(), "expected no report for {p:?}");
        }
    }

    // --- stop ------------------------------------------------------------

    fn transcript_with(lines: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flk-hook-{}", seq()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn stop_with_sentinel_reports_reply_then_recap_no_nudge() {
        let path = transcript_with(&[
            r#"{"role":"user","content":"hi"}"#,
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"Did it.\n※ recap: done. Next: ship."}]}}"#,
        ]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1");
        let names: Vec<_> = out.reports.iter().map(method_name).collect();
        assert_eq!(names, ["report_reply", "report_recap"]);
        assert!(out.stdout.is_none(), "sentinel present ⇒ no nudge");
        let Method::PaneReportRecap(recap) = &out.reports[1] else {
            panic!()
        };
        assert_eq!(recap.recap, "※ recap: done. Next: ship.");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stop_without_sentinel_nudges() {
        let path = transcript_with(&[
            r#"{"type":"assistant","content":"Just did the work, no sentinel."}"#,
        ]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1");
        assert_eq!(
            out.reports.iter().map(method_name).collect::<Vec<_>>(),
            ["report_reply"]
        );
        let nudge = out.stdout.expect("expected a nudge");
        assert!(nudge.contains("\"decision\":\"block\""));
        assert!(nudge.contains("※ recap:"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stop_subagent_does_not_nudge() {
        let path =
            transcript_with(&[r#"{"type":"assistant","content":"subagent output, no sentinel"}"#]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap(), "agent_id": "sub-1"});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1");
        assert!(out.stdout.is_none(), "subagent must not loop on a nudge");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn emit_against_a_dead_socket_is_fire_and_forget() {
        // The socket edge must swallow a failed send: pointing at a socket path
        // with no listener, emit() must return promptly, never panic or hang.
        // (nextest runs each test in its own process, so the env set is safe.)
        std::env::set_var(
            "FLOCK_SOCKET_PATH",
            "/nonexistent/flk-hook-fire-and-forget.sock",
        );
        let outcome = plan(
            Agent::Claude,
            Action::Session,
            &json!({"session_id": "s"}),
            "SessionStart",
            "p_1",
        );
        assert_eq!(outcome.reports.len(), 1, "precondition: one report to send");
        emit(outcome); // must not panic or block the caller
        std::env::remove_var("FLOCK_SOCKET_PATH");
    }

    #[test]
    fn seq_is_strictly_increasing_within_a_process() {
        // reply then recap in one Stop invocation must not collide.
        assert!(seq() < seq());
    }

    #[test]
    fn stop_empty_transcript_is_a_noop() {
        let input = json!({"hook_event_name": "Stop", "transcript_path": "/no/such/path"});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1");
        assert!(out.reports.is_empty() && out.stdout.is_none());
    }

    // --- helpers ---------------------------------------------------------

    #[test]
    fn system_reminder_prefixes_are_dropped() {
        assert!(is_system_reminder("  <task-notification>done"));
        assert!(!is_system_reminder("<div>jsx is fine</div>"));
    }

    #[test]
    fn cap_counts_codepoints_not_bytes() {
        assert_eq!(cap("※※※", 2), "※※");
        assert_eq!(cap("abc", 10), "abc");
    }

    #[test]
    fn last_assistant_prefers_last_text_block() {
        let path = transcript_with(&[
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"assistant","content":"※ recap: done. Next: ship."}"#,
        ]);
        assert_eq!(
            last_assistant_text(path.to_str().unwrap()).unwrap(),
            "※ recap: done. Next: ship."
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
