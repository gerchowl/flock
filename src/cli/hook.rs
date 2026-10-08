#![expect(
    clippy::print_stdout,
    reason = "the stop nudge is written to stdout as the agent-facing hook contract"
)]
//! `flk hook <agent> <action>` — the single-source-of-truth agent hook body.
//!
//! The per-agent shim assets (claude/copilot `sh`, opencode `js`, pi/omp `ts`,
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
    Method, PaneAgentState, PaneReleaseAgentParams, PaneReportAgentParams,
    PaneReportAgentSessionParams, PaneReportPromptParams, PaneReportRecapParams,
    PaneReportReplyParams, Request,
};

/// Read/write deadline on the report socket, matching the shims' 0.5s: a
/// server that accepts but stalls must never wedge the agent's turn. The
/// `connect` leg is not covered (std `UnixStream` has no connect timeout), but
/// the failure mode #158 cares about — no server listening — fails `connect`
/// immediately with ENOENT/ECONNREFUSED, so it stays fire-and-forget.
const HOOK_TIMEOUT: Duration = Duration::from_millis(500);

/// The line an agent ends its turn with; the Stop hook lifts it verbatim.
const RECAP_SENTINEL: &str = "※ recap:";

/// Floor on the Stop hook's transcript re-read interval: `[session]
/// stop_transcript_poll_ms = 0` must not turn the wait into a busy spin.
const MIN_TRANSCRIPT_POLL: Duration = Duration::from_millis(1);

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
    Codex,
    Kimi,
    Qodercli,
    Copilot,
}

impl Agent {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            "kimi" => Some(Self::Kimi),
            "qodercli" => Some(Self::Qodercli),
            "copilot" => Some(Self::Copilot),
            _ => None,
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Claude => "flock:claude",
            Self::Opencode => "flock:opencode",
            Self::Codex => "flock:codex",
            Self::Kimi => "flock:kimi",
            Self::Qodercli => "flock:qodercli",
            Self::Copilot => "flock:copilot",
        }
    }

    fn agent(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::Qodercli => "qodercli",
            Self::Copilot => "copilot",
        }
    }

    /// Whether this agent's host speaks the STATE vocabulary
    /// (`working`/`idle`/`blocked`/`release`) rather than the lifecycle one.
    ///
    /// This is a per-agent capability, not a global one. Claude derives its own
    /// state from lifecycle events and must ignore a state action outright —
    /// `claude_hook_ignores_state_actions` in `tests/cli_wrapper.rs` pins it,
    /// because honouring one would let a Notification hook (or a SUBAGENT's)
    /// drive the parent pane's state directly, which is the bug the claude
    /// integration is built to avoid.
    fn reports_state(self) -> bool {
        matches!(self, Self::Kimi | Self::Qodercli)
    }
}

#[derive(Clone, Copy)]
enum Action {
    /// Copilot supplies the lifecycle event in its JSON payload.
    Event,
    Session,
    Prompt,
    Stop,
    /// Direct state reports (#238). Agents whose host emits a state rather than
    /// a lifecycle event — kimi, qodercli — call these instead of
    /// session/prompt/stop. The state is the argument, not something inferred
    /// from the payload; the payload is still read for the session id and the
    /// subagent guard, when the host sends one at all (qoder does, kimi does
    /// not).
    State(PaneAgentState),
    Release,
}

impl Action {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "event" => Some(Self::Event),
            "session" => Some(Self::Session),
            "prompt" => Some(Self::Prompt),
            "stop" => Some(Self::Stop),
            "working" => Some(Self::State(PaneAgentState::Working)),
            "idle" => Some(Self::State(PaneAgentState::Idle)),
            "blocked" => Some(Self::State(PaneAgentState::Blocked)),
            "release" => Some(Self::Release),
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
    /// `(session_id, config_dir)` to persist so a post-restart resume re-selects
    /// the same Claude account profile. See `agent_resume::record_claude_config_dir`.
    config_dir_record: Option<(String, String)>,
}

pub(super) fn run_hook_command(args: &[String]) -> std::io::Result<i32> {
    // #455: `hook` is the one dispatched command with no help answer of its
    // own — `flk hook --help` used to exit 2 — and the predicate is
    // `cli::help`'s so it cannot become a second rule. Ahead of the pane-env
    // guard on purpose: asking what a hook is must never wake anything.
    if super::help::asks_for_help(args) {
        // stdout, like `flk --help`: the request was honoured, so this is the
        // command's output and can be redirected or paged.
        println!(
            "usage: flk hook <agent> <event|session|prompt|stop|working|idle|blocked|release>"
        );
        return Ok(0);
    }

    let (Some(agent), Some(action)) = (args.first(), args.get(1)) else {
        eprintln!(
            "usage: flk hook <agent> <event|session|prompt|stop|working|idle|blocked|release>"
        );
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

    // ADR-0008: the inbox is pull, so an idle agent would never learn mail
    // arrived. The stop hook is the wake — a peek (never a consume; the agent
    // reads its own inbox) whose count rides the existing decision:block
    // contract. Fire-and-forget like every other hook query: a server that
    // does not answer means no nudge, never a blocked turn.
    //
    // The count comes from `msg.wake`, not `msg.list`: the server decides
    // whether a wake may fire at all (a paused fleet, a muted recipient) and
    // answers with a number rather than message previews (#316).
    let pending_messages = if matches!(action, Action::Stop) {
        wake_message_count(&pane_id)
    } else {
        0
    };
    // #415: an older harness hands the hook no `last_assistant_message`, so
    // `plan_stop` has only the transcript — and the turn's final reply may not
    // have landed there yet. Give it a bounded window here, at the IO edge,
    // so the plan below reads what the turn actually ended with. The config
    // is loaded only on that path: a current harness never pays for it.
    if matches!((agent, action), (Agent::Claude, Action::Stop))
        && harness_final_text(&input).is_none()
    {
        if let Some(path) = str_field(&input, "transcript_path") {
            let session = crate::config::Config::load().config.session;
            await_settled_transcript(
                &path,
                Duration::from_millis(session.stop_transcript_wait_ms),
                Duration::from_millis(session.stop_transcript_poll_ms),
            );
        }
    }
    // The account profile the session is running under (g-fleet's claude-auth):
    // read here, at the IO edge, so `plan` stays a pure function of its inputs.
    let config_dir = env_nonempty("CLAUDE_CONFIG_DIR");
    let outcome = plan(
        agent,
        action,
        &input,
        &hook_event_name,
        &pane_id,
        pending_messages,
        config_dir.as_deref(),
    );
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
    pending_messages: usize,
    config_dir: Option<&str>,
) -> HookOutcome {
    match action {
        Action::Event if matches!(agent, Agent::Copilot) => plan_copilot(input, pane_id),
        Action::Event => HookOutcome::default(),
        Action::Session => plan_session(agent, input, hook_event_name, pane_id, config_dir),
        Action::Prompt => plan_prompt(agent, input, pane_id),
        // Only Claude carries a scrapable transcript + nudge protocol on Stop.
        Action::Stop => match agent {
            Agent::Claude => plan_stop(agent, input, pane_id, pending_messages),
            _ => HookOutcome::default(),
        },
        // Only the state-reporting hosts may drive state directly; for every
        // other agent a state action is not part of its contract and is
        // ignored, exactly as an unknown action was before it existed.
        Action::State(_) | Action::Release if !agent.reports_state() => HookOutcome::default(),
        Action::State(state) => plan_state(agent, state, input, pane_id),
        Action::Release => plan_release(agent, input, pane_id),
    }
}

/// Copilot's event vocabulary and inference order match its installed hook.
/// Explicit event names win over payload clues, including unknown events.
fn plan_copilot(input: &serde_json::Value, pane_id: &str) -> HookOutcome {
    let field = |snake, camel| str_field(input, snake).or_else(|| str_field(input, camel));
    let tool = field("tool_name", "toolName");
    let notification = field("notification_type", "notificationType");
    let stop_reason = field("stop_reason", "stopReason");
    let reason = str_field(input, "reason");
    let session_id = field("session_id", "sessionId");
    let event = field("hook_event_name", "hookEventName").unwrap_or_else(|| {
        if notification.is_some() {
            "notification"
        } else if input.get("toolResult").is_some() || input.get("tool_result").is_some() {
            "postToolUse"
        } else if input.get("error").is_some() && tool.is_some() {
            "postToolUseFailure"
        } else if tool.is_some() {
            "preToolUse"
        } else if stop_reason.is_some() {
            "agentStop"
        } else if reason.is_some() {
            "sessionEnd"
        } else if input.get("prompt").is_some() {
            "userPromptSubmitted"
        } else if input.get("initial_prompt").is_some()
            || input.get("initialPrompt").is_some()
            || input.get("source").is_some()
            || session_id.is_some()
        {
            "sessionStart"
        } else {
            ""
        }
        .to_string()
    });
    let event: String = event
        .chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    let state = match event.as_str() {
        "sessionstart" => {
            let has_prompt = input
                .get("initial_prompt")
                .or_else(|| input.get("initialPrompt"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|prompt| !prompt.trim().is_empty());
            if has_prompt {
                PaneAgentState::Working
            } else {
                PaneAgentState::Idle
            }
        }
        "userpromptsubmit" | "userpromptsubmitted" => PaneAgentState::Working,
        "pretooluse" => match tool.as_deref() {
            Some("ask_user" | "exit_plan_mode") => PaneAgentState::Blocked,
            _ => PaneAgentState::Working,
        },
        "posttooluse" | "posttoolusefailure" => {
            // report_intent may follow ask_user before the user has answered.
            if tool.as_deref() == Some("report_intent") {
                return HookOutcome::default();
            }
            PaneAgentState::Working
        }
        "notification" => match notification.as_deref() {
            Some("permission_prompt" | "elicitation_dialog") => PaneAgentState::Blocked,
            Some("agent_idle") => PaneAgentState::Idle,
            _ => return HookOutcome::default(),
        },
        "stop" | "agentstop" | "sessionstop" => {
            if stop_reason
                .as_deref()
                .is_some_and(|reason| reason != "end_turn")
            {
                return HookOutcome::default();
            }
            PaneAgentState::Idle
        }
        "sessionend" => {
            // complete is a turn boundary, not an ownership release.
            return if matches!(reason.as_deref(), Some("user_exit" | "abort")) {
                HookOutcome {
                    reports: vec![Method::PaneReleaseAgent(PaneReleaseAgentParams {
                        pane_id: pane_id.to_string(),
                        source: Agent::Copilot.source().to_string(),
                        agent: Agent::Copilot.agent().to_string(),
                        seq: Some(seq()),
                    })],
                    ..HookOutcome::default()
                }
            } else {
                HookOutcome::default()
            };
        }
        _ => return HookOutcome::default(),
    };
    HookOutcome {
        reports: vec![Method::PaneReportAgent(PaneReportAgentParams {
            pane_id: pane_id.to_string(),
            source: Agent::Copilot.source().to_string(),
            agent: Agent::Copilot.agent().to_string(),
            state,
            message: None,
            custom_status: None,
            seq: Some(seq()),
            agent_session_id: session_id,
            agent_session_path: None,
        })],
        ..HookOutcome::default()
    }
}

/// A subagent's completion, identified by `agent_id` on the payload. Qoder
/// emits the parent pane's own hooks for these, so forwarding an `idle` or a
/// release would make the pane look done while the parent turn is still
/// running. Carried over from the shim verbatim — the `SubagentStop` half of
/// the same guard lives in [`run_hook_command`], which every action shares.
fn is_subagent(input: &serde_json::Value) -> bool {
    str_field(input, "agent_id").is_some_and(|id| !id.is_empty())
}

/// A host-reported state (#238). The state is the argument, so nothing is
/// inferred from the payload — but the payload is still consulted for the two
/// things the shims took from it: the session id to attach, and whether this
/// is a subagent whose completion must not settle the parent pane.
fn plan_state(
    agent: Agent,
    state: PaneAgentState,
    input: &serde_json::Value,
    pane_id: &str,
) -> HookOutcome {
    if matches!(state, PaneAgentState::Idle) && is_subagent(input) {
        return HookOutcome::default();
    }
    HookOutcome {
        reports: vec![Method::PaneReportAgent(PaneReportAgentParams {
            pane_id: pane_id.to_string(),
            source: agent.source().to_string(),
            agent: agent.agent().to_string(),
            state,
            message: None,
            custom_status: None,
            seq: Some(seq()),
            agent_session_id: str_field(input, "session_id").filter(|id| !id.is_empty()),
            agent_session_path: None,
        })],
        stdout: None,
        config_dir_record: None,
    }
}

fn plan_release(agent: Agent, input: &serde_json::Value, pane_id: &str) -> HookOutcome {
    if is_subagent(input) {
        return HookOutcome::default();
    }
    HookOutcome {
        reports: vec![Method::PaneReleaseAgent(PaneReleaseAgentParams {
            pane_id: pane_id.to_string(),
            source: agent.source().to_string(),
            agent: agent.agent().to_string(),
            seq: Some(seq()),
        })],
        stdout: None,
        config_dir_record: None,
    }
}

fn plan_session(
    agent: Agent,
    input: &serde_json::Value,
    hook_event_name: &str,
    pane_id: &str,
    config_dir: Option<&str>,
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

    // Remember which account profile (CLAUDE_CONFIG_DIR) this Claude session is
    // running under, so a resume after a flk restart re-selects it instead of
    // orphaning onto the default ~/.claude account. Claude-only: it's the one
    // agent whose auth is relocated by an env var.
    let config_dir_record = matches!(agent, Agent::Claude)
        .then(|| config_dir.filter(|dir| !dir.is_empty()))
        .flatten()
        .map(|dir| (session_id.clone(), dir.to_string()));

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
        config_dir_record,
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
        config_dir_record: None,
    }
}

fn plan_stop(
    agent: Agent,
    input: &serde_json::Value,
    pane_id: &str,
    pending_messages: usize,
) -> HookOutcome {
    // Parity with the shim's `bool(hook_input.get("agent_id"))`: a non-empty
    // string agent_id marks a subagent. `str_field` already rejects empty
    // strings, so an `agent_id: ""` is (correctly) not treated as a subagent.
    let is_subagent = str_field(input, "agent_id").is_some();
    // The text the turn ended with. `None` means it could not be known: the
    // transcript has not caught up with the turn yet (#415).
    let Some(last_assistant) = final_text(input) else {
        // Unknown is not "no sentinel". Asking for a recap here re-nudged
        // turns that had ended with one, on nearly every turn of every
        // session, so the recap is left alone — mail still wakes.
        return HookOutcome {
            stdout: mail_nudge(pending_messages, false),
            ..HookOutcome::default()
        };
    };
    // Inside a continuation a Stop hook already forced, never force another:
    // at most one recap nudge per turn, whatever the reply looked like.
    // The flag is also set when another plugin's Stop hook forced the
    // continuation, and skipping flock's nudge then is intended: that turn
    // was not the agent's own ending either.
    let stop_hook_active = input
        .get("stop_hook_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

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
    if let Some(recap) = last_assistant.lines().find_map(recap_line) {
        outcome
            .reports
            .push(Method::PaneReportRecap(PaneReportRecapParams {
                pane_id: pane_id.to_string(),
                source: agent.source().to_string(),
                agent: agent.agent().to_string(),
                recap: cap(recap, 4096),
                seq: Some(seq()),
            }));
        // A clean turn still has to be told about mail.
        outcome.stdout = mail_nudge(pending_messages, false);
        return outcome;
    }

    // No sentinel: nudge for one more turn (self-heal, never user-facing).
    // Skip when we saw no assistant text, this is a subagent, or this turn is
    // already the nudge's continuation, so we don't loop on nothing.
    if !last_assistant.is_empty() && !is_subagent && !stop_hook_active {
        outcome.stdout = mail_nudge(pending_messages, true);
    } else {
        outcome.stdout = mail_nudge(pending_messages, false);
    }
    outcome
}

/// The `decision:block` payload that wakes an agent at its turn boundary.
///
/// ADR-0008: this names the COUNT and the tool, never a body. Message content
/// reaches the recipient only through `flock_msg_read`, so nothing another
/// agent wrote can arrive dressed as the operator's instruction — the wake
/// channel carries no attacker-controlled text.
///
/// Mail and the recap self-heal are never fused. The recap asks a turn that is
/// ENDING to summarise itself and stop; a turn with mail waiting is not ending,
/// and telling it both made an agent read a `needs_reply` review, write its
/// plan into the recap line and stop without doing the work — the message
/// acknowledged and dropped (#408). So mail pending means read AND act, with
/// no word of stopping; the recap is asked for at the next boundary, when the
/// inbox is empty and the turn really is over.
fn mail_nudge(pending_messages: usize, want_recap: bool) -> Option<String> {
    let reason = match (pending_messages, want_recap) {
        (0, false) => return None,
        (0, true) => "End your turn with a single sentinel line: `※ recap: \
                      <current state>. Next: <one concrete step>.` Then stop."
            .to_string(),
        (n, _) => format!(
            "You have {n} unread message{} from other agents. Read {} with the \
             `flock_msg_read` tool and act on any that need a reply before you end \
             your turn.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "it" } else { "them" }
        ),
    };
    Some(serde_json::json!({ "decision": "block", "reason": reason }).to_string())
}

/// Ask the server how many messages this pane may be woken about (#316).
///
/// Never consumes — the agent reads its own inbox — and never fetches bodies:
/// `msg.wake` answers with a count, so the wake path structurally cannot
/// carry text a sender wrote (ADR-0008). It also answers ZERO when a wake is
/// suppressed, which is why the hook asks this rather than `msg.list`: the
/// decision belongs to the server, where the fleet pause and the recipient's
/// own mute are known.
///
/// Any failure counts as zero. A hook must never block or fail the parent
/// agent, so an unreachable or older server means no nudge.
fn wake_message_count(pane_id: &str) -> usize {
    let request = Request {
        id: format!("flock:hook:{}", seq()),
        method: Method::MsgWake(crate::api::schema::MsgWakeParams {
            pane: Some(pane_id.to_string()),
        }),
    };
    ApiClient::local()
        .request_value_with_timeout(&request, HOOK_TIMEOUT)
        .ok()
        .map_or(0, |value| wake_count_from_response(&value))
}

/// Pure half of [`wake_message_count`]: read the count out of a `msg.wake`
/// response.
///
/// Split out so a test can pin it against a response built from the server's
/// OWN `ResponseResult` type. The hook reads the wire by pointer, and a
/// pointer that stops matching what the handler emits fails silently — the
/// agent simply never hears about its mail again (#328's shape).
fn wake_count_from_response(value: &serde_json::Value) -> usize {
    value
        .pointer("/result/count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0)
}

/// Apply a planned outcome: fire each report (swallowing errors) then print any
/// stdout. The only place this module touches the socket or stdout.
fn emit(outcome: HookOutcome) {
    // Persist the account-profile record before the reports: a resume needs it
    // even if the socket send below fails, and a hook must never fail its parent
    // agent, so neither leg can be allowed to skip the other.
    if let Some((session_id, config_dir)) = outcome.config_dir_record.clone() {
        crate::agent_resume::record_claude_config_dir(&session_id, &config_dir);
    }
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

/// The final reply as the harness itself reports it on the Stop input.
///
/// Claude Code sends `last_assistant_message` on current versions, and it is
/// the only source that cannot lag: the harness appends the reply to the
/// transcript only AFTER the Stop hook returns, so a transcript read during
/// the hook sees the message before it (#415, measured on 2.1.281). A present
/// field is authoritative even when empty.
fn harness_final_text(input: &serde_json::Value) -> Option<String> {
    input
        .get("last_assistant_message")
        .and_then(serde_json::Value::as_str)
        .map(|text| text.trim().to_string())
}

/// The text the turn ended with: the harness's own field, else the transcript
/// when it has caught up with the turn. `None` means it cannot be known.
///
/// A missing or unreadable transcript is `Some("")` — nothing to report and
/// nothing to nudge about, exactly as before — and not `None`, which is kept
/// for the one case that used to lie: a transcript that exists but lags.
fn final_text(input: &serde_json::Value) -> Option<String> {
    if let Some(text) = harness_final_text(input) {
        return Some(text);
    }
    let Some(tail) = str_field(input, "transcript_path").and_then(|path| read_transcript(&path))
    else {
        return Some(String::new());
    };
    tail.settled
        .then(|| tail.last_assistant.unwrap_or_default())
}

/// Re-read the transcript until the turn's final reply has landed, or the
/// budget is spent. Returns whether it settled. The waiting half of
/// [`final_text`]'s fallback, kept at the IO edge so `plan` never sleeps.
///
/// Both knobs come from config, so neither may hurt the turn: the budget is
/// capped at the harness's own hook timeout (waiting past it only gets the
/// hook killed, and an absurd value must not overflow `Instant`), and the
/// poll is floored so `0` cannot busy-spin whole-file reads.
fn await_settled_transcript(path: &str, budget: Duration, poll: Duration) -> bool {
    let budget = budget.min(Duration::from_secs(crate::integration::CLAUDE_HOOK_TIMEOUT));
    let poll = poll.max(MIN_TRANSCRIPT_POLL);
    let deadline = std::time::Instant::now() + budget;
    loop {
        match read_transcript(path) {
            None => return false,
            Some(tail) if tail.settled => return true,
            Some(_) => {}
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(poll.min(deadline - now));
    }
}

/// The recap sentinel within one line of the reply, if the line is one.
///
/// Agents dress the line in markdown — bold, a list item, a quote — so the
/// leading `**`, `- ` and `> ` are peeled (and a closing `**` with them)
/// before matching `※ recap:` itself; any other `※` line is not a recap.
fn recap_line(line: &str) -> Option<&str> {
    Some(crate::agent_transcript::undecorated_line(line))
        .filter(|line| line.starts_with(RECAP_SENTINEL))
}

/// What a transcript read found.
struct TranscriptTail {
    /// Text of the newest assistant entry that has any.
    last_assistant: Option<String>,
    /// Whether the newest conversational entry is an assistant entry with
    /// text. A turn always ends on an assistant reply, so a `user` entry last
    /// — a prompt, a tool result, a Stop hook's feedback — means the reply has
    /// not landed and `last_assistant` is the PREVIOUS message. Claude writes
    /// each content block as its own entry, so a textless assistant entry last
    /// (a thinking block) may be a reply whose text is still to come.
    settled: bool,
}

fn read_transcript(path: &str) -> Option<TranscriptTail> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut settled = None;
    for line in content.lines().rev() {
        let Some((role, text)) = transcript_entry(line) else {
            continue;
        };
        let is_assistant = role == "assistant";
        settled.get_or_insert(is_assistant && !text.is_empty());
        if is_assistant && !text.is_empty() {
            return Some(TranscriptTail {
                last_assistant: Some(text),
                settled: settled.unwrap_or(true),
            });
        }
    }
    Some(TranscriptTail {
        last_assistant: None,
        settled: settled.unwrap_or(true),
    })
}

/// Walk a Claude JSONL transcript backwards for the last assistant message's
/// text, whether or not the transcript has caught up with the turn.
#[cfg(test)]
fn last_assistant_text(path: &str) -> Option<String> {
    read_transcript(path)?.last_assistant
}

/// One transcript line as `(role, text)`, for the conversational entries only
/// (`user` / `assistant`). Transcript shapes vary by version: role is on the
/// top-level object or the nested `message` object; content is a string or a
/// list of `text` blocks.
fn transcript_entry(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let obj = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let message = obj.get("message").filter(|m| m.is_object());
    let role = message
        .and_then(|m| str_field(m, "role"))
        .or_else(|| str_field(&obj, "role"))
        .or_else(|| str_field(&obj, "type"))
        .filter(|role| role == "assistant" || role == "user")?;
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
    Some((role, text))
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

    /// ADR-0018 §1, from where the value starts to where it is observed: a
    /// `msg.send` as a client puts it on the socket, the `msg.wake` the stop
    /// hook asks, and the hook's own decision about whether to block the turn.
    /// A unit test on `wake_count` alone would pass with the handler still
    /// reporting `queued_len` — the wiring is the thing under test.
    #[tokio::test]
    async fn an_inbox_of_only_fyi_never_blocks_the_turn_and_a_question_names_everything() {
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![
            crate::workspace::Workspace::test_new("alpha"),
            crate::workspace::Workspace::test_new("beta"),
        ];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane = {
            let pane_id = app.state.workspaces[1].focused_pane_id().expect("pane");
            app.state.public_pane_id(1, pane_id).expect("public id")
        };
        let mut wire = |request: serde_json::Value| -> serde_json::Value {
            let response = app.handle_api_request(
                serde_json::from_value(request).expect("a request a client sends"),
            );
            serde_json::from_str(&response).expect("json response")
        };
        let send = |cid: &str, intent: &str| {
            json!({"id": "req", "method": "msg.send", "params": {
                "to": {"type": "pane", "pane": pane}, "body": "hi",
                "correlation_id": cid, "intent": intent}})
        };
        let wake = json!({"id": "flock:hook:1", "method": "msg.wake",
            "params": {"pane": pane}});

        for cid in ["c-fyi-1", "c-fyi-2"] {
            assert!(wire(send(cid, "fyi")).get("result").is_some());
        }
        let answered = wire(wake.clone());
        assert_eq!(
            mail_nudge(wake_count_from_response(&answered), false),
            None,
            "notices alone must not cost the recipient a turn: {answered}"
        );

        assert!(wire(send("c-question", "needs_reply"))
            .get("result")
            .is_some());
        let answered = wire(wake);
        let nudge = mail_nudge(wake_count_from_response(&answered), false)
            .expect("a question wakes the recipient");
        assert!(
            nudge.contains("3 unread messages"),
            "once a wake fires it names the notices too, so the read takes them: {nudge}"
        );
    }

    /// #316: the hook reads the wake count out of the wire by pointer, and a
    /// pointer that stops matching what the handler emits fails SILENTLY —
    /// the agent simply never hears about its mail again. So build the
    /// response from the server's own `ResponseResult` rather than from a
    /// hand-written literal, and let a shape change break this test.
    #[test]
    fn the_wake_count_is_read_from_the_servers_own_response_shape() {
        let encode = |result| {
            serde_json::to_value(crate::api::schema::SuccessResponse {
                id: "flock:hook:1".into(),
                result,
            })
            .expect("encode")
        };

        let woken = encode(crate::api::schema::ResponseResult::MsgWake {
            pane: None,
            channel_push: false,
            count: 3,
            suppressed: None,
            muted_until_ms: None,
        });
        assert_eq!(wake_count_from_response(&woken), 3);

        // A suppressed wake reports zero in the count itself, so a hook that
        // reads only the count cannot nudge through a pause or a mute.
        let suppressed = encode(crate::api::schema::ResponseResult::MsgWake {
            pane: None,
            channel_push: false,
            count: 0,
            suppressed: Some("fleet_paused".into()),
            muted_until_ms: None,
        });
        assert_eq!(wake_count_from_response(&suppressed), 0);
        assert_eq!(
            mail_nudge(wake_count_from_response(&suppressed), false),
            None
        );

        // An older server, or one that answered with an error, is not a
        // reason to block the parent agent's turn.
        assert_eq!(
            wake_count_from_response(&json!({"error": {"code": "x"}})),
            0
        );
    }

    fn method_name(method: &Method) -> &'static str {
        match method {
            Method::PaneReportAgentSession(_) => "report_agent_session",
            Method::PaneReportPrompt(_) => "report_prompt",
            Method::PaneReportReply(_) => "report_reply",
            Method::PaneReportRecap(_) => "report_recap",
            Method::PaneReportAgent(_) => "report_agent",
            Method::PaneReleaseAgent(_) => "release_agent",
            _ => "other",
        }
    }

    // --- #238: the state-reporting agents whose shims were python3 ------

    /// The shims built this JSON by hand in an embedded heredoc. Pin the shape
    /// they produced so the port is parity, not a rewrite.
    #[test]
    fn state_actions_report_the_state_verbatim() {
        for (raw, expected) in [
            ("working", PaneAgentState::Working),
            ("idle", PaneAgentState::Idle),
            ("blocked", PaneAgentState::Blocked),
        ] {
            let action = Action::parse(raw).expect("state action parses");
            let outcome = plan(Agent::Kimi, action, &json!({}), "", "p_23", 0, None);
            assert_eq!(outcome.reports.len(), 1, "{raw}");
            let Method::PaneReportAgent(params) = &outcome.reports[0] else {
                panic!(
                    "expected report_agent for {raw}, got {}",
                    method_name(&outcome.reports[0])
                );
            };
            assert_eq!(params.state, expected, "{raw}");
            assert_eq!(params.pane_id, "p_23");
            assert_eq!(params.source, "flock:kimi");
            assert_eq!(params.agent, "kimi");
            assert!(
                params.seq.is_some(),
                "seq is what orders concurrent reports"
            );
            assert!(outcome.stdout.is_none(), "only Claude's Stop writes stdout");
        }
    }

    #[test]
    fn release_action_releases_rather_than_reporting_a_state() {
        let outcome = plan(
            Agent::Qodercli,
            Action::parse("release").unwrap(),
            &json!({}),
            "",
            "p_7",
            0,
            None,
        );
        let Method::PaneReleaseAgent(params) = &outcome.reports[0] else {
            panic!(
                "expected release_agent, got {}",
                method_name(&outcome.reports[0])
            );
        };
        assert_eq!(params.source, "flock:qodercli");
        assert_eq!(params.agent, "qodercli");
    }

    /// The state is the argument, but the payload still supplies the session
    /// id when the host sends one (qoder does; kimi sends nothing at all).
    #[test]
    fn state_actions_attach_the_session_id_when_the_payload_carries_one() {
        let with_id = plan(
            Agent::Qodercli,
            Action::parse("working").unwrap(),
            &json!({"session_id": "sess-1"}),
            "",
            "p_1",
            0,
            None,
        );
        let Method::PaneReportAgent(params) = &with_id.reports[0] else {
            panic!("expected report_agent");
        };
        assert_eq!(params.agent_session_id.as_deref(), Some("sess-1"));

        // kimi's host sends no payload at all — absent, not empty-string.
        let bare = plan(
            Agent::Kimi,
            Action::parse("working").unwrap(),
            &json!({}),
            "",
            "p_1",
            0,
            None,
        );
        let Method::PaneReportAgent(params) = &bare.reports[0] else {
            panic!("expected report_agent");
        };
        assert!(params.agent_session_id.is_none());
    }

    /// Regression guard: a subagent finishing must not settle the PARENT pane.
    /// The shim guarded this and a naive port drops it — the pane would read as
    /// idle while the parent turn was still running.
    #[test]
    fn a_subagents_completion_does_not_settle_the_parent_pane() {
        let sub = json!({"agent_id": "sub-1", "session_id": "sess-1"});
        for raw in ["idle", "release"] {
            let outcome = plan(
                Agent::Qodercli,
                Action::parse(raw).unwrap(),
                &sub,
                "",
                "p_1",
                0,
                None,
            );
            assert!(
                outcome.reports.is_empty(),
                "{raw} from a subagent must be dropped"
            );
        }
        // `working` from a subagent is still real activity — only the settling
        // states are suppressed.
        let working = plan(
            Agent::Qodercli,
            Action::parse("working").unwrap(),
            &sub,
            "",
            "p_1",
            0,
            None,
        );
        assert_eq!(working.reports.len(), 1);
    }

    /// Codex's shim only ever handled `session`, and its body was exactly what
    /// plan_session already does — that is why it needed no new action.
    #[test]
    fn codex_session_reports_the_agent_session_id() {
        let outcome = plan(
            Agent::Codex,
            Action::parse("session").unwrap(),
            &json!({"session_id": "sess-abc"}),
            "SessionStart",
            "p_9",
            0,
            None,
        );
        let Method::PaneReportAgentSession(params) = &outcome.reports[0] else {
            panic!("expected report_agent_session");
        };
        assert_eq!(params.agent_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(params.source, "flock:codex");
        // Only Claude may forward SessionStart's `source`; codex must not.
        assert!(params.session_start_source.is_none());
    }

    #[test]
    fn stop_is_a_no_op_for_the_non_claude_agents() {
        for agent in [Agent::Codex, Agent::Kimi, Agent::Qodercli] {
            let outcome = plan(agent, Action::Stop, &json!({}), "Stop", "p_1", 3, None);
            assert!(outcome.reports.is_empty());
            assert!(outcome.stdout.is_none(), "a nudge would be Claude-only");
        }
    }

    /// The state vocabulary is per-agent, not global. Adding it to the shared
    /// `Action::parse` made it valid for EVERY agent, which let
    /// `flk hook claude working` drive claude's pane state directly — caught by
    /// `claude_hook_ignores_state_actions` in tests/cli_wrapper.rs. Pinned here
    /// too so the unit layer fails first and names the reason.
    #[test]
    fn only_state_reporting_agents_honour_the_state_vocabulary() {
        for agent in [Agent::Claude, Agent::Opencode, Agent::Codex] {
            for raw in ["working", "idle", "blocked", "release"] {
                let outcome = plan(
                    agent,
                    Action::parse(raw).unwrap(),
                    &json!({}),
                    "",
                    "p_1",
                    0,
                    None,
                );
                assert!(
                    outcome.reports.is_empty(),
                    "{} must ignore `{raw}`",
                    agent.agent()
                );
            }
        }
        for agent in [Agent::Kimi, Agent::Qodercli] {
            let outcome = plan(
                agent,
                Action::parse("working").unwrap(),
                &json!({}),
                "",
                "p_1",
                0,
                None,
            );
            assert_eq!(outcome.reports.len(), 1, "{} reports state", agent.agent());
        }
    }

    #[test]
    fn every_ported_agent_parses_and_keeps_its_identity() {
        for (raw, source, agent) in [
            ("codex", "flock:codex", "codex"),
            ("kimi", "flock:kimi", "kimi"),
            ("qodercli", "flock:qodercli", "qodercli"),
            ("copilot", "flock:copilot", "copilot"),
        ] {
            let parsed = Agent::parse(raw).expect("agent parses");
            assert_eq!(parsed.source(), source);
            assert_eq!(parsed.agent(), agent);
        }
    }

    #[test]
    fn copilot_event_reports_preserve_state_and_session_identity() {
        for (input, state) in [
            (json!({"session_id": "s"}), PaneAgentState::Idle),
            (
                json!({"sessionId": "s", "initialPrompt": "go"}),
                PaneAgentState::Working,
            ),
            (
                json!({"sessionId": "s", "initialPrompt": "  "}),
                PaneAgentState::Idle,
            ),
            (
                json!({"session_id": "s", "prompt": "go"}),
                PaneAgentState::Working,
            ),
            (
                json!({"session_id": "s", "tool_name": "ask_user"}),
                PaneAgentState::Blocked,
            ),
            (
                json!({"sessionId": "s", "toolName": "exit_plan_mode"}),
                PaneAgentState::Blocked,
            ),
            (
                json!({"session_id": "s", "tool_name": "bash"}),
                PaneAgentState::Working,
            ),
            (
                json!({"sessionId": "s", "toolName": "ask_user", "toolResult": null}),
                PaneAgentState::Working,
            ),
            (
                json!({"session_id": "s", "tool_name": "exit_plan_mode", "error": null}),
                PaneAgentState::Working,
            ),
            (
                json!({"session_id": "s", "notification_type": "permission_prompt"}),
                PaneAgentState::Blocked,
            ),
            (
                json!({"sessionId": "s", "notificationType": "elicitation_dialog"}),
                PaneAgentState::Blocked,
            ),
            (
                json!({"session_id": "s", "notification_type": "agent_idle"}),
                PaneAgentState::Idle,
            ),
            (
                json!({"sessionId": "s", "stopReason": "end_turn"}),
                PaneAgentState::Idle,
            ),
            (
                json!({"session_id": "s", "hook_event_name": "Stop"}),
                PaneAgentState::Idle,
            ),
            (
                json!({"session_id": "s", "hookEventName": "USER-PROMPT_SUBMITTED"}),
                PaneAgentState::Working,
            ),
        ] {
            let out = plan(Agent::Copilot, Action::Event, &input, "", "p_1", 0, None);
            assert_eq!(out.reports.len(), 1, "{input}");
            let Method::PaneReportAgent(report) = &out.reports[0] else {
                panic!("expected state report for {input}");
            };
            assert_eq!(report.state, state, "{input}");
            assert_eq!(report.agent_session_id.as_deref(), Some("s"));
            assert_eq!(report.source, "flock:copilot");
            assert_eq!(report.agent, "copilot");
            assert_eq!(report.pane_id, "p_1");
            assert!(report.seq.is_some());
            assert!(out.stdout.is_none());
        }
    }

    #[test]
    fn copilot_ignores_incidental_events_and_keeps_turn_ownership() {
        for input in [
            json!({}),
            json!(null),
            json!({"hookEventName": "unknown", "prompt": "go"}),
            json!({"tool_name": "report_intent", "tool_result": {}}),
            json!({"toolName": "report_intent", "error": "failed"}),
            json!({"notification_type": "other"}),
            json!({"stop_reason": "tool_use"}),
            json!({"reason": "complete"}),
            json!({"reason": "other"}),
        ] {
            assert!(plan_copilot(&input, "p_1").reports.is_empty(), "{input}");
        }
        for reason in ["user_exit", "abort"] {
            let out = plan_copilot(&json!({"reason": reason}), "p_1");
            assert_eq!(out.reports.len(), 1);
            let Method::PaneReleaseAgent(report) = &out.reports[0] else {
                panic!("expected release for {reason}");
            };
            assert_eq!(report.source, "flock:copilot");
            assert_eq!(report.agent, "copilot");
            assert_eq!(report.pane_id, "p_1");
        }
    }

    #[test]
    fn copilot_field_precedence_matches_the_shell_hook() {
        let out = plan_copilot(
            &json!({
                "hook_event_name": "session-start", "hookEventName": "PreToolUse",
                "session_id": "snake", "sessionId": "camel",
                "initial_prompt": null, "initialPrompt": "go", "tool_name": "ask_user"
            }),
            "p_1",
        );
        let Method::PaneReportAgent(report) = &out.reports[0] else {
            panic!("expected state report");
        };
        assert_eq!(report.state, PaneAgentState::Idle);
        assert_eq!(report.agent_session_id.as_deref(), Some("snake"));
        let out = plan_copilot(
            &json!({"hookEventName": "SessionStop", "session_id": 42}),
            "p_1",
        );
        let Method::PaneReportAgent(report) = &out.reports[0] else {
            panic!("expected state report without session id");
        };
        assert_eq!(report.agent_session_id, None);
    }

    // --- run_hook_command guards (all trip before stdin is read) ---------

    #[test]
    fn missing_args_is_usage_error() {
        assert_eq!(run_hook_command(&[]).unwrap(), 2);
        assert_eq!(run_hook_command(&["claude".into()]).unwrap(), 2);
    }

    #[test]
    fn outside_a_flock_pane_is_a_noop() {
        // No FLOCK_ENV: the hook must do nothing (and not read stdin).
        std::env::remove_var("FLOCK_ENV");
        let args = ["claude".into(), "session".into()];
        assert_eq!(run_hook_command(&args).unwrap(), 0);
    }

    #[test]
    fn unknown_agent_or_action_is_a_noop() {
        // Guards trip after the env check but before stdin, so a valid pane env
        // with an unknown agent/action returns cleanly without blocking on read.
        std::env::set_var("FLOCK_ENV", "1");
        std::env::set_var("FLOCK_PANE_ID", "p_1");
        std::env::set_var("FLOCK_SOCKET_PATH", "/nonexistent/flk-hook-guard.sock");
        assert_eq!(
            run_hook_command(&["kimi".into(), "session".into()]).unwrap(),
            0
        );
        assert_eq!(
            run_hook_command(&["claude".into(), "explode".into()]).unwrap(),
            0
        );
        std::env::remove_var("FLOCK_ENV");
        std::env::remove_var("FLOCK_PANE_ID");
        std::env::remove_var("FLOCK_SOCKET_PATH");
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
            0,
            None,
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
    fn claude_session_records_config_dir_when_set() {
        // The account-profile recall: a Claude SessionStart under a
        // CLAUDE_CONFIG_DIR yields a (session_id, config_dir) record so a
        // post-restart resume re-selects the same account instead of
        // orphaning onto the default ~/.claude.
        let input = json!({"hook_event_name": "SessionStart", "session_id": "sid-9"});
        let out = plan(
            Agent::Claude,
            Action::Session,
            &input,
            "SessionStart",
            "p_1",
            0,
            Some("/home/u/.claude-profiles/work"),
        );
        assert_eq!(
            out.config_dir_record,
            Some(("sid-9".into(), "/home/u/.claude-profiles/work".into()))
        );
    }

    #[test]
    fn session_records_no_config_dir_when_unset_or_non_claude() {
        // No CLAUDE_CONFIG_DIR (the default account) — nothing to record.
        let input = json!({"hook_event_name": "SessionStart", "session_id": "sid-9"});
        assert!(plan(
            Agent::Claude,
            Action::Session,
            &input,
            "SessionStart",
            "p_1",
            0,
            None,
        )
        .config_dir_record
        .is_none());
        // Other agents do not relocate auth by env var, so recording one for
        // them would attach a Claude-shaped fact to a non-Claude session.
        assert!(plan(
            Agent::Opencode,
            Action::Session,
            &json!({"session_id": "os-1"}),
            "",
            "p_1",
            0,
            Some("/home/u/.claude-profiles/work"),
        )
        .config_dir_record
        .is_none());
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
            0,
            None,
        );
        assert!(out.reports.is_empty());
    }

    #[test]
    fn session_source_suppressed_when_not_sessionstart() {
        // A non-SessionStart lifecycle action can't spoof an identity change.
        let input =
            json!({"hook_event_name": "SessionEnd", "session_id": "sid", "source": "resume"});
        let out = plan(
            Agent::Claude,
            Action::Session,
            &input,
            "SessionEnd",
            "p_1",
            0,
            None,
        );
        let Method::PaneReportAgentSession(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.session_start_source, None);
    }

    #[test]
    fn opencode_session_maps_to_report_with_opencode_identity() {
        let input = json!({"session_id": "os-1"});
        let out = plan(Agent::Opencode, Action::Session, &input, "", "p_1", 0, None);
        let Method::PaneReportAgentSession(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.source, "flock:opencode");
        assert_eq!(params.agent, "opencode");
        assert_eq!(params.agent_session_id.as_deref(), Some("os-1"));
        // Opencode never forwards session_start_source (claude-only).
        assert_eq!(params.session_start_source, None);
    }

    #[test]
    fn opencode_stop_is_a_noop() {
        // Only claude carries a scrapable transcript + nudge protocol on Stop.
        let input = json!({"hook_event_name": "Stop", "transcript_path": "/whatever"});
        let out = plan(
            Agent::Opencode,
            Action::Stop,
            &input,
            "Stop",
            "p_1",
            0,
            None,
        );
        assert!(out.reports.is_empty() && out.stdout.is_none());
    }

    // --- prompt ----------------------------------------------------------

    #[test]
    fn prompt_reports_text() {
        let input = json!({"prompt": "fix the bug"});
        let out = plan(Agent::Claude, Action::Prompt, &input, "", "p_1", 0, None);
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
                0,
                None,
            );
            assert!(out.reports.is_empty(), "expected no report for {p:?}");
        }
    }

    #[test]
    fn prompt_is_capped_at_the_wire_limit_by_codepoints() {
        // 20_000 multi-byte chars → capped to 16_384 chars (not bytes).
        let prompt = "é".repeat(20_000);
        let out = plan(
            Agent::Claude,
            Action::Prompt,
            &json!({ "prompt": prompt }),
            "",
            "p_1",
            0,
            None,
        );
        let Method::PaneReportPrompt(params) = &out.reports[0] else {
            panic!()
        };
        assert_eq!(params.prompt.chars().count(), 16_384);
    }

    // --- stop ------------------------------------------------------------

    fn transcript_with(lines: &[&str]) -> std::path::PathBuf {
        // The pid is load-bearing: nextest gives each test its own process, so
        // `seq()`'s counter restarts at 0 per process and only the clock reading
        // separates them. On a CI runner with a coarse clock two processes can
        // land the same name — and every test here ends with `remove_dir_all` on
        // its parent, so a collision deletes the other test's transcript
        // mid-run. That is the `stop_without_sentinel_nudges` flake. Every other
        // temp-dir fixture in this repo already includes the pid.
        let dir = std::env::temp_dir().join(format!("flk-hook-{}-{}", std::process::id(), seq()));
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
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 0, None);
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
    fn stop_wakes_the_agent_when_mail_is_waiting() {
        // ADR-0008: the inbox is pull, so the stop hook is what tells an idle
        // agent mail arrived. The nudge names the COUNT and the TOOL — never a
        // body, so nothing another agent wrote reaches this one through the
        // wake channel.
        let path = transcript_with(&[
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"Done.\n※ recap: done. Next: ship."}]}}"#,
        ]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});

        // Clean turn, no mail: nothing to say.
        let quiet = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 0, None);
        assert!(
            quiet.stdout.is_none(),
            "no mail, sentinel present ⇒ no nudge"
        );

        // Clean turn WITH mail: still woken.
        let woken = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 2, None);
        let nudge = woken.stdout.expect("mail must wake a clean turn too");
        assert!(nudge.contains("\"decision\":\"block\""), "{nudge}");
        assert!(nudge.contains("2 unread messages"), "{nudge}");
        assert!(nudge.contains("flock_msg_read"), "{nudge}");

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_mail_wake_says_act_and_never_stop_even_when_the_recap_is_missing() {
        // #408, observed live: fused with the recap self-heal, the wake said
        // "read it … Then … Then stop", and the agent did exactly that — read a
        // `needs_reply` review, put its plan in the recap line and stopped
        // without doing the work. A turn with mail waiting is not ending, so
        // the wake asks for action and the recap waits for the next boundary.
        let path = transcript_with(&[r#"{"type":"assistant","content":"No sentinel here."}"#]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 1, None);
        let nudge = out.stdout.expect("expected a nudge");
        assert!(nudge.contains("1 unread message"), "{nudge}");
        assert!(!nudge.contains("1 unread messages"), "singular: {nudge}");
        assert!(nudge.contains("flock_msg_read"), "{nudge}");
        assert!(nudge.contains("act on"), "{nudge}");
        assert!(
            !nudge.contains("recap"),
            "the recap waits for an empty inbox: {nudge}"
        );
        for (pending, want_recap) in [(1, false), (1, true), (3, false), (3, true)] {
            let nudge = mail_nudge(pending, want_recap).expect("mail wakes");
            assert!(
                !nudge.to_lowercase().contains("stop"),
                "a mail wake must never tell the agent to stop: {nudge}"
            );
        }

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stop_without_sentinel_nudges() {
        let path = transcript_with(&[
            r#"{"type":"assistant","content":"Just did the work, no sentinel."}"#,
        ]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 0, None);
        assert_eq!(
            out.reports.iter().map(method_name).collect::<Vec<_>>(),
            ["report_reply"]
        );
        let nudge = out.stdout.expect("expected a nudge");
        assert!(nudge.contains("\"decision\":\"block\""));
        assert!(nudge.contains("※ recap:"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    // --- #415: the recap is judged on what the turn really ended with ------

    /// A tool-using turn as Claude 2.1.x writes it: one entry per content
    /// block, tool results as `user` entries.
    const EARLIER_REPLY: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Checking the tests first."}]}}"#;
    const TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;
    const TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
    const THINKING: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":""}]}}"#;
    const FINAL_WITH_RECAP: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"All green.\n\n※ recap: tests pass. Next: open the PR."}]}}"#;
    const FINAL_REPLY_RECAP: &str = "All green.\n\n※ recap: tests pass. Next: open the PR.";

    fn stop(input: &serde_json::Value, pending_messages: usize) -> HookOutcome {
        plan(
            Agent::Claude,
            Action::Stop,
            input,
            "Stop",
            "p_1",
            pending_messages,
            None,
        )
    }

    fn recap_of(out: &HookOutcome) -> Option<&str> {
        out.reports.iter().find_map(|method| match method {
            Method::PaneReportRecap(recap) => Some(recap.recap.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_transcript_ending_on_the_sentinel_is_never_nudged() {
        // (a) The final entry has landed and carries the recap.
        let path = transcript_with(&[
            EARLIER_REPLY,
            TOOL_USE,
            TOOL_RESULT,
            THINKING,
            FINAL_WITH_RECAP,
        ]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap(), "stop_hook_active": false});
        let out = stop(&input, 0);
        assert_eq!(
            recap_of(&out),
            Some("※ recap: tests pass. Next: open the PR.")
        );
        assert!(out.stdout.is_none(), "sentinel present ⇒ no nudge");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_harness_field_wins_over_a_transcript_one_message_behind() {
        // (b) #415 as observed: the reply is not in the transcript yet, so the
        // newest text there is the message BEFORE it, which has no sentinel.
        // The hook input's own `last_assistant_message` has the real reply.
        let path = transcript_with(&[EARLIER_REPLY, TOOL_USE, TOOL_RESULT]);
        let input = json!({
            "hook_event_name": "Stop",
            "transcript_path": path.to_str().unwrap(),
            "stop_hook_active": false,
            "last_assistant_message": FINAL_REPLY_RECAP,
        });
        let out = stop(&input, 0);
        assert_eq!(
            recap_of(&out),
            Some("※ recap: tests pass. Next: open the PR.")
        );
        assert!(
            out.stdout.is_none(),
            "a turn that ended with the sentinel must never be re-nudged"
        );
        let Method::PaneReportReply(reply) = &out.reports[0] else {
            panic!("the reply is reported first")
        };
        assert_eq!(
            reply.reply, FINAL_REPLY_RECAP,
            "the reply, not the one before it"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_lagging_transcript_without_the_field_is_unknown_not_missing() {
        // An older harness sends no field. A transcript whose newest entry is
        // a tool result — or a thinking block whose text is still to come —
        // shows the previous message, so the hook cannot tell whether the turn
        // ended with a recap and must not ask for one. Mail still wakes.
        for tail in [
            &[EARLIER_REPLY, TOOL_USE, TOOL_RESULT][..],
            &[EARLIER_REPLY, TOOL_USE, TOOL_RESULT, THINKING][..],
        ] {
            let path = transcript_with(tail);
            let input =
                json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
            let out = stop(&input, 0);
            assert!(out.reports.is_empty(), "nothing stale is reported");
            assert!(out.stdout.is_none(), "unknown is not a missing sentinel");

            let woken = stop(&input, 1).stdout.expect("mail still wakes");
            assert!(woken.contains("flock_msg_read"), "{woken}");
            assert!(!woken.contains("recap"), "{woken}");
            std::fs::remove_dir_all(path.parent().unwrap()).ok();
        }
    }

    #[test]
    fn a_turn_without_the_sentinel_is_nudged_exactly_once() {
        // (c) Genuinely no sentinel: one nudge. The continuation that nudge
        // forces arrives with `stop_hook_active: true`, and whatever it wrote,
        // it is not nudged again.
        let path = transcript_with(&[EARLIER_REPLY]);
        let first = json!({
            "hook_event_name": "Stop",
            "transcript_path": path.to_str().unwrap(),
            "stop_hook_active": false,
            "last_assistant_message": "Done, no recap line.",
        });
        let nudge = stop(&first, 0).stdout.expect("no sentinel ⇒ one nudge");
        assert!(nudge.contains("\"decision\":\"block\""), "{nudge}");
        assert!(nudge.contains("※ recap:"), "{nudge}");

        let again = json!({
            "hook_event_name": "Stop",
            "transcript_path": path.to_str().unwrap(),
            "stop_hook_active": true,
            "last_assistant_message": "Still no recap line.",
        });
        assert!(
            stop(&again, 0).stdout.is_none(),
            "at most one nudge per turn"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn inside_a_hook_continuation_the_recap_is_never_asked_for() {
        // (d) stop_hook_active: no recap nudge from the transcript path
        // either — but a mail wake is not a recap nudge, and still fires.
        let path = transcript_with(&[r#"{"type":"assistant","content":"No sentinel here."}"#]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap(), "stop_hook_active": true});
        let out = stop(&input, 0);
        assert_eq!(
            out.reports.iter().map(method_name).collect::<Vec<_>>(),
            ["report_reply"]
        );
        assert!(out.stdout.is_none(), "stop_hook_active ⇒ no recap nudge");

        let woken = stop(&input, 2).stdout.expect("mail still wakes");
        assert!(woken.contains("2 unread messages"), "{woken}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_wait_sees_a_final_entry_written_after_a_delay() {
        // An older harness that DOES flush while the hook runs: the wait at
        // the IO edge picks the reply up, and the plan then reads it.
        let path = transcript_with(&[EARLIER_REPLY, TOOL_USE, TOOL_RESULT]);
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                let mut body = std::fs::read_to_string(&path).unwrap();
                body.push('\n');
                body.push_str(FINAL_WITH_RECAP);
                std::fs::write(&path, body).unwrap();
            })
        };
        let settled = await_settled_transcript(
            path.to_str().unwrap(),
            Duration::from_secs(10),
            Duration::from_millis(10),
        );
        writer.join().unwrap();
        assert!(settled, "the delayed reply lands inside the budget");

        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap()});
        let out = stop(&input, 0);
        assert_eq!(
            recap_of(&out),
            Some("※ recap: tests pass. Next: open the PR.")
        );
        assert!(out.stdout.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_wait_gives_up_when_its_budget_is_spent() {
        // Claude 2.1.x appends the reply only after the hook returns, so the
        // wait must end on its own; `0` reads once and returns.
        let path = transcript_with(&[EARLIER_REPLY, TOOL_USE, TOOL_RESULT]);
        let started = std::time::Instant::now();
        assert!(!await_settled_transcript(
            path.to_str().unwrap(),
            Duration::from_millis(50),
            Duration::from_millis(10),
        ));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        assert!(!await_settled_transcript(
            path.to_str().unwrap(),
            Duration::ZERO,
            Duration::from_millis(10),
        ));
        assert!(
            !await_settled_transcript(
                "/no/such/path",
                Duration::from_secs(10),
                Duration::from_millis(10)
            ),
            "a missing transcript never waits"
        );
        // Config cannot hurt the turn: a zero poll is floored rather than
        // spinning, and a budget past `Instant`'s range is capped, not a
        // panic.
        let started = std::time::Instant::now();
        assert!(!await_settled_transcript(
            path.to_str().unwrap(),
            Duration::from_millis(20),
            Duration::ZERO,
        ));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        let writer = std::thread::spawn({
            let path = path.clone();
            move || {
                // Lands the reply so the capped wait ends; what is under test
                // is that `u64::MAX` ms reaches the loop without panicking.
                std::thread::sleep(Duration::from_millis(50));
                let mut body = std::fs::read_to_string(&path).unwrap();
                body.push('\n');
                body.push_str(FINAL_WITH_RECAP);
                std::fs::write(&path, body).unwrap();
            }
        });
        assert!(await_settled_transcript(
            path.to_str().unwrap(),
            Duration::from_millis(u64::MAX),
            Duration::from_millis(10),
        ));
        writer.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_recap_dressed_in_markdown_is_still_the_recap() {
        for (line, expected) in [
            (
                "※ recap: done. Next: ship.",
                Some("※ recap: done. Next: ship."),
            ),
            (
                "**※ recap: done. Next: ship.**",
                Some("※ recap: done. Next: ship."),
            ),
            (
                "- ※ recap: done. Next: ship.",
                Some("※ recap: done. Next: ship."),
            ),
            (
                "> ※ recap: done. Next: ship.",
                Some("※ recap: done. Next: ship."),
            ),
            (
                "  > **※ recap: done. Next: ship.**  ",
                Some("※ recap: done. Next: ship."),
            ),
            ("※ note: not a recap", None),
            ("the ※ recap: line goes last", None),
            ("", None),
        ] {
            assert_eq!(recap_line(line), expected, "{line:?}");
        }

        // End to end: a bolded recap is lifted and the turn is not nudged,
        // while a line that merely starts with ※ is no recap at all.
        let path = transcript_with(&[EARLIER_REPLY]);
        let bold = json!({
            "hook_event_name": "Stop",
            "transcript_path": path.to_str().unwrap(),
            "last_assistant_message": "Done.\n\n**※ recap: done. Next: ship.**",
        });
        let out = stop(&bold, 0);
        assert_eq!(recap_of(&out), Some("※ recap: done. Next: ship."));
        assert!(out.stdout.is_none(), "a bolded recap is a recap");

        let other = json!({
            "hook_event_name": "Stop",
            "transcript_path": path.to_str().unwrap(),
            "last_assistant_message": "Done.\n※ note: nothing to recap",
        });
        let out = stop(&other, 0);
        assert_eq!(recap_of(&out), None);
        assert!(
            out.stdout.is_some(),
            "a ※ line that is not the recap still nudges"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn stop_subagent_does_not_nudge() {
        let path =
            transcript_with(&[r#"{"type":"assistant","content":"subagent output, no sentinel"}"#]);
        let input = json!({"hook_event_name": "Stop", "transcript_path": path.to_str().unwrap(), "agent_id": "sub-1"});
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 0, None);
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
            0,
            None,
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
        let out = plan(Agent::Claude, Action::Stop, &input, "Stop", "p_1", 0, None);
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
