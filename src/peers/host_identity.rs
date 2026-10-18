//! Peer hostname identities, including the legacy SSH-destination fallback.
//! ADR-0014 §4: reject invalid identities before choosing a cache key.

use std::net::Ipv6Addr;

/// Canonical identity for either reported hosts or parsed SSH host components.
/// DNS labels are ASCII LDH; IPv6 reports must already use canonical spelling.
pub(super) fn canonical_host(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 253 {
        return None;
    }
    let ldh = value.split('.').all(|label| {
        let bytes = label.as_bytes();
        bytes.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    });
    if ldh {
        return Some(value.to_ascii_lowercase());
    }
    value
        .parse::<Ipv6Addr>()
        .ok()
        .filter(|address| address.to_string() == value)
        .map(|address| address.to_string())
}

fn port(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u16>().is_ok_and(|port| port != 0)
}

fn ssh_host(target: &str) -> Option<String> {
    if target.len() > super::received::MAX_TOKEN_BYTES
        || !crate::remote::is_valid_ssh_destination(target)
    {
        return None;
    }
    let endpoint = match target.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.contains('@') => host,
        Some(_) => return None,
        None => target,
    };
    if let Some(bracketed) = endpoint.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']')?;
        if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(port) {
            return None;
        }
        return host
            .parse::<Ipv6Addr>()
            .ok()
            .map(|address| address.to_string());
    }
    if let Ok(address) = endpoint.parse::<Ipv6Addr>() {
        return Some(address.to_string());
    }
    match endpoint.rsplit_once(':') {
        Some((host, suffix)) if port(suffix) => Some(host.to_owned()),
        Some(_) => None,
        None => Some(endpoint.to_owned()),
    }
}

/// The single key derivation for relay rows and carried fleet entries. Missing
/// hosts use a parsed SSH host, never the opaque user/host/port destination.
/// Receive boundaries report invalid keys through typed, content-free diagnostics.
pub(crate) fn row_host_key(host: Option<&str>, ssh_target: &str) -> Option<String> {
    match host.filter(|host| !host.is_empty()) {
        Some(host) => canonical_host(host),
        None => ssh_host(ssh_target).and_then(|host| canonical_host(&host)),
    }
}

/// Operator-facing warning about this node's own advertised identity. Values
/// are fixed text; the rejected name is never echoed or automatically changed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct NodeHostWarning {
    /// Registered refusal code, advisory on the local status surface.
    pub code: &'static str,
    /// The admission rule violated by the effective configured or OS name.
    pub message: &'static str,
    /// Explicit recovery, including the cached-name restart requirement.
    pub recovery: &'static str,
}

impl std::fmt::Display for NodeHostWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} {}", self.code, self.message, self.recovery)
    }
}

/// Validate the effective local name without changing it or emitting peer logs.
pub(crate) fn node_host_warning(name: &str) -> Option<NodeHostWarning> {
    canonical_host(name).is_none().then_some(NodeHostWarning {
        code: crate::api::error_code::ErrorCode::InvalidNodeHost.as_str(),
        message: "Peer host identity requires ASCII LDH labels (1-63 bytes each, 253 total, no empty labels or edge hyphens) or a canonical IPv6 literal.",
        recovery: "Set top-level name to a valid hostname and restart the server; the name has not been changed.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security2200_ipv6_and_ipv4_fallbacks_share_reported_host_keys() {
        for (target, host) in [
            ("operator@[2001:0DB8:0:0:0:0:0:1]:2222", "2001:db8::1"),
            ("2001:0DB8:0:0:0:0:0:1", "2001:db8::1"),
            ("operator@192.0.2.1:22", "192.0.2.1"),
        ] {
            assert_eq!(row_host_key(None, target), Some(host.into()));
            assert_eq!(row_host_key(Some(host), target), Some(host.into()));
        }
    }

    #[test]
    fn security2200_malformed_fallback_authorities_are_refused() {
        for target in [
            "operator@@node-a.invalid",
            "@node-a.invalid",
            "operator@",
            "node-a.invalid:",
            "node-a.invalid:port",
            "node-a.invalid:0",
            "node-a.invalid:65536",
            "[2001:db8::1]suffix",
            "[node-a.invalid]:22",
            "node-a.invalid..",
        ] {
            assert!(row_host_key(None, target).is_none(), "{target}");
        }
    }

    #[test]
    fn security2200_display_dedupe_uses_configured_name_and_admitted_ssh_host() {
        let mut state = crate::app::state::AppState::test_new();
        let mut plain = crate::peers::PeerSummaryState::new(&crate::config::PeerConfig {
            name: "node-a.invalid".into(),
            ssh: "operator@node-a.invalid:22".into(),
            ..Default::default()
        });
        plain.host = Some("node-a.invalid".into());
        state.peer_summaries.push(plain);
        let relayed = serde_json::from_value(serde_json::json!({
            "name":"Other display", "ssh_target":"operator@NODE-A.invalid:2222",
            "host":"", "origin":"hub.invalid"
        }))
        .unwrap();
        crate::peers::merge_relayed_fleet(
            &mut state.relayed_fleet_cache,
            vec![relayed],
            "hub.invalid",
        );
        assert_eq!(state.relayed_fleet_cache.len(), 1);
        assert_eq!(state.remote_peers().len(), 1);
    }
}
