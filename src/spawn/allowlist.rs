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
//! Since #397 the agent-specific half is TWO tables with different jobs, and
//! the distinction is worth stating. The fleet's `[spawn.env]` declaration
//! (`crate::config::SpawnEnvConfig`) names the keys whose absence changes
//! WHICH ACCOUNT the child runs as; those are carried down from the requester
//! by [`super::env`] and allowlisted here from ONE declaration, so a key can
//! never be resolved by one and cleared by the other. [`per_agent`] below is
//! the rest: keys the server may already hold and the child may keep, which is
//! a decision about what crosses `env_clear()` rather than about how a fleet
//! selects a profile, and carries no refusal when one is absent.
//!
//! Exact keys only, no prefixes. `AWS_*`-style matching reads as convenient
//! and is how a credential ends up allowed by a rule written for a config
//! variable that happened to share a stem. A declared key is validated the
//! same way before it reaches this table.

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

/// Claude Code's own keys, on top of [`BASELINE`] and on top of whatever the
/// fleet declared in `[spawn.env]`.
///
/// The profile selector is NOT here any more (#397): it is declared by the
/// fleet and reaches this allowlist through [`for_argv`], which is what keeps
/// the carry and the allowlist from disagreeing. What stays compiled in is the
/// set below, and it stays for a reason rather than by omission — these keys
/// carry no refusal. A fleet's declaration means "absent, this child is on the
/// wrong account, so stop"; `ANTHROPIC_API_KEY` absent from the server must
/// not stop a spawn, it just means this deployment does not use one.
const CLAUDE_CREDENTIALS: &[&str] = &[
    // Where the child talks to, and as whom.
    //
    // These are credential-shaped, and they are here deliberately. A profile
    // directory holds Claude Code's stored OAuth credentials, so allowing the
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
    /// Owned rather than `&'static str`, because half the table is now runtime
    /// data: the fleet's `[spawn.env]` declaration (#397).
    keys: Vec<String>,
}

impl SpawnAllowlist {
    /// Which agent this was resolved for, or `unknown` for an argv flock does
    /// not recognise as an agent CLI — which gets the baseline and nothing
    /// more, because flock has no per-CLI knowledge to apply.
    pub fn agent(&self) -> &'static str {
        self.agent
    }

    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// The keys as one line, for the spawn's log record.
    pub fn joined(&self) -> String {
        self.keys.join(",")
    }
}

/// Which keys survive into a child launched from `argv`.
///
/// `declared` is the fleet's `[spawn.env]` table, the SAME value
/// [`super::env::resolve`] reads to decide what to carry down from the
/// requester. One declaration, both consumers: a key the fleet declares is
/// carried AND allowed through, and a key it removes is neither. Resolving
/// them from two tables is how a variable gets carried and then cleared.
pub fn for_argv(argv: &[String], declared: &crate::config::SpawnEnvConfig) -> SpawnAllowlist {
    let agent = super::env::agent_for_argv(argv);
    let mut keys: Vec<String> = BASELINE
        .iter()
        .chain(per_agent(agent).iter())
        .map(|key| (*key).to_string())
        .collect();
    for key in declared.keys_for(agent) {
        if !keys.iter().any(|seen| seen == key) {
            keys.push(key.clone());
        }
    }
    SpawnAllowlist {
        agent: agent.map_or("unknown", crate::detect::agent_label),
        keys,
    }
}

/// The keys one CLI needs beyond the baseline.
///
/// A kind with no entry is not an error: it gets the baseline, which is enough
/// to start a process. It is the agent-specific state whose absence is silent
/// that has to be enumerated here, one CLI at a time.
fn per_agent(agent: Option<crate::detect::Agent>) -> &'static [&'static str] {
    match agent {
        Some(crate::detect::Agent::Claude) => CLAUDE_CREDENTIALS,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SpawnEnvConfig;
    use crate::detect::Agent;
    use crate::spawn::AgentKind;

    /// Any valid prompt will do here: these tests read argv[0], not the turn.
    fn probe_prompt() -> crate::spawn::prompt::SpawnPrompt {
        crate::spawn::prompt::SpawnPrompt::compose("probe").expect("a plain prompt composes")
    }

    fn claude_argv() -> Vec<String> {
        vec!["claude".to_string(), "prompt".to_string()]
    }

    fn contains(allowlist: &SpawnAllowlist, key: &str) -> bool {
        allowlist.keys().iter().any(|seen| seen == key)
    }

    /// Acceptance: every supported kind resolves to a table that can actually
    /// start it. PATH and HOME are the two whose absence is not a subtle
    /// misbehaviour but a failure to exec at all — `CommandBuilder` resolves
    /// argv[0] through the builder's PATH.
    #[test]
    fn every_supported_agent_kind_gets_an_allowlist_that_can_start_it() {
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            let allowlist = for_argv(&kind.argv(&probe_prompt()), &SpawnEnvConfig::default());
            for required in ["PATH", "HOME"] {
                assert!(
                    contains(&allowlist, required),
                    "{} must inherit {required} or it cannot exec",
                    allowlist.agent()
                );
            }
        }
    }

    /// A fleet that declares nothing at all still gets a startable child: the
    /// declaration adds to the baseline, it does not replace it.
    #[test]
    fn an_empty_declaration_still_leaves_a_startable_baseline() {
        let allowlist = for_argv(&claude_argv(), &SpawnEnvConfig::empty());
        for required in ["PATH", "HOME"] {
            assert!(contains(&allowlist, required));
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
            let argv = kind.argv(&probe_prompt());
            assert!(
                super::super::env::agent_for_argv(&argv).is_some(),
                "{argv:?} must resolve to an agent, or its per-kind keys are unreachable"
            );
        }
    }

    /// The property #397 exists to make unbreakable: ONE declaration feeds
    /// both halves of the child's environment. A key declared once is carried
    /// AND allowed through; a key removed is neither. Two tables would let a
    /// variable be resolved from the requester and then cleared by the scrub,
    /// which is the #359 failure with extra steps.
    #[test]
    fn a_declared_key_is_both_carried_and_allowlisted() {
        let declared = SpawnEnvConfig::empty().with_agent(Agent::Claude, &["FLEET_PROFILE_DIR"]);
        let requester = crate::spawn::env::RequesterEnv::Attested(
            [(
                "FLEET_PROFILE_DIR".to_string(),
                "/profiles/work".to_string(),
            )]
            .into_iter()
            .collect(),
        );

        let carried =
            crate::spawn::env::resolve(Some(Agent::Claude), &declared, &requester, |_| true)
                .expect("a present, existing profile resolves");
        let allowlist = for_argv(&claude_argv(), &declared);

        assert_eq!(carried.len(), 1, "{carried:?}");
        assert!(
            contains(&allowlist, "FLEET_PROFILE_DIR"),
            "a carried key the allowlist drops is a key resolved and then cleared"
        );
    }

    /// The other direction of the same property, and the one a union merge
    /// would break: a fleet that removes a key must have it disappear from
    /// BOTH tables, not linger in the allowlist.
    #[test]
    fn a_removed_key_is_neither_carried_nor_allowlisted() {
        // Whatever the shipped default declares, so the test follows the
        // default rather than restating a variable name here.
        let removed = SpawnEnvConfig::default()
            .keys_for(Some(Agent::Claude))
            .first()
            .expect("the shipped default declares a key")
            .clone();
        let declared = SpawnEnvConfig::empty().with_agent(Agent::Claude, &[]);
        let requester = crate::spawn::env::RequesterEnv::Attested(
            [(removed.clone(), "/profiles/work".to_string())]
                .into_iter()
                .collect(),
        );

        let carried =
            crate::spawn::env::resolve(Some(Agent::Claude), &declared, &requester, |_| true)
                .expect("nothing declared, nothing to refuse over");
        let allowlist = for_argv(&claude_argv(), &declared);

        assert!(carried.is_empty(), "{carried:?}");
        assert!(
            !contains(&allowlist, &removed),
            "{removed} was removed from the declaration and must leave both tables"
        );
    }

    /// The key #366 went to some trouble to supply, still allowed through on
    /// the SHIPPED default — a fleet that never writes a config must see no
    /// change from #397.
    #[test]
    fn the_shipped_default_still_allows_the_claude_profile_selector() {
        let allowlist = for_argv(&claude_argv(), &SpawnEnvConfig::default());
        for declared in SpawnEnvConfig::default().keys_for(Some(Agent::Claude)) {
            assert!(
                contains(&allowlist, declared),
                "{declared} is declared by default and must survive the scrub"
            );
        }
        assert!(
            !SpawnEnvConfig::default()
                .keys_for(Some(Agent::Claude))
                .is_empty(),
            "the default must declare something, or this test asserts nothing"
        );
        assert_eq!(allowlist.agent(), "claude");
    }

    /// A declared key that the compiled table already names must not be
    /// listed twice — the scrub would work either way, but the logged line is
    /// what an operator reads to see what a child inherited.
    #[test]
    fn a_declaration_that_repeats_a_compiled_key_does_not_duplicate_it() {
        let declared = SpawnEnvConfig::empty().with_agent(Agent::Claude, &["HOME", "PATH"]);
        let allowlist = for_argv(&claude_argv(), &declared);
        let homes = allowlist.keys().iter().filter(|key| *key == "HOME").count();
        assert_eq!(homes, 1, "{:?}", allowlist.keys());
    }

    /// An argv flock does not recognise gets the baseline and nothing more —
    /// no per-CLI knowledge exists to apply, and guessing one CLI's keys onto
    /// another is how a credential ends up allowed. A declaration for another
    /// agent must not leak in either.
    #[test]
    fn an_unrecognised_argv_gets_the_baseline_only() {
        let declared = SpawnEnvConfig::empty().with_agent(Agent::Claude, &["FLEET_PROFILE_DIR"]);
        let allowlist = for_argv(&["cat".to_string()], &declared);
        assert_eq!(allowlist.agent(), "unknown");
        assert_eq!(allowlist.keys(), BASELINE);
    }

    /// Exact keys, never prefixes: `AWS_*`-style matching is how a credential
    /// ends up allowed by a rule written for a config variable next to it.
    /// Config is validated for this too, so the property holds for a declared
    /// key as well as a compiled one.
    #[test]
    fn no_entry_is_a_wildcard() {
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            for key in for_argv(&kind.argv(&probe_prompt()), &SpawnEnvConfig::default()).keys() {
                assert!(
                    !key.contains('*') && !key.is_empty(),
                    "{key:?} is not an exact environment variable name"
                );
            }
        }
    }
}
