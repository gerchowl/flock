//! `events.read` (ADR-0028 §3, #1028): a pull read of the durable event log
//! from a cursor the consumer holds.
//!
//! The answer is every matching durable event with `seq > after`, from the
//! ring or the rotated files on disk, a `gap` first when the cursor is older
//! than what is retained, and `next_after` to resume from. With `wait_ms`
//! the request is held on the event hub's condvar until a matching event
//! lands, like `msg.wait_reply`, never by a client-side poll.

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorBody, ErrorResponse, EventEnvelope, EventGap, EventsReadParams, ResponseResult,
    SequencedEvent, SuccessResponse, EVENTS_READ_DEFAULT_LIMIT, EVENTS_READ_MAX_LIMIT,
};
use crate::api::server::{should_stop_connection, CONNECTION_POLL_INTERVAL};
use crate::api::EventHub;

fn encode(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"encode failed"}}"#.into()
    })
}

fn refuse(id: String, code: &str, message: String) -> String {
    encode(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
}

fn matches_subject(envelope: &EventEnvelope, subjects: &[String]) -> bool {
    if subjects.is_empty() {
        return true;
    }
    let Ok(serde_json::Value::Object(data)) = serde_json::to_value(&envelope.data) else {
        return false;
    };
    data.values()
        .filter_map(serde_json::Value::as_str)
        .any(|value| subjects.iter().any(|subject| subject == value))
}

/// Answer one `events.read`. `None` when the client hung up while it waited.
pub(super) fn read_events(
    request_id: String,
    params: EventsReadParams,
    stream: &mut UnixStream,
    event_hub: &EventHub,
    running: &Arc<AtomicBool>,
) -> std::io::Result<Option<String>> {
    if let Some(kind) = params.kinds.iter().find(|kind| !kind.is_persisted()) {
        return Ok(Some(refuse(
            request_id,
            "invalid_params",
            format!("{kind:?} is not durable; events.read serves durable kinds only"),
        )));
    }
    if params.after > event_hub.current_sequence() {
        // Suggest the durable head: a later sequence may belong to an
        // ephemeral event that a restart hands out again.
        // `data.head` is the structured form a client resumes from (#1029).
        let head = event_hub.durable_sequence();
        return Ok(Some(encode(&serde_json::json!({
            "id": request_id,
            "error": {
                "code": "cursor_ahead",
                "message": format!(
                    "cursor {} is past this server's newest event {head}; read from 0 or {head}",
                    params.after
                ),
                "data": {"head": head},
            },
        }))));
    }
    let limit = params
        .limit
        .unwrap_or(EVENTS_READ_DEFAULT_LIMIT)
        .clamp(1, EVENTS_READ_MAX_LIMIT) as usize;
    let wait_ms = params.wait_ms.unwrap_or(0);
    let wait = Duration::from_millis(wait_ms.min(event_hub.read_max_wait_ms()));
    let deadline = Instant::now() + wait;
    let keep = |envelope: &EventEnvelope| {
        (params.kinds.is_empty() || params.kinds.contains(&envelope.event))
            && matches_subject(envelope, &params.subjects)
    };
    let mut after = params.after;
    loop {
        let read = event_hub.read(after, limit, keep);
        let found = !read.events.is_empty() || read.gap.is_some();
        if found || Instant::now() >= deadline {
            return Ok(Some(encode(&SuccessResponse {
                id: request_id,
                result: ResponseResult::EventsRead {
                    gap: read.gap.map(|(from, to)| EventGap { from, to }),
                    events: read
                        .events
                        .into_iter()
                        .map(|(seq, envelope)| SequencedEvent { seq, envelope })
                        .collect(),
                    next_after: read.next_after.max(params.after),
                },
            })));
        }
        // Nothing matched up to here: skip it rather than rescan it.
        after = read.scanned;
        if should_stop_connection(stream, running)? {
            return Ok(None);
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(CONNECTION_POLL_INTERVAL);
        event_hub.wait_after(after, slice);
    }
}
