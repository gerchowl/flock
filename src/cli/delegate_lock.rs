//! Explicit, recorded Codex lock choices; no automatic input or re-send.
use super::*;
use crate::api::error_code::ErrorCode;
use crate::api::schema::{CodexLockChoice, PaneResolveCodexLockParams};

pub(super) const NAME: &str = ErrorCode::CodexConversationLocked.as_str();
pub(super) const REFUSAL: &str = "codex_conversation_locked: use flk delegate send <name> --resolve-lock retry or --resolve-lock fork (#1667); no brief was typed";

/// Publish the existing registry JSON durably before permitting an explicit key.
pub(super) fn save(entry: &Entry) -> io::Result<()> {
    write_entry(entry)?;
    std::fs::File::open(registry_dir())?.sync_all()
}

fn screen(entry: &Entry, deadline: Instant) -> io::Result<String> {
    let response = bounded(
        Method::PaneRead(PaneReadParams {
            pane_id: entry.pane_id.clone(),
            source: ReadSource::Detection,
            lines: None,
            format: ReadFormat::Text,
            strip_ansi: true,
        }),
        Some(deadline),
    )
    .map_err(|error| match error {
        BoundedError::TimedOut => io::Error::from(io::ErrorKind::TimedOut),
        BoundedError::Transport(error) => error,
    })?;
    response
        .pointer("/result/read/text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(response.to_string()))
}

/// Attach lock choices only to a refusal that actually diagnoses that lock.
pub(super) fn refuse(code: ErrorCode, message: &str) -> i32 {
    let response = serde_json::json!({"error": {"code": code.as_str(), "message": message,
        "data": (code == ErrorCode::CodexConversationLocked).then(|| serde_json::json!({"startup_dialog": NAME, "choices": ["retry", "fork"]}))}});
    crate::cli::output::error(&response, crate::cli::refusal_exit(code.as_str()))
}

/// Clear an observed obsolete intent without overwriting a concurrent round.
pub(super) fn clear_observed(entry: &mut Entry) -> io::Result<Option<i32>> {
    let _guard = match take_lock(&entry.name) {
        Ok(guard) => guard,
        Err("busy") => {
            return Ok(Some(refuse(
                ErrorCode::InjectionPending,
                "A delegate write is pending; lock metadata was not changed",
            )))
        }
        Err(reason) => return Err(io::Error::other(reason)),
    };
    if let Some(mut fresh) = read_entry(&entry.name) {
        if fresh.round == entry.round && fresh.lock_resolution == entry.lock_resolution {
            fresh.lock_resolution = None;
            save(&fresh)?;
        }
    }
    entry.lock_resolution = None;
    Ok(None)
}

/// Report an unsubmitted start while retaining its durably recorded pane.
pub(super) fn blocked(entry: &Entry, json: bool) -> i32 {
    emit_outcome(
        entry,
        Outcome::AgentBlocked.as_str(),
        Some(&serde_json::json!({"startup_dialog": NAME, "status_text": REFUSAL})),
        json,
        &entry.cursor,
    );
    crate::cli::exit::AGENT_BLOCKED
}

/// Record one explicit intent, admit one guarded key and bound confirmation.
pub(super) fn resolve(name: &str, choice: CodexLockChoice, flags: &SendFlags) -> io::Result<i32> {
    let _lock = match take_lock(name) {
        Ok(lock) => lock,
        Err(reason) => return Ok(fail(format!("delegate {name}: {reason}"))),
    };
    let mut entry = match require_delegate_no_deadline(name) {
        Ok(Recorded::Live(entry)) => entry,
        Ok(Recorded::Gone(entry)) => return Ok(emit_submit_gone(&entry, flags.json)),
        Err(code) => return Ok(code),
    };
    let deadline = Instant::now()
        + Duration::from_millis(
            flags
                .ready_timeout_ms
                .unwrap_or(super::super::ready::DEFAULT_READY_TIMEOUT_MS),
        );
    // Subscribe before snapshot and key: a fast repaint cannot fall in a gap.
    let request = Request {
        id: "cli:delegate:lock".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            after: None,
            subscriptions: vec![
                Subscription::PaneAgentStatusChanged {
                    pane_id: entry.pane_id.clone(),
                    agent_status: None,
                },
                Subscription::PaneExited {},
            ],
        }),
    };
    let (ack, mut stream) = ApiClient::local()
        .subscribe_value(
            &request,
            Some(deadline.saturating_duration_since(Instant::now())),
        )
        .map_err(crate::cli::api_client_error_to_io)?;
    crate::api::client::parse_response_value(ack).map_err(crate::cli::api_client_error_to_io)?;
    if harness_for(&entry).name != "codex" {
        return Ok(refuse(
            ErrorCode::AgentNotReady,
            "This delegate is not Codex; no key sent",
        ));
    }
    if !crate::detect::codex_conversation_locked(&screen(&entry, deadline)?) {
        if entry.lock_resolution.take().is_some() {
            save(&entry)?;
        }
        return Ok(refuse(
            ErrorCode::AgentNotReady,
            "codex_conversation_locked is not currently detected; no key sent",
        ));
    }
    entry.lock_resolution = Some(serde_json::json!({
        "choice": choice, "chosen_at_ms": now_ms(), "confirmed": false,
    }));
    save(&entry)?;
    let response = match bounded(
        Method::PaneResolveCodexLock(PaneResolveCodexLockParams {
            pane_id: entry.pane_id.clone(),
            choice,
            write_timeout_ms: Some(serde_json::json!(deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 10000) as u64)),
        }),
        Some(deadline),
    ) {
        Ok(response) => response,
        Err(error) => {
            return Ok(refuse(
                ErrorCode::CodexLockUnconfirmed,
                &format!("lock key delivery unconfirmed: {error:?}; inspect before choosing again"),
            ))
        }
    };
    if response.get("error").is_some() {
        return Ok(crate::cli::output::error(
            &response,
            crate::cli::error_exit(&response, true),
        ));
    }
    loop {
        let current = match screen(&entry, deadline) {
            Ok(current) => current,
            Err(error) => return Ok(refuse(ErrorCode::CodexLockUnconfirmed,
                &format!("could not confirm the lock screen changed: {error}; inspect before choosing again"))),
        };
        if !crate::detect::codex_conversation_locked(&current) {
            entry.lock_resolution = None;
            save(&entry)?;
            emit_outcome(
                &entry,
                "lock_resolved",
                Some(
                    &serde_json::json!({"text": "The pane left codex_conversation_locked; no brief was submitted"}),
                ),
                flags.json,
                &entry.cursor,
            );
            return Ok(crate::cli::exit::SUCCESS);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Status changes wake promptly; bounded condition polling also covers
        // a lock replaced by another blocked dialog without a status change.
        stream.set_read_timeout(Some(remaining.min(READY_POLL)))?;
        match stream.next_value() {
            Ok(Some(_)) => {}
            Err(ApiClientError::Io(error)) if crate::cli::api_timeout_error(&error) => {}
            other => {
                return Ok(refuse(
                    ErrorCode::CodexLockUnconfirmed,
                    &format!(
                        "lock confirmation stream ended: {other:?}; inspect before choosing again"
                    ),
                ))
            }
        }
    }
    Ok(refuse(ErrorCode::CodexLockUnconfirmed, "The key was accepted but leaving codex_conversation_locked was not confirmed; inspect the pane before choosing again"))
}
