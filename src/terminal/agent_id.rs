use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fleet-global, restart-stable identity for one agent.
///
/// **Address is not location.** flock already had three id-shaped things and
/// none of them can name an agent:
///
/// - a public pane id (`w3:p1`) names a *placement* — it changes when the pane
///   moves, and on another host it names someone else's pane entirely;
/// - a [`TerminalId`](super::TerminalId) names a running PTY — stable across a
///   pane move, but re-minted on every server start, so it cannot survive a
///   restart;
/// - an agent *session* id (Claude's uuid) belongs to the agent process and
///   resets on `/clear`, and a pane with no agent has none at all.
///
/// That gap is why a message relayed from another machine arrived `from
/// unknown`: the sender was inferred from local process ancestry, and there was
/// no name to fall back to. An `AgentId` is minted once when the pane is
/// created, persisted in the session snapshot, and never rewritten — so it
/// still names the same agent after a restart, a pane move, or a workspace
/// rename, and it is unique across the fleet.
///
/// Host and pane remain *resolvable metadata* about an agent (see the fleet
/// directory), never part of its name. Encoding either would reintroduce the
/// bug: an agent that moves would have to be re-addressed, breaking in-flight
/// threads — the same mistake as keying a worktree namespace on a directory
/// basename (#212) or a workspace's repo on stale membership (#197).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct AgentId(String);

static NEXT_AGENT_ID: AtomicU64 = AtomicU64::new(1);

impl AgentId {
    /// Mint a new identity. Called exactly once per pane, at creation.
    ///
    /// Fleet-uniqueness comes from the host name plus a wallclock/pid/counter
    /// triple: two hosts cannot collide because the host differs, and two
    /// processes on one host cannot because the pid does. The host is a
    /// *uniqueness* ingredient captured at mint time, not a routing hint —
    /// where the agent lives is answered by the directory, and an agent whose
    /// host is renamed keeps the id it was minted with.
    pub fn alloc(host: &str) -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_micros())
            .unwrap_or(0);
        let counter = NEXT_AGENT_ID.fetch_add(1, Ordering::Relaxed);
        let host = host
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
            .take(24)
            .collect::<String>();
        let host = if host.is_empty() {
            "host".to_string()
        } else {
            host
        };
        Self(format!(
            "agent_{host}_{micros:x}{:x}{counter:x}",
            std::process::id()
        ))
    }

    /// Adopt an id read back from a session snapshot.
    pub fn from_persisted(raw: String) -> Self {
        Self(raw)
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_within_and_across_hosts() {
        let a = AgentId::alloc("sage");
        let b = AgentId::alloc("sage");
        assert_ne!(a, b, "two agents on one host must differ");

        let elsewhere = AgentId::alloc("anvil-dev");
        assert_ne!(a, elsewhere);
        assert!(a.to_string().contains("sage"));
        assert!(elsewhere.to_string().contains("anvil-dev"));
    }

    #[test]
    fn a_persisted_id_round_trips_unchanged() {
        // The whole point: an id read back from a snapshot is the SAME id, so
        // an agent keeps its name across a restart. Minting on restore would
        // silently re-address every agent and orphan in-flight message threads.
        let minted = AgentId::alloc("sage");
        let restored = AgentId::from_persisted(minted.to_string());
        assert_eq!(minted, restored);
    }

    #[test]
    fn a_hostile_host_name_cannot_shape_the_id() {
        // Host names reach this from config and gossip. An id is used in paths,
        // logs and wire payloads, so it must not carry separators or control
        // characters no matter what it was handed.
        let id = AgentId::alloc("../../etc/passwd\n\u{1b}[31m");
        assert!(
            id.to_string()
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
            "{id}"
        );
    }

    #[test]
    fn an_empty_host_still_yields_a_usable_id() {
        let id = AgentId::alloc("");
        assert!(id.to_string().starts_with("agent_host_"), "{id}");
    }
}
