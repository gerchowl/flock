//! The MCP resource surface: handed-over files (#286, ADR-0017).
//!
//! Resources are a different primitive from tools, and the difference is the
//! whole reason this module exists rather than a fifteenth entry in
//! [`super::tools`]. A tool is a parameterised call — `flock_agent_history`
//! takes a detail level, a cursor and a limit, and no URI can stand for that.
//! A resource is content the server already holds, that it can enumerate and
//! hand back by a stable identifier. A file a human dropped on a pane is
//! exactly that, and nothing else on flock's MCP surface is.
//!
//! Two rules the mapping keeps:
//!
//! * **Every listed resource is readable.** The server-side projection has
//!   already dropped records whose bytes are gone, so `resources/list` never
//!   advertises a URI that `resources/read` will refuse.
//! * **The list is not filtered by who is asking.** An MCP client that cannot
//!   be tied to a pane would otherwise get an empty list, which reads as
//!   "nothing was handed over" — the one answer that must never be a guess.
//!   Each row says who it was handed to instead.

use serde_json::{json, Value};

use super::framing::McpError;

/// The URI scheme resources are addressed by. `flock://handoff/<file_id>` —
/// scoped to this host, like the id itself.
pub(super) const URI_PREFIX: &str = "flock://handoff/";

/// Map a `handoff.list` result onto MCP's `resources/list` shape.
pub(super) fn list_result(flock: &Value) -> Value {
    let resources: Vec<Value> = flock
        .get("files")
        .and_then(Value::as_array)
        .map(|files| files.iter().map(descriptor).collect())
        .unwrap_or_default();
    json!({ "resources": resources })
}

/// One `Resource` descriptor. `name` is the staged file's own name; the
/// description carries what a listing cannot show — who it was handed to, and
/// the server-side path for a caller that would rather read it itself.
fn descriptor(file: &Value) -> Value {
    let file_id = file.get("file_id").and_then(Value::as_str).unwrap_or("");
    let name = file.get("name").and_then(Value::as_str).unwrap_or(file_id);
    let mime = file
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let bytes = file.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    let path = file.get("path").and_then(Value::as_str).unwrap_or("");
    let handed_to = file
        .get("agent_id")
        .and_then(Value::as_str)
        .or_else(|| file.get("pane_id").and_then(Value::as_str))
        .unwrap_or("nobody in particular");
    json!({
        "uri": format!("{URI_PREFIX}{file_id}"),
        "name": name,
        "description": format!(
            "file handed to {handed_to}; staged at {path} on this host"
        ),
        "mimeType": mime,
        "size": bytes,
    })
}

/// Pull the `file_id` out of a `resources/read` request's `uri` param.
pub(super) fn file_id_from_params(params: &Value) -> Result<String, McpError> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::invalid_params("resources/read missing `uri`"))?;
    let file_id = uri.strip_prefix(URI_PREFIX).unwrap_or("").trim();
    if file_id.is_empty() {
        return Err(McpError::invalid_params(format!(
            "`{uri}` is not a flock resource uri; they look like {URI_PREFIX}<file_id>"
        )));
    }
    Ok(file_id.to_string())
}

/// Map a `handoff.read` result onto MCP's `resources/read` shape.
///
/// MCP splits the payload by field, not by a flag: `text` for text, `blob`
/// for base64. flock's own `encoding` says which, so the mapping is a rename
/// rather than a re-decision.
pub(super) fn read_result(flock: &Value) -> Value {
    let file_id = flock.get("file_id").and_then(Value::as_str).unwrap_or("");
    let mime = flock
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let content = flock
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut item = json!({
        "uri": format!("{URI_PREFIX}{file_id}"),
        "mimeType": mime,
    });
    let field = match flock.get("encoding").and_then(Value::as_str) {
        Some("utf8") => "text",
        _ => "blob",
    };
    if let Some(object) = item.as_object_mut() {
        object.insert(field.to_string(), json!(content));
    }
    json!({ "contents": [item] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_file() -> Value {
        json!({
            "files": [{
                "file_id": "file:abc:0",
                "name": "client-1-clipboard-9-0.pdf",
                "mime": "application/pdf",
                "bytes": 12,
                "path": "/tmp/flock-clipboard-images-501/client-1-clipboard-9-0.pdf",
                "pane_id": "ws_1:p1",
                "agent_id": "agent_host_abc",
                "origin_host": "host",
                "received_at_ms": 7
            }],
            "total": 1
        })
    }

    #[test]
    fn a_handoff_becomes_a_readable_resource_descriptor() {
        let listed = list_result(&one_file());
        let resource = &listed["resources"][0];
        assert_eq!(resource["uri"], "flock://handoff/file:abc:0");
        assert_eq!(resource["name"], "client-1-clipboard-9-0.pdf");
        assert_eq!(resource["mimeType"], "application/pdf");
        assert_eq!(resource["size"], 12);
        assert!(
            resource["description"]
                .as_str()
                .is_some_and(|d| d.contains("agent_host_abc")),
            "the row has to say who it was handed to: {resource}"
        );
    }

    #[test]
    fn an_empty_list_is_an_empty_array_not_a_missing_field() {
        // A client that gets `resources: null` cannot tell an empty flock
        // from a broken one.
        let listed = list_result(&json!({ "files": [], "total": 0 }));
        assert_eq!(listed["resources"], json!([]));
    }

    #[test]
    fn the_uri_round_trips_back_to_the_file_id() {
        let listed = list_result(&one_file());
        let uri = listed["resources"][0]["uri"].clone();
        assert_eq!(
            file_id_from_params(&json!({ "uri": uri })).unwrap(),
            "file:abc:0"
        );
    }

    #[test]
    fn a_foreign_uri_is_invalid_params_not_a_lookup() {
        // `file:///etc/passwd` must never reach the server as a file id.
        let err = file_id_from_params(&json!({ "uri": "file:///etc/passwd" })).unwrap_err();
        assert_eq!(err.code, -32602);
        let err = file_id_from_params(&json!({})).unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn text_lands_in_text_and_bytes_land_in_blob() {
        let text = read_result(&json!({
            "file_id": "file:a:0",
            "mime": "text/markdown",
            "encoding": "utf8",
            "content": "# hello"
        }));
        assert_eq!(text["contents"][0]["text"], "# hello");
        assert!(text["contents"][0].get("blob").is_none());

        let blob = read_result(&json!({
            "file_id": "file:a:0",
            "mime": "application/pdf",
            "encoding": "base64",
            "content": "JVBERi0="
        }));
        assert_eq!(blob["contents"][0]["blob"], "JVBERi0=");
        assert!(blob["contents"][0].get("text").is_none());
        assert_eq!(blob["contents"][0]["uri"], "flock://handoff/file:a:0");
    }
}
