//! Legacy identity repair uses saved native sessions, never terminal equality.
use super::*;

pub(in crate::cli) const USAGE: &str = "flk delegate rebind <name> --agent <id> [--json]";

fn unbound(entry: Entry, reason: &'static str) -> RequireFailure {
    RequireFailure::SessionUnbound(Box::new(entry), reason)
}

fn session_key(session: &serde_json::Value) -> Option<(&str, &str)> {
    let harness = field(session, "agent")?;
    let id = field(session, "value").filter(|id| !id.is_empty())?;
    (field(session, "kind") == Some("id")).then_some((harness, id))
}

fn bind(mut entry: Entry, record: &serde_json::Value) -> Result<Entry, RequireFailure> {
    let Some((agent, terminal, pane)) = field(record, "agent_id")
        .filter(|id| !id.is_empty())
        .zip(field(record, "terminal_id").filter(|id| !id.is_empty()))
        .zip(field(record, "pane_id").filter(|id| !id.is_empty()))
        .map(|((agent, terminal), pane)| (agent, terminal, pane))
    else {
        return Err(unbound(entry, "live_binding_incomplete"));
    };
    entry.agent_id = Some(agent.into());
    entry.terminal_id = terminal.into();
    entry.pane_id = pane.into();
    Ok(entry)
}

pub(super) fn upgrade(entry: Entry, deadline: Option<Instant>) -> Result<Entry, RequireFailure> {
    let Some(key) = entry
        .session
        .as_ref()
        .and_then(session_key)
        .filter(|(harness, _)| *harness == entry.harness)
    else {
        return Err(unbound(entry, "saved_session_missing"));
    };
    let response =
        bounded(Method::AgentList(EmptyParams {}), deadline).map_err(|err| match err {
            BoundedError::TimedOut => RequireFailure::TimedOut(Box::new(entry.clone())),
            err => RequireFailure::Failed(err.to_string()),
        })?;
    if let Some(error) = response.get("error") {
        return Err(RequireFailure::Failed(server_error(error)));
    }
    let agents = response
        .pointer("/result/agents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| RequireFailure::Failed("malformed agent.list answer".into()))?;
    let matches: Vec<_> = agents
        .iter()
        .filter(|record| record.get("agent_session").and_then(session_key) == Some(key))
        .collect();
    // A structured claim also makes a PTY session non-unique. This registry
    // controls PTY delegates and cannot invent a terminal for a native runtime.
    let native_claims = response
        .pointer("/result/structured_agents")
        .and_then(serde_json::Value::as_array)
        .map_or(0, |agents| {
            agents
                .iter()
                .filter(|agent| {
                    field(agent, "harness") == Some(key.0)
                        && field(agent, "session_id") == Some(key.1)
                })
                .count()
        });
    let record = match (matches.as_slice(), native_claims) {
        ([record], 0) => *record,
        ([], 0) => return Err(unbound(entry, "saved_session_not_found")),
        ([], 1) => return Err(unbound(entry, "saved_session_runtime_unsupported")),
        _ => return Err(unbound(entry, "saved_session_ambiguous")),
    };
    let live = bind(entry, record)?;
    write_entry(&live).map_err(|err| RequireFailure::Failed(err.to_string()))?;
    tracing::info!(
        event = "delegate.identity.backfilled",
        delegate = live.name.as_str(),
        agent_id = live.agent_id.as_deref(),
        "backfilled delegate identity from unique saved session"
    );
    Ok(live)
}

pub(super) fn rebind(args: &[String]) -> io::Result<i32> {
    let json = super::super::output::json() || args.iter().any(|arg| arg == "--json");
    let words: Vec<_> = args.iter().filter(|arg| arg.as_str() != "--json").collect();
    let [name, flag, agent] = words.as_slice() else {
        return Ok(usage(USAGE));
    };
    if flag.as_str() != "--agent" || validate_name(name).is_err() || agent.is_empty() {
        return Ok(usage(USAGE));
    }
    let _lock = match take_lock(name) {
        Ok(lock) => lock,
        Err(reason) => return Ok(fail(reason)),
    };
    let Some(entry) = read_entry(name) else {
        return Ok(usage("not a delegate"));
    };
    let response = match bounded(
        Method::DelegateRebind(crate::api::schema::DelegateRebindParams {
            name: entry.name.clone(),
            agent_id: agent.to_string(),
            previous_agent_id: entry.agent_id.clone(),
        }),
        None,
    ) {
        Ok(response) => response,
        Err(err) => return Ok(fail(err)),
    };
    if response.get("error").is_some() {
        return super::super::print_response(&response);
    }
    let Some(record) = response.pointer("/result/agent") else {
        return Ok(fail("malformed delegate.rebind answer"));
    };
    if field(record, "agent_id") != Some(agent.as_str()) {
        return Ok(fail("delegate.rebind returned a different agent"));
    }
    let mut live = match bind(entry, record) {
        Ok(live) => live,
        Err(RequireFailure::SessionUnbound(entry, reason)) => {
            return Ok(session::unbound(&entry, reason, json))
        }
        Err(_) => return Ok(fail("invalid live binding")),
    };
    live.harness = record
        .get("agent_session")
        .and_then(session_key)
        .map(|key| key.0)
        .or_else(|| field(record, "agent"))
        .unwrap_or(&live.harness)
        .to_string();
    live.session = record
        .get("agent_session")
        .filter(|s| !s.is_null())
        .cloned();
    // Explicit rebinding starts observation at the chosen execution. The saved
    // round/submission remains intact, so result freshness checks still apply.
    live.cursor = field(record, "turn_cursor").unwrap_or_default().into();
    write_entry(&live)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"name":live.name,"agent_id":live.agent_id,"outcome":"rebound"})
        );
    } else {
        println!("delegate {} rebound", live.name);
    }
    Ok(exit::OK)
}
