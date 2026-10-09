//! `msg.wait_reply` (#576): the sender's half of a `needs_reply` round trip.
//!
//! A sender used to get a correlation id back and then had to build its own
//! watcher over `msg.status` to learn that an answer had landed. This holds
//! the request instead, on the socket thread like `pane.wait_for_output`, but
//! woken by the event hub's condvar (ADR-0019 §5) rather than by a poll: the
//! answer is an event this server already records, so waiting for it is a
//! matter of watching the log.
//!
//! What counts as the answer is read from the log alone, which is what makes
//! it work for a sender with no inbox (an ssh shell, a script):
//!
//! - a `MessageQueued` whose `in_reply_to` is the message: a reply delivered
//!   to an inbox on this server, or one that came back from another host as a
//!   `msg.send`. A deferral id (`<id>:deferred`) is a muted recipient's
//!   automatic answer (ADR-0018 §3);
//! - a `MessageReplied` that carries a body: the reply was HELD here because
//!   the original sender had no inbox to deliver it to;
//! - a `MessageDelivered` with `dropped_undeliverable`: nobody read it in
//!   time, so no answer is coming.

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventData, EventEnvelope, MsgReplyInfo, MsgWaitReplyParams,
    ResponseResult, SuccessResponse, MSG_WAIT_REPLY_MAX_MS,
};
use crate::api::server::{should_stop_connection, CONNECTION_POLL_INTERVAL};
use crate::api::EventHub;

/// The end of a message's round trip, as its sender sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Answer {
    Replied(MsgReplyInfo),
    Deferred(MsgReplyInfo),
    Expired,
    RetentionElapsed,
    RecipientGone,
    /// The receiver refused custody; the reason, when it gave one.
    Refused(Option<String>),
}

impl Answer {
    pub(crate) fn outcome(&self) -> &'static str {
        match self {
            Self::Replied(_) => "replied",
            Self::Deferred(_) => "deferred",
            Self::Expired => "expired",
            Self::RetentionElapsed => "outcome_retention_elapsed",
            Self::RecipientGone => "recipient_gone",
            Self::Refused(_) => "refused",
        }
    }

    /// Which answer wins when a message has several. A real reply outranks
    /// a mute's deferral: a sender whose question was deferred and later
    /// answered must be shown the answer, not the deferral, on every read
    /// after that — for a sender with no inbox the held reply is the only
    /// copy there is. Among equals the first stands.
    fn rank(&self) -> u8 {
        match self {
            Self::Replied(_) => 3,
            Self::Deferred(_) => 2,
            Self::Expired | Self::RetentionElapsed | Self::RecipientGone | Self::Refused(_) => 1,
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Refused(reason) => reason.clone(),
            _ => None,
        }
    }

    pub(crate) fn into_reply(self) -> Option<MsgReplyInfo> {
        match self {
            Self::Replied(reply) | Self::Deferred(reply) => Some(reply),
            Self::Expired | Self::RetentionElapsed | Self::RecipientGone | Self::Refused(_) => None,
        }
    }
}

fn answer_kind(reply_correlation_id: &str, reply: MsgReplyInfo) -> Answer {
    if crate::app::mailboxes::is_deferral(reply_correlation_id) {
        Answer::Deferred(MsgReplyInfo {
            kind: "deferral".into(),
            ..reply
        })
    } else {
        Answer::Replied(reply)
    }
}

/// The answer to `correlation_id` this one event carries, if any.
pub(crate) fn answer_in(event: &EventEnvelope, correlation_id: &str) -> Option<Answer> {
    match &event.data {
        EventData::MessageQueued {
            message_key,
            correlation_id: reply_id,
            in_reply_to: Some(original),
            from_pane,
            from_agent,
            from_host,
            body,
            to_pane,
            ..
        } if original == correlation_id => Some(answer_kind(
            reply_id,
            MsgReplyInfo {
                correlation_id: reply_id.clone(),
                kind: "reply".into(),
                body: crate::mesh::delivery::body(message_key.as_ref(), body)?,
                from_agent: from_agent.clone(),
                from_pane: from_pane.clone(),
                from_host: from_host.clone(),
                held: to_pane.is_empty(),
            },
        )),
        EventData::MessageReplied {
            correlation_id: original,
            reply_correlation_id,
            body: Some(body),
            from_pane,
            from_agent,
            held,
            ..
        } if original == correlation_id => Some(answer_kind(
            reply_correlation_id,
            MsgReplyInfo {
                correlation_id: reply_correlation_id.clone(),
                kind: "reply".into(),
                body: body.clone(),
                from_agent: from_agent.clone(),
                from_pane: from_pane.clone(),
                from_host: Some(crate::app::short_host_name()),
                held: *held,
            },
        )),
        EventData::MessageDelivered {
            correlation_id: original,
            delivered: false,
            outcome,
            ..
        } if original == correlation_id && outcome == "dropped_undeliverable" => {
            Some(Answer::Expired)
        }
        _ => None,
    }
}

/// The answer to `correlation_id` in `events` (oldest first), by
/// [`Answer::rank`]: the first real reply, else the first deferral, else
/// expiry.
pub(crate) fn best_answer<'a>(
    events: impl IntoIterator<Item = &'a EventEnvelope>,
    correlation_id: &str,
) -> Option<Answer> {
    let mut watch = Watch::default();
    for event in events {
        watch.observe(event, correlation_id);
    }
    watch.answer
}

/// The message's own delivery state as `msg.status` names it, when this
/// event moves it: what a timed-out waiter reports, so "read but
/// unanswered" is distinguishable from "never picked up".
fn delivery_state(event: &EventEnvelope, correlation_id: &str) -> Option<&'static str> {
    match &event.data {
        EventData::MessageQueued {
            correlation_id: id, ..
        } if id == correlation_id => Some("queued"),
        EventData::MessageRelayed {
            correlation_id: id, ..
        } if id == correlation_id => Some("relayed"),
        EventData::MessageDelivered {
            correlation_id: id,
            delivered,
            ..
        } if id == correlation_id => Some(if *delivered { "read" } else { "dropped" }),
        _ => None,
    }
}

/// What a scan of the log knows about one message so far.
#[derive(Debug, Default)]
struct Watch {
    state: Option<String>,
    answer: Option<Answer>,
}

impl Watch {
    fn refresh_mesh(
        &mut self,
        origin: Option<&str>,
        correlation: &str,
        reference: Option<&crate::mesh::store::StatusReference>,
    ) -> Result<(), String> {
        if let Some(status) = crate::mesh::runtime_store::status(origin, correlation, reference)? {
            let durable = status
                .reply
                .map(|reply| {
                    if reply.kind == "deferral" {
                        Answer::Deferred(reply)
                    } else {
                        Answer::Replied(reply)
                    }
                })
                .or(match status.state.as_str() {
                    "expired" => Some(Answer::Expired),
                    "outcome_retention_elapsed" => Some(Answer::RetentionElapsed),
                    "recipient_gone" => Some(Answer::RecipientGone),
                    "refused" => Some(Answer::Refused(status.detail.clone())),
                    _ => None,
                });
            // Merge by rank, as `observe` does: a local answer the mesh
            // writer could not record still beats a durable expiry.
            if let Some(found) = durable {
                if self
                    .answer
                    .as_ref()
                    .is_none_or(|held| found.rank() > held.rank())
                {
                    self.answer = Some(found);
                }
            }
            self.state = Some(status.state);
        }
        Ok(())
    }

    fn observe(&mut self, event: &EventEnvelope, correlation_id: &str) {
        if let Some(state) = delivery_state(event, correlation_id) {
            self.state = Some(state.into());
        }
        if let Some(found) = answer_in(event, correlation_id) {
            if self
                .answer
                .as_ref()
                .is_none_or(|held| found.rank() > held.rank())
            {
                self.answer = Some(found);
            }
        }
    }
}

fn encode(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"encode failed"}}"#.into()
    })
}

fn awaited(
    request_id: String,
    correlation_id: String,
    outcome: &str,
    reply: Option<MsgReplyInfo>,
    detail: Option<String>,
    state: Option<&str>,
) -> String {
    encode(&SuccessResponse {
        id: request_id,
        result: ResponseResult::MsgReplyAwaited {
            correlation_id,
            outcome: outcome.into(),
            reply,
            detail,
            state: state.map(str::to_string),
        },
    })
}

/// Hold the request until `params.correlation_id` is answered, deferred or
/// expired, or the timeout passes. `None` when the client hung up first.
///
/// The cursor is taken BEFORE the history is read, so an answer that lands
/// between the two is seen twice rather than not at all; seeing it twice is
/// harmless, the first sighting ends the wait.
pub(super) fn wait_for_reply(
    request_id: String,
    params: MsgWaitReplyParams,
    origin: Option<&str>,
    stream: &mut UnixStream,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    let correlation_id = params.correlation_id;
    let timeout_ms = params
        .timeout_ms
        .unwrap_or(MSG_WAIT_REPLY_MAX_MS)
        .min(MSG_WAIT_REPLY_MAX_MS);
    crate::logging::msg_wait_reply_started(&request_id, &correlation_id, timeout_ms);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    let mut cursor = event_hub.current_sequence();
    let mut watch = Watch::default();
    // The in-memory ring only, as `msg.status` reads it, so the two never
    // disagree. NOT the durable log: `persisted_events_after` re-reads every
    // log file while holding the hub's lock, and every `emit_event` on the
    // app thread waits on that lock — a retried typo would stall the server.
    for (_, event) in event_hub.events_after(0) {
        watch.observe(&event, &correlation_id);
    }
    if let Err(reason) = watch.refresh_mesh(origin, &correlation_id, params.reference.as_ref()) {
        if params.reference.is_some() {
            return Ok(Some(encode(&ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "mail_store_unavailable".into(),
                    message: reason,
                },
            })));
        }
        // Without a durable reference the event ring still answers for
        // pre-mesh and local messages, so a store outage degrades, not fails.
        crate::logging::mesh_custody_failed("wait_reply", "mail_store_unavailable");
    }
    if watch.state.is_none() && watch.answer.is_none() {
        crate::logging::msg_wait_reply_completed(&request_id, &correlation_id, "not_found");
        return Ok(Some(encode(&ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "message_not_found".into(),
                message: format!("no message with correlation id {correlation_id}"),
            },
        })));
    }

    let _waiting = event_hub.track_reply_wait(&correlation_id);
    loop {
        // One indexed read per wake, bounded by CONNECTION_POLL_INTERVAL. A
        // transient outage (a handoff suspends the writer) keeps the wait
        // alive on the event ring rather than ending it. It is not logged:
        // at this cadence one outage would flood the log.
        let _ = watch.refresh_mesh(origin, &correlation_id, params.reference.as_ref());
        if let Some(answer) = watch.answer.take() {
            let outcome = answer.outcome();
            crate::logging::msg_wait_reply_completed(&request_id, &correlation_id, outcome);
            let detail = answer.detail();
            return Ok(Some(awaited(
                request_id,
                correlation_id,
                outcome,
                answer.into_reply(),
                detail,
                watch.state.as_deref(),
            )));
        }
        if Instant::now() >= deadline {
            crate::logging::msg_wait_reply_completed(&request_id, &correlation_id, "timeout");
            return Ok(Some(awaited(
                request_id,
                correlation_id,
                "timeout",
                None,
                None,
                watch.state.as_deref(),
            )));
        }
        if should_stop_connection(stream, running)? {
            crate::logging::msg_wait_reply_completed(
                &request_id,
                &correlation_id,
                "client_disconnected",
            );
            return Ok(None);
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(CONNECTION_POLL_INTERVAL);
        if event_hub.wait_after(cursor, slice) {
            let fresh = event_hub.events_after(cursor);
            // More than the ring holds arrived between two wakes: the events
            // just past the cursor are gone. Re-read what the ring still has
            // rather than miss an answer among them silently.
            let gap = fresh
                .first()
                .is_some_and(|(sequence, _)| *sequence > cursor + 1);
            let batch = if gap {
                event_hub.events_after(0)
            } else {
                fresh
            };
            for (sequence, event) in batch {
                cursor = cursor.max(sequence);
                watch.observe(&event, &correlation_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventKind, MsgIntent};

    fn queued(id: &str, in_reply_to: Option<&str>, body: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageQueued,
            data: EventData::MessageQueued {
                message_key: None,
                correlation_id: id.into(),
                from_pane: Some("w1:p1".into()),
                from_agent: Some("agent_example_1".into()),
                from_host: Some("host".into()),
                from_repo: None,
                to_pane: "w2:p1".into(),
                to_repo: None,
                cross_repo: false,
                in_reply_to: in_reply_to.map(str::to_string),
                enqueued_at_ms: 1,
                intent: MsgIntent::NeedsReply,
                body: body.into(),
            },
        }
    }

    fn delivered(id: &str, delivered: bool, outcome: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageDelivered,
            data: EventData::MessageDelivered {
                correlation_id: id.into(),
                delivered,
                outcome: outcome.into(),
                delivery_attempts: 1,
                latency_ms: 1,
            },
        }
    }

    fn held(id: &str, reply: &str, body: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::MessageReplied,
            data: EventData::MessageReplied {
                correlation_id: id.into(),
                reply_correlation_id: reply.into(),
                reply_latency_ms: 1,
                round_trips: 1,
                body: Some(body.into()),
                from_pane: Some("w2:p1".into()),
                from_agent: None,
                held: true,
            },
        }
    }

    #[test]
    fn a_reply_delivered_to_an_inbox_is_the_answer() {
        let events = [
            queued("q", None, "question"),
            delivered("q", true, "read"),
            queued("r", Some("q"), "the answer"),
        ];
        let Some(Answer::Replied(reply)) = best_answer(&events, "q") else {
            panic!("expected a reply");
        };
        assert_eq!(
            (reply.correlation_id.as_str(), reply.body.as_str()),
            ("r", "the answer")
        );
        assert!(!reply.held);
    }

    #[test]
    fn a_held_reply_is_the_answer_for_a_sender_without_an_inbox() {
        let events = [queued("q", None, "question"), held("q", "r", "held answer")];
        let Some(Answer::Replied(reply)) = best_answer(&events, "q") else {
            panic!("expected a held reply");
        };
        assert_eq!(reply.body, "held answer");
        assert!(reply.held);
    }

    #[test]
    fn a_deferral_is_its_own_outcome() {
        let deferral = crate::app::mailboxes::deferral_id("q");
        let events = [
            queued("q", None, "question"),
            queued(&deferral, Some("q"), "muted until 12:00"),
        ];
        let answer = best_answer(&events, "q").expect("deferred");
        assert_eq!(answer.outcome(), "deferred");
        assert_eq!(
            answer.into_reply().map(|reply| reply.kind),
            Some("deferral".into())
        );
    }

    #[test]
    fn a_reply_after_a_deferral_wins_on_every_later_read() {
        // Review finding: a muted recipient defers an anonymous question at
        // once, then unmutes and answers. "First answer wins" showed the
        // deferral forever, and the held reply — the sender's only copy —
        // was visible nowhere.
        let deferral = crate::app::mailboxes::deferral_id("q");
        let events = [
            queued("q", None, "question"),
            held("q", &deferral, "muted until 12:00"),
            held("q", "r", "the real answer"),
            queued("r2", Some("q"), "a second reply"),
        ];
        let Some(Answer::Replied(reply)) = best_answer(&events, "q") else {
            panic!("the real reply must win");
        };
        assert_eq!(reply.body, "the real answer", "the first real reply stands");
    }

    #[test]
    fn dropped_unread_is_expired_and_a_plain_read_is_no_answer() {
        assert_eq!(
            best_answer(&[delivered("q", false, "dropped_undeliverable")], "q"),
            Some(Answer::Expired)
        );
        assert_eq!(best_answer(&[delivered("q", true, "read")], "q"), None);
    }

    #[test]
    fn another_messages_reply_is_not_this_ones() {
        let events = [
            queued("r", Some("other"), "not yours"),
            held("other", "r2", "x"),
        ];
        assert_eq!(best_answer(&events, "q"), None);
    }

    #[test]
    fn a_reply_delivered_without_a_body_in_its_replied_event_does_not_count_twice() {
        // An inbox-delivered reply records its body in the reply's own
        // MessageQueued; its MessageReplied carries none and is not an answer.
        let mut replied = held("q", "r", "x");
        if let EventData::MessageReplied { body, held, .. } = &mut replied.data {
            *body = None;
            *held = false;
        }
        assert_eq!(best_answer(&[replied], "q"), None);
    }

    #[test]
    fn the_watch_keeps_the_last_delivery_state_for_a_timeout() {
        let mut watch = Watch::default();
        for event in [queued("q", None, "x"), delivered("q", true, "read")] {
            watch.observe(&event, "q");
        }
        assert_eq!(watch.state.as_deref(), Some("read"));
        assert!(watch.answer.is_none());
    }
}
