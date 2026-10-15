//! Delegate guidance from the current socket's CLI registration records.
//! An edge or matching display name alone does not register a CLI delegate.

use serde_json::{json, Value};
use std::path::Path;

/// Add the next-round command only for an unambiguous readable registration.
/// Durable agent identity takes precedence; legacy entries use terminal identity.
/// Missing, unreadable or ambiguous records leave the generic draft remedy intact.
/// This never sends a request or changes refusal classification.
pub(in crate::cli) fn enrich_draft_refusal(response: &mut Value) {
    if response.pointer("/error/code").and_then(Value::as_str)
        != Some(crate::api::error_code::ErrorCode::InputNotEmpty.as_str())
    {
        return;
    }
    let agent = response
        .pointer("/error/data/agent_id")
        .and_then(Value::as_str);
    let terminal = response
        .pointer("/error/data/terminal_id")
        .and_then(Value::as_str);
    let Some(name) = registered_name(&super::registry_dir(), agent, terminal) else {
        return;
    };
    let Some(remedy) = response
        .pointer_mut("/error/data/remedy")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    remedy.insert(
        "next_round".into(),
        json!({
            "argv":["flk","delegate","send",name,"--brief","FILE"],
            "requires":"composer_empty"
        }),
    );
    if let Some(message) = response.pointer_mut("/error/message") {
        if let Some(text) = message.as_str() {
            *message = json!(format!("{text} Once the composer is empty, use flk delegate send {name} --brief FILE for the next task round; delegate send also refuses drafts."));
        }
    }
}

fn registered_name(dir: &Path, agent: Option<&str>, terminal: Option<&str>) -> Option<String> {
    if agent.is_none() && terminal.is_none() {
        return None;
    }
    let mut names = Vec::new();
    for file in std::fs::read_dir(dir).ok()?.flatten() {
        if file
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let Some(entry) = std::fs::read_to_string(file.path())
            .ok()
            .and_then(|body| serde_json::from_str::<super::Entry>(&body).ok())
        else {
            continue;
        };
        let matches = match entry.agent_id.as_deref() {
            Some(id) => agent == Some(id),
            None => terminal == Some(entry.terminal_id.as_str()),
        };
        if matches
            && super::validate_name(&entry.name).is_ok()
            && file.path().file_stem().and_then(|stem| stem.to_str()) == Some(entry.name.as_str())
        {
            names.push(entry.name);
        }
    }
    names.sort();
    names.dedup();
    if names.len() == 1 {
        names.pop()
    } else {
        None
    }
}
