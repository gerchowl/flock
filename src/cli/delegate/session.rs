use super::*;

pub(super) fn changed(entry: &Entry) -> bool {
    Cursor::parse(&entry.cursor).is_ok_and(|cursor| cursor.terminal_id != entry.terminal_id)
}

pub(super) fn prepare(entry: &mut Entry, deadline: Option<Instant>) -> Result<(), String> {
    let before = entry.clone();
    let target = entry
        .agent_id
        .as_deref()
        .ok_or("no stable agent identity was recorded")?;
    let record = match agent_record(target, deadline) {
        AgentFetch::Found(record) => record,
        _ => return Err("the current agent record could not be verified".into()),
    };
    if field(&record, "agent_id") != Some(target) {
        return Err("the current record does not match the stable agent identity".into());
    }
    let session = record
        .get("agent_session")
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| entry.session.clone())
        .and_then(|raw| serde_json::from_value::<crate::api::schema::AgentSessionInfo>(raw).ok())
        .ok_or("no harness-reported session id was recorded")?;
    if session.agent != entry.harness || crate::session_evidence::path(&session).is_none() {
        return Err("the recorded session id has no unique matching harness session".into());
    }
    if record
        .get("agent_session")
        .is_none_or(|value| value.is_null())
    {
        let response = bounded(
            Method::PaneReportAgentSession(crate::api::schema::PaneReportAgentSessionParams {
                rebind_agent_id: entry.agent_id.clone(),
                pane_id: entry.pane_id.clone(),
                source: session.source.clone(),
                agent: session.agent.clone(),
                seq: None,
                agent_session_id: Some(session.value.clone()),
                agent_session_path: None,
                session_start_source: None,
            }),
            deadline,
        )
        .map_err(|err| err.to_string())?;
        if let Some(error) = response.get("error") {
            return Err(server_error(error));
        }
        let AgentFetch::Found(rebound) = agent_record(target, deadline) else {
            return Err("the rebound session could not be verified".into());
        };
        if rebound.get("agent_session")
            != Some(&serde_json::to_value(&session).map_err(|err| err.to_string())?)
        {
            return Err("the current agent did not accept the recorded session".into());
        }
    }
    entry.session = serde_json::to_value(session).ok();
    persist_reidentified(&before, entry);
    Ok(())
}

pub(super) fn unbound(entry: &Entry, reason: &str, json: bool) -> i32 {
    let message = format!("session_unbound: delegate {} (agent {}, terminal {}): {reason}; inspect with `flk pane read {}`",
        entry.name, entry.agent_id.as_deref().unwrap_or("unknown"), entry.terminal_id, entry.pane_id);
    eprintln!("{message}");
    emit_outcome(
        entry,
        "session_unbound",
        Some(
            &serde_json::json!({"error":{"code":"session_unbound","message":message},
        "agent_id":entry.agent_id,"terminal_id":entry.terminal_id,"pane_id":entry.pane_id}),
        ),
        json,
        &entry.cursor,
    );
    crate::cli::exit::SESSION_UNBOUND
}
