//! What an agent-spawned child is allowed to INHERIT (#347, ADR-0014 §3).
//!
//! [`super::env`] answers the other half of the child's environment: what a
//! login shell would have supplied and the flock server does not have,
//! restored explicitly. This module answers the opposite question — of what
//! the server DOES have, what survives into the child.
//!
//! ADR-0014 §3 decided a scrubbed baseline plus an explicit allowlist, the way
//! `[[checks.script]]` already runs (`env_clear()` and a declared set). #345
//! shipped a credential deny-list instead and said why: an allowlist that
//! omits something a CLI needs fails as a mysterious startup break rather than
//! a clean refusal. But a deny-list is only as good as its enumeration — a new
//! provider's token variable, a project-local `*_API_KEY`, `GIT_ASKPASS` — so
//! it defends against the leaks somebody thought of, while an allowlist
//! defends against the ones nobody has thought of yet. Both ship: the
//! allowlist inverts the default, and the deny-list sweeps afterwards for
//! anything that lands on both tables by accident.
//!
//! The table is per-agent for the reason the ADR flags and #366 has already
//! been bitten by: what a CLI requires is per-CLI knowledge. [`BASELINE`] is
//! what any process needs in order to be a process on this machine; everything
//! agent-specific hangs off [`per_agent`], keyed through the same argv seam
//! [`super::env::agent_for_argv`] that resolves the profile carry, so both
//! halves of the child's environment answer for one agent from one lookup.
//!
//! Exact keys only, no prefixes. `AWS_*`-style matching reads as convenient
//! and is how a credential ends up allowed by a rule written for a config
//! variable that happened to share a stem.

/// Keys every agent kind needs, regardless of which CLI it is.
///
/// The bar for an entry is that a real agent misbehaves without it, and that
/// the misbehaviour is the kind #359 named — the child runs and is subtly
/// wrong — rather than a clean failure. Each group below says which.
pub const BASELINE: &[&str] = &[
    // Without these two the child does not start at all. `CommandBuilder`
    // resolves argv[0] through the builder's own PATH, so an empty PATH is a
    // spawn failure, and HOME is where every agent CLI keeps its config.
    "HOME",
    "PATH",
    // The CLI shells out to run commands. Absent, portable-pty re-derives a
    // shell from the password database, which is not necessarily the one the
    // operator's tooling is written against.
    "SHELL",
    // Identity the child reports, and what git falls back to for an author
    // when a repo has no configured user.
    "USER",
    "LOGNAME",
    // macOS gives each user a private per-session temp directory and points
    // TMPDIR at it. Absent, the child writes into a shared /tmp instead.
    "TMPDIR",
    // Text handling. An agent that reads a diff containing non-ASCII under the
    // C locale mangles it, which is a data bug rather than a crash, and a
    // child on a different TZ timestamps its work differently from every other
    // agent in the fleet.
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    // TLS trust roots. On a Nix machine the CA bundle lives at a store path
    // that nothing but these variables names, so dropping them fails every
    // HTTPS call the agent makes — including the one to its own provider.
    // `NIX_SSL_CERT_FILE` is the one actually set on this fleet; the portable
    // spellings are here because the fleet is not the only deployment.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
    // Where terminfo lives. `pane.rs` sets the child's TERM to
    // `xterm-256color`, and on a Nix host the database describing it is a
    // store path these name — without them every curses tool the agent shells
    // out to comes up unable to find its own terminal type.
    "TERMINFO",
    "TERMINFO_DIRS",
    // Network reachability. Both halves matter: the proxy an operator's
    // network requires, and the exemptions that keep loopback traffic off it.
    // A proxy URL can carry credentials in its userinfo, which makes this the
    // one group here worth revisiting on a fleet that actually uses one.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    // Where a Linux CLI keeps config, cache and state when the operator has
    // moved them off the home-directory defaults.
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
];

/// Claude Code's own keys, on top of [`BASELINE`].
const CLAUDE: &[&str] = &[
    // The profile selector. `super::env` carries this down from the requester
    // for the case that bites here — the server does not have it at all — and
    // listing it keeps the allowlist from dropping it on a server that does
    // (#359, #366). An allowlist that forgot this key would reproduce exactly
    // that bug, now by design.
    super::env::CLAUDE_CONFIG_DIR,
    // Where the child talks to, and as whom.
    //
    // These are credential-shaped, and they are here deliberately. The config
    // dir above holds Claude Code's stored OAuth credentials, so allowing the
    // directory and refusing the key would be incoherent — both are the same
    // thing: the identity that lets the child BE the agent it was asked to be,
    // billed to the operator who asked for it. Contrast `GH_TOKEN`, whose
    // hazard is the child acting as the operator toward a THIRD party, which
    // is outside the blast radius the `Agent-Run:` trailer can bound. Refusing
    // these instead would leave an API-key fleet's children running and
    // unauthenticated, which is the #359 failure mode, not a clean refusal.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
];

/// The resolved allowlist for one spawn, plus the agent label the spawn logs
/// it under. Resolved at the arming site and carried to the scrub so the line
/// in the log and the set actually applied cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnAllowlist {
    agent: &'static str,
    keys: Vec<&'static str>,
}

impl SpawnAllowlist {
    /// Which agent this was resolved for, or `unknown` for an argv flock does
    /// not recognise as an agent CLI — which gets the baseline and nothing
    /// more, because flock has no per-CLI knowledge to apply.
    pub fn agent(&self) -> &'static str {
        self.agent
    }

    pub fn keys(&self) -> &[&'static str] {
        &self.keys
    }

    /// The keys as one line, for the spawn's log record.
    pub fn joined(&self) -> String {
        self.keys.join(",")
    }
}

/// Which keys survive into a child launched from `argv`.
pub fn for_argv(argv: &[String]) -> SpawnAllowlist {
    let agent = super::env::agent_for_argv(argv);
    SpawnAllowlist {
        agent: agent.map_or("unknown", crate::detect::agent_label),
        keys: BASELINE
            .iter()
            .chain(per_agent(agent).iter())
            .copied()
            .collect(),
    }
}

/// The keys one CLI needs beyond the baseline.
///
/// A kind with no entry is not an error: it gets the baseline, which is enough
/// to start a process. It is the agent-specific state whose absence is silent
/// that has to be enumerated here, one CLI at a time.
fn per_agent(agent: Option<crate::detect::Agent>) -> &'static [&'static str] {
    match agent {
        Some(crate::detect::Agent::Claude) => CLAUDE,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::AgentKind;

    /// Acceptance: every supported kind resolves to a table that can actually
    /// start it. PATH and HOME are the two whose absence is not a subtle
    /// misbehaviour but a failure to exec at all — `CommandBuilder` resolves
    /// argv[0] through the builder's PATH.
    #[test]
    fn every_supported_agent_kind_gets_an_allowlist_that_can_start_it() {
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            let allowlist = for_argv(&kind.argv("prompt"));
            for required in ["PATH", "HOME"] {
                assert!(
                    allowlist.keys().contains(&required),
                    "{} must inherit {required} or it cannot exec",
                    allowlist.agent()
                );
            }
        }
    }

    /// The per-kind table is reached through argv, so a kind whose argv[0] is
    /// not a name `identify_agent` knows would silently fall through to the
    /// baseline and lose everything its CLI needs. That is the mysterious
    /// startup break ADR-0014 §3 warns about, so it fails here instead.
    #[test]
    fn every_supported_agent_kind_is_recognised_from_its_own_argv() {
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            let argv = kind.argv("prompt");
            assert!(
                super::super::env::agent_for_argv(&argv).is_some(),
                "{argv:?} must resolve to an agent, or its per-kind keys are unreachable"
            );
        }
    }

    /// The key #366 went to some trouble to supply. An allowlist that drops it
    /// lands the child on the default profile, i.e. possibly a different
    /// authenticated account.
    #[test]
    fn the_claude_table_carries_the_profile_selector() {
        let allowlist = for_argv(&["claude".to_string(), "prompt".to_string()]);
        assert!(allowlist
            .keys()
            .contains(&crate::spawn::env::CLAUDE_CONFIG_DIR));
        assert_eq!(allowlist.agent(), "claude");
    }

    /// An argv flock does not recognise gets the baseline and nothing more —
    /// no per-CLI knowledge exists to apply, and guessing one CLI's keys onto
    /// another is how a credential ends up allowed.
    #[test]
    fn an_unrecognised_argv_gets_the_baseline_only() {
        let allowlist = for_argv(&["cat".to_string()]);
        assert_eq!(allowlist.agent(), "unknown");
        assert_eq!(allowlist.keys(), BASELINE);
    }

    /// Exact keys, never prefixes: `AWS_*`-style matching is how a credential
    /// ends up allowed by a rule written for a config variable next to it.
    #[test]
    fn no_entry_is_a_wildcard() {
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            for key in for_argv(&kind.argv("prompt")).keys() {
                assert!(
                    !key.contains('*') && !key.is_empty(),
                    "{key:?} is not an exact environment variable name"
                );
            }
        }
    }
}
