//! Telling a booting agent from a crashed one, and from one waiting on a
//! dialog (#362).
//!
//! Starting an agent answers the moment the child has exec'd and survived the
//! liveness window (#178). That is honest about the *process* and says nothing
//! about the *TUI*: a Claude pane spends 30-60 seconds painting nothing that
//! flock's detector recognises, and during that window `pane read` returns an
//! empty string. An operator watching that window cannot tell "still booting"
//! from "crashed on launch" from "blocked on a first-run permission dialog" —
//! all three look identical — so every retry is made blind. Five attempts were
//! needed to start one agent on a remote host, and four of the five failures
//! were silent.
//!
//! # What "ready" means
//!
//! Not "the pane is non-empty". A pane can be non-empty and still be
//! mid-splash, and a pane can be empty because the agent already died. Ready
//! here is flock's own answer to *what is this pane doing*: the agent status
//! is anything other than `unknown`.
//!
//! `unknown` is the only status flock reports when its detector has not
//! recognised the agent's own chrome — the invariant controls each agent's
//! screen detector gates on. A booting TUI is `unknown`; a TUI that has
//! painted is `idle`, `working` or `blocked`. So the first non-`unknown`
//! status is exactly the moment the caller's question becomes answerable, and
//! the status it lands on IS the answer: `blocked` says a dialog is waiting,
//! and no amount of further waiting will clear it.
//!
//! Because ready is a detector verdict, a pane running something flock does
//! not detect never becomes ready. That is not a failure to report — it is
//! what a timeout reporting the status it saw is for.
//!
//! # Why the wait lives here and not in the server
//!
//! The readiness the caller wants takes tens of seconds. `agent.start`'s
//! liveness wait blocks the app loop for 250ms and is uncomfortable at that;
//! blocking it for a minute would stop the render loop and the detector that
//! produces the very signal being waited on. So the wait is a client-side
//! composition over the events the server already publishes, the same shape
//! `flk agent wait` already takes.

use std::time::{Duration, Instant};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{
    AgentTarget, EventsSubscribeParams, Method, PaneReadParams, ReadFormat, ReadSource, Request,
    Subscription,
};

/// How long a readiness wait runs when the caller does not say.
///
/// Sized from the failure it exists for: the reported boot window for a Claude
/// TUI on a loaded host is 30-60 seconds. A default under that would turn the
/// normal case into a refusal, which is worse than no flag at all.
pub(super) const DEFAULT_READY_TIMEOUT_MS: u64 = 60_000;

/// How far up the pane to look for the line quoted back in a failed wait.
const READY_REPORT_TAIL_LINES: u32 = 40;

/// How much of that line to quote.
const READY_REPORT_MAX_CHARS: usize = 200;

/// The status flock reports when it cannot tell what a pane is running.
const UNKNOWN_STATUS: &str = "unknown";

/// Is this agent status one a caller can act on?
///
/// Every named status is: `idle`/`done` means the agent is at its prompt,
/// `working` that it took the turn, `blocked` that something is waiting on a
/// human, `hibernated` that the pane was parked. Only `unknown` — flock's "I
/// cannot tell" — is not, and it is what a booting TUI looks like.
pub(super) fn status_is_ready(status: &str) -> bool {
    !status.is_empty() && status != UNKNOWN_STATUS
}

/// What one line off the subscription stream means for a readiness wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReadySignal {
    /// The pane reached a status flock can name.
    Ready(String),
    /// The pane's process exited before it ever became ready.
    Exited,
    /// Anything else on the stream: another pane, a title change, a status
    /// that is still `unknown`.
    KeepWaiting,
}

/// Classify one event.
///
/// Two envelope shapes arrive on the same stream and are told apart by their
/// `event` tag: the parameterised agent-status subscription publishes
/// `pane.agent_status_changed`, and the plain event subscription republishes
/// the server's own `pane_exited`. Both are matched on `data.pane_id` because
/// the exit subscription has no per-pane filter to give it.
pub(super) fn classify_ready_event(event: &serde_json::Value, pane_id: &str) -> ReadySignal {
    if event["data"]["pane_id"].as_str() != Some(pane_id) {
        return ReadySignal::KeepWaiting;
    }
    match event["event"].as_str() {
        Some("pane.agent_status_changed") => match event["data"]["agent_status"].as_str() {
            Some(status) if status_is_ready(status) => ReadySignal::Ready(status.to_owned()),
            _ => ReadySignal::KeepWaiting,
        },
        Some("pane_exited") => ReadySignal::Exited,
        _ => ReadySignal::KeepWaiting,
    }
}

/// The last thing a pane printed, for a wait that has to explain itself.
///
/// Read over `pane.read` rather than `agent.read` on purpose: it needs no
/// agent identity, so it still answers for a pane whose agent has gone, which
/// is precisely the case being reported.
fn last_pane_line(pane_id: &str) -> Option<String> {
    let response = super::send_request(&Request {
        id: "cli:agent:ready:read".into(),
        method: Method::PaneRead(PaneReadParams {
            pane_id: pane_id.to_owned(),
            source: ReadSource::RecentUnwrapped,
            lines: Some(READY_REPORT_TAIL_LINES),
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
    })
    .ok()?;
    let text = response["result"]["read"]["text"].as_str()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(line.trim().chars().take(READY_REPORT_MAX_CHARS).collect())
}

/// Print a refusal in the same shape every other CLI failure prints, and
/// return the exit code with it.
fn refuse(code: &str, message: &str) -> i32 {
    let body = serde_json::json!({
        "id": "cli:agent:wait-ready",
        "error": { "code": code, "message": message },
    });
    match serde_json::to_string(&body) {
        Ok(json) => eprintln!("{json}"),
        Err(_) => eprintln!("{message}"),
    }
    1
}

/// Everything a failed wait knows, phrased as one sentence.
///
/// A timeout that says only "timed out" reproduces the bug it was added for:
/// the caller is back to not knowing whether the agent is slow, gone, or
/// waiting on a dialog. The status flock last saw and the last line the pane
/// printed are usually the whole explanation — and an agent that no longer
/// resolves at all is itself the answer, so it is reported rather than
/// flattened into `unknown`.
fn describe(target: &str, pane_id: &str) -> String {
    let status = current_status(target).map_or_else(
        || "gone (the agent no longer resolves)".to_string(),
        |(_, status)| status,
    );
    match last_pane_line(pane_id) {
        Some(line) => format!("last status {status}, last pane output: {line}"),
        None => format!("last status {status}, and the pane has printed nothing"),
    }
}

/// Read the agent's current status, if it still has one.
fn current_status(target: &str) -> Option<(serde_json::Value, String)> {
    let response = super::send_request(&Request {
        id: "cli:agent:ready:get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    })
    .ok()?;
    let status = response["result"]["agent"]["agent_status"]
        .as_str()?
        .to_owned();
    Some((response, status))
}

/// Block until `target`'s pane reports a status flock can name, then print the
/// agent exactly as `agent get` would.
///
/// `pane_id` is passed rather than re-resolved because the caller already has
/// it, and because a start whose agent dies mid-wait must still be reportable
/// after `agent get` has stopped answering for it.
pub(super) fn wait_until_ready(
    target: &str,
    pane_id: &str,
    timeout_ms: u64,
) -> std::io::Result<i32> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    // Subscribe FIRST, then snapshot. The subscription's own probe fixes the
    // sequence it starts delivering from, so a status that lands between the
    // ack and the snapshot is delivered rather than missed; taking the
    // snapshot first would leave that gap open.
    let subscribe = Request {
        id: "cli:agent:ready".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::PaneAgentStatusChanged {
                    pane_id: pane_id.to_owned(),
                    // No filter: the wait is for "no longer unknown", which is
                    // a set of statuses and not one of them.
                    agent_status: None,
                },
                // A pane that dies at second 5 is invisible to the start-time
                // liveness window, and is the difference between an immediate
                // explanation and a full timeout.
                Subscription::PaneExited {},
            ],
        }),
    };
    let (ack, mut stream) = ApiClient::local()
        .subscribe_value(&subscribe, Some(Duration::from_millis(timeout_ms)))
        .map_err(super::api_client_error_to_io)?;
    if let Err(err) = crate::api::client::parse_response_value(ack) {
        if let ApiClientError::ErrorResponse(response) = err {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        return Err(super::api_client_error_to_io(err));
    }

    if let Some((response, status)) = current_status(target) {
        if status_is_ready(&status) {
            return super::print_response(&response);
        }
    }

    loop {
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            return Ok(refuse(
                "agent_not_ready",
                &format!(
                    "agent {target} did not become ready within {timeout_ms}ms; {}",
                    describe(target, pane_id)
                ),
            ));
        };
        stream.set_read_timeout(Some(remaining))?;

        match stream.next_value() {
            Ok(None) => {
                return Ok(refuse(
                    "agent_not_ready",
                    &format!(
                        "the readiness subscription for agent {target} closed before it became \
                         ready; {}",
                        describe(target, pane_id)
                    ),
                ));
            }
            Ok(Some(event)) => match classify_ready_event(&event, pane_id) {
                ReadySignal::Ready(_) => {
                    // Re-read rather than reshaping the event: the caller gets
                    // the same object `agent get` returns, so one branch reads
                    // both a wait and a poll.
                    return match current_status(target) {
                        Some((response, _)) => super::print_response(&response),
                        None => Ok(refuse(
                            "agent_not_ready",
                            &format!("agent {target} became ready and then disappeared"),
                        )),
                    };
                }
                ReadySignal::Exited => {
                    // Deliberately not `describe`: the pane is reaped in the
                    // same breath as the exit, so a read here answers about
                    // nothing and would report "printed nothing" for a child
                    // that printed its reason on the way out. Say what is
                    // actually known, and point at the door that does keep
                    // the words — a child that dies inside its first 250ms is
                    // refused by the start itself, carrying its exit code and
                    // last line (#178).
                    return Ok(refuse(
                        "agent_exited_before_ready",
                        &format!(
                            "agent {target} exited before it became ready; its pane went with it,                              so there is nothing left to read — run the agent's own command in a                              pane to see why it will not stay up"
                        ),
                    ));
                }
                ReadySignal::KeepWaiting => continue,
            },
            Err(ApiClientError::Io(err)) if super::api_timeout_error(&err) => {
                return Ok(refuse(
                    "agent_not_ready",
                    &format!(
                        "agent {target} did not become ready within {timeout_ms}ms; {}",
                        describe(target, pane_id)
                    ),
                ));
            }
            Err(err) => return Err(super::api_client_error_to_io(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_unknown_is_not_ready() {
        // The whole definition, pinned: `unknown` is flock saying it cannot
        // tell, and every other status is an answer the caller can act on —
        // including `blocked`, which is a result and not a reason to wait
        // longer.
        assert!(!status_is_ready("unknown"));
        assert!(!status_is_ready(""));
        for status in ["idle", "done", "working", "blocked", "hibernated"] {
            assert!(status_is_ready(status), "{status} should count as ready");
        }
    }

    #[test]
    fn a_status_change_on_the_watched_pane_ends_the_wait() {
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "1:p1", "workspace_id": "1", "agent_status": "blocked" },
        });
        assert_eq!(
            classify_ready_event(&event, "1:p1"),
            ReadySignal::Ready("blocked".into())
        );
    }

    #[test]
    fn a_still_unknown_status_is_not_the_signal() {
        // The subscription is unfiltered on purpose, so it also delivers the
        // presentation changes a booting pane produces. Treating any event as
        // readiness would return the instant the title changed.
        let event = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "1:p1", "workspace_id": "1", "agent_status": "unknown" },
        });
        assert_eq!(
            classify_ready_event(&event, "1:p1"),
            ReadySignal::KeepWaiting
        );
    }

    #[test]
    fn another_panes_events_are_ignored() {
        // `pane.exited` has no per-pane filter to subscribe with, so the
        // filtering has to happen here or an unrelated pane closing would be
        // reported as this agent dying.
        let exit = json!({
            "event": "pane_exited",
            "data": { "type": "pane_exited", "pane_id": "2:p1", "workspace_id": "2" },
        });
        assert_eq!(
            classify_ready_event(&exit, "1:p1"),
            ReadySignal::KeepWaiting
        );
        assert_eq!(classify_ready_event(&exit, "2:p1"), ReadySignal::Exited);
    }

    #[test]
    fn an_unrelated_event_kind_keeps_the_wait_open() {
        let event = json!({
            "event": "pane_focused",
            "data": { "type": "pane_focused", "pane_id": "1:p1" },
        });
        assert_eq!(
            classify_ready_event(&event, "1:p1"),
            ReadySignal::KeepWaiting
        );
    }
}
