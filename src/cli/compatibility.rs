// Turn cursors and agent.result first shipped together in v0.9.0.
const MIN_TURN_SERVER: crate::update::Version = crate::update::Version {
    major: 0,
    minor: 9,
    patch: 0,
};

fn running_version() -> Option<String> {
    crate::api::compatibility::server_version(&super::ApiClient::local())
        .map(|server| server.version)
}

fn version_gap(command: &str, version: Option<&str>) -> String {
    format!(
        "{command} needs a server ≥ {MIN_TURN_SERVER} (running: {}); hand it off with `flk server live-handoff` or upgrade and restart it",
        version.unwrap_or("unknown")
    )
}

fn parse_server_version(raw: &str) -> Option<crate::update::Version> {
    crate::update::Version::parse(raw.split(['-', '+']).next()?)
}

fn is_capability_failure(reason: &str) -> bool {
    crate::api::compatibility::is_capability_failure(reason) || reason.contains("no turn cursor")
}

pub(super) fn diagnose_failure(command: &str, reason: &str) -> String {
    if !is_capability_failure(reason) {
        return reason.to_owned();
    }
    let version = running_version();
    diagnosis(command, reason, version.as_deref())
}

fn diagnosis(command: &str, reason: &str, version: Option<&str>) -> String {
    match version.and_then(parse_server_version) {
        Some(parsed) if parsed < MIN_TURN_SERVER => version_gap(command, version),
        _ => reason.to_owned(),
    }
}

pub(super) fn capability_error_message(
    response: &serde_json::Value,
    command: &str,
) -> Option<String> {
    let error = response.get("error")?;
    let reason = error["message"].as_str()?;
    if matches!(
        error["code"].as_str(),
        Some("unknown_method" | "method_not_found" | "server_version_gap")
    ) || is_capability_failure(reason)
    {
        let server = crate::api::compatibility::server_version(&super::ApiClient::local())?;
        if server.protocol >= crate::protocol::PROTOCOL_VERSION {
            return None;
        }
        let message = diagnosis(command, reason, Some(&server.version));
        (message != reason).then_some(message)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_version_parser_accepts_fork_preview_and_build_suffixes() {
        for raw in [
            "0.6.8-fork.9ebb536",
            "v0.6.8",
            "0.6.8-preview.abc",
            "0.6.8+build",
        ] {
            assert_eq!(
                parse_server_version(raw),
                crate::update::Version::parse("0.6.8")
            );
        }
        assert_eq!(parse_server_version("unknown"), None);
    }

    #[test]
    fn diagnosis_preserves_original_error_without_confirmed_old_version() {
        for version in [
            None,
            Some("unknown"),
            Some("0.9.0"),
            Some("0.9.0-preview.abc"),
            Some("0.10.0+build"),
        ] {
            assert_eq!(
                diagnosis(
                    "flk delegate",
                    "the server's record carried no turn cursor",
                    version
                ),
                "the server's record carried no turn cursor"
            );
        }
    }
}
