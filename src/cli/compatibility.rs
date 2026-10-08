use std::io;
use std::time::Duration;

use crate::api::schema::{Method, PingParams, Request};

// Turn cursors and agent.result first shipped together in v0.9.0.
const MIN_TURN_SERVER: crate::update::Version = crate::update::Version {
    major: 0,
    minor: 9,
    patch: 0,
};

fn running_version() -> io::Result<Option<String>> {
    let response = super::ApiClient::local()
        .request_value_with_timeout(
            &Request {
                id: "cli:compatibility".into(),
                method: Method::Ping(PingParams::default()),
            },
            Duration::from_secs(2),
        )
        .map_err(super::api_client_error_to_io)?;
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(error.to_string()));
    }
    Ok(response["result"]["version"].as_str().map(str::to_owned))
}

fn version_gap(command: &str, version: Option<&str>) -> String {
    format!(
        "{command} needs a server ≥ {MIN_TURN_SERVER} (running: {}); hand it off with `flk server live-handoff` or restart it",
        version.unwrap_or("unknown")
    )
}

pub(super) fn require_turn_server(command: &str) -> io::Result<()> {
    let version = running_version()?;
    let supported = version
        .as_deref()
        .and_then(|raw| crate::update::Version::parse(raw.split(['-', '+']).next()?))
        .is_some_and(|version| version >= MIN_TURN_SERVER);
    if !supported {
        return Err(io::Error::other(version_gap(command, version.as_deref())));
    }
    Ok(())
}

pub(super) fn unknown_variant_message(
    response: &serde_json::Value,
    command: &str,
) -> Option<String> {
    let error = response.get("error")?;
    if error["code"] == "invalid_request" && error["message"].as_str()?.contains("unknown variant")
    {
        let version = running_version().ok().flatten();
        Some(version_gap(command, version.as_deref()))
    } else {
        None
    }
}
