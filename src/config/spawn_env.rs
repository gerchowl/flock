//! `[spawn.env]` — which environment keys each agent CLI needs carried into an
//! argv-spawned child, declared by the fleet rather than compiled in (#397).
//!
//! An argv exec runs no shell init, so whatever a login shell would have
//! exported never reaches the child. [`crate::spawn::env`] is the mechanism
//! that restores it and refuses when it cannot. WHICH keys those are is not
//! mechanism: it is a fact about how one vendor's CLI finds its credentials on
//! one fleet, and while it lived in a `match` in flock's binary, a fleet whose
//! agents needed a different key had to wait for a flock release to say so.
//!
//! One table, read by both halves of the child's environment — the carry in
//! [`crate::spawn::env`] and the allowlist in [`crate::spawn::allowlist`] — so
//! a key cannot be resolved by one and cleared by the other.
//!
//! # The compiled default, and why it names a Claude variable
//!
//! The default declares `CLAUDE_CONFIG_DIR` for `claude`, so every existing
//! fleet keeps working with no config change. That variable relocates Claude
//! Code's whole home — config, stored credentials, history — and on a fleet
//! that selects a profile with a zsh function it is SHELL-SESSION state, not
//! process-tree state. An interactive pane inherits it because zsh sources the
//! operator's rc file; an argv-spawned pane does not, and falls back to
//! `~/.claude`. That fallback is worse than a clean failure: the default
//! profile may be authenticated to a DIFFERENT account, so the child runs,
//! bills the wrong account, and writes its history into a config dir nobody is
//! watching (#359, #366).
//!
//! It is a default, not a law. A fleet that pins one account per host declares
//! an empty list and carries nothing; a fleet whose Codex or Kimi install
//! needs a key of its own declares that instead. A declaration REPLACES the
//! default for that agent rather than adding to it, because a table that can
//! only grow is a one-way ratchet: removal has to be expressible or the
//! default becomes permanent.
//!
//! One sharp edge, inherited from the config layer rather than introduced
//! here: `config.local.toml` merges over `config.toml` with ARRAYS APPENDED
//! (`super::io::deep_merge_tables`, the behaviour `[[peers]]` relies on). So
//! an empty list in the overlay cannot clear a list the base file set — the
//! removal has to be written in the file that declared it. Replacement is
//! against the COMPILED default; between the two file layers, lists add.
//!
//! # Declaring a key has two consequences, deliberately
//!
//! A declared key is carried down from the requester AND allowlisted through
//! the child's `env_clear()`. It also becomes refusable: declared, and absent
//! from a requester whose environment flock could not read, is
//! `agent_profile_unresolved` — a clean stop rather than a child that comes up
//! looking healthy on the wrong account. So the bar for an entry is the one
//! [`crate::spawn::env`] states: keys whose absence silently changes WHICH
//! ACCOUNT the child runs as, not everything the child might like.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `[spawn]` — what the fleet declares about the children flock launches.
///
/// Separate from `[fleet]`, which is the ceiling on agent-initiated spawn
/// (ADR-0014): that section answers whether a spawn may happen at all, this
/// one answers what the child's environment looks like once it does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SpawnConfig {
    /// `[spawn.env]` — per-agent carried and allowlisted environment keys.
    pub env: SpawnEnvConfig,
}

/// Environment keys per agent kind, keyed by the agent name flock detects
/// (`claude`, `codex`, `kimi`, ...).
///
/// The key is a string resolved through [`crate::detect::identify_agent`], not
/// a serde enum. Two reasons. An unknown key has to be a diagnostic rather
/// than a parse error, or one config shared across a fleet would break every
/// node running a flock that predates the agent named in it. And the agent
/// enum stays closed (ADR-0014): the config names an agent flock already
/// detects, it does not invent one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SpawnEnvConfig {
    by_agent: BTreeMap<String, Vec<String>>,
}

/// Claude Code's config-dir selector, the one key the compiled default
/// declares. The module header says why relocating it is a wrong-ACCOUNT
/// concern and not a cosmetic one.
///
/// What is NOT here is as deliberate as what is. `ANTHROPIC_API_KEY` and its
/// siblings stay compiled into `spawn::allowlist` rather than moving here,
/// because a declared key carries a REFUSAL: declared and absent means "this
/// child would run on the wrong account, so stop". A missing
/// `ANTHROPIC_API_KEY` must not refuse a spawn — it means this deployment
/// does not use one. Different semantics, different table.
const DEFAULT_CLAUDE_KEYS: &[&str] = &["CLAUDE_CONFIG_DIR"];

impl Default for SpawnEnvConfig {
    fn default() -> Self {
        Self {
            by_agent: BTreeMap::from([(
                crate::detect::agent_label(crate::detect::Agent::Claude).to_string(),
                DEFAULT_CLAUDE_KEYS
                    .iter()
                    .map(|key| (*key).to_string())
                    .collect(),
            )]),
        }
    }
}

impl SpawnEnvConfig {
    /// The keys declared for `agent`.
    ///
    /// Empty for an agent with no declaration, and for `None` — the argv flock
    /// does not recognise as an agent CLI, where flock has no per-CLI
    /// knowledge to apply and guessing one CLI's keys onto another is how a
    /// credential ends up allowed.
    pub fn keys_for(&self, agent: Option<crate::detect::Agent>) -> &[String] {
        agent
            .and_then(|agent| self.by_agent.get(crate::detect::agent_label(agent)))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// A declaration assembled in code rather than parsed from a file.
    /// Replaces whatever this agent had, the same way a config file's entry
    /// does. Test-only: the shipped paths all read a parsed config.
    #[cfg(test)]
    pub fn with_agent(mut self, agent: crate::detect::Agent, keys: &[&str]) -> Self {
        self.by_agent.insert(
            crate::detect::agent_label(agent).to_string(),
            keys.iter().map(|key| (*key).to_string()).collect(),
        );
        self
    }

    /// A declaration with nothing in it: no agent carries anything, and no
    /// spawn can refuse over a profile. What a fleet that pins one account per
    /// host would write, and how a test states that starting point.
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            by_agent: BTreeMap::new(),
        }
    }

    /// Canonicalise what a config file declared, merge it OVER the compiled
    /// default, and report what was dropped.
    ///
    /// Merging here rather than in serde is what makes the default survive a
    /// partial declaration: `#[serde(default)]` replaces the whole map, so a
    /// fleet declaring only `codex` would otherwise silently un-declare
    /// `claude`. Idempotent — a map that is already the default merges to
    /// itself — so a config with no `[spawn.env]` at all passes through
    /// unchanged.
    pub(super) fn normalize(&mut self) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let declared = std::mem::take(&mut self.by_agent);
        let mut merged = Self::default().by_agent;
        // Which spelling already claimed each agent. Iteration is over a
        // BTreeMap, so a `claude` / `claude-code` collision resolves the same
        // way on every node rather than by hash order.
        let mut claimed: BTreeMap<String, String> = BTreeMap::new();

        for (spelling, keys) in declared {
            let Some(agent) = crate::detect::identify_agent(&spelling) else {
                diagnostics.push(format!(
                    "[spawn.env] {spelling:?} is not an agent flock detects; entry ignored"
                ));
                continue;
            };
            let label = crate::detect::agent_label(agent).to_string();
            if let Some(first) = claimed.get(&label) {
                diagnostics.push(format!(
                    "[spawn.env] {spelling:?} and {first:?} both name the {label} agent; \
                     {spelling:?} ignored"
                ));
                continue;
            }

            let mut accepted: Vec<String> = Vec::with_capacity(keys.len());
            for key in keys {
                let trimmed = key.trim();
                if let Some(reason) = key_rejection(trimmed) {
                    diagnostics.push(format!(
                        "[spawn.env] {label} key {key:?} {reason}; key ignored"
                    ));
                    continue;
                }
                if !accepted.iter().any(|seen| seen == trimmed) {
                    accepted.push(trimmed.to_string());
                }
            }

            claimed.insert(label.clone(), spelling);
            merged.insert(label, accepted);
        }

        self.by_agent = merged;
        diagnostics
    }
}

/// Why a declared key cannot be used, or `None` when the name is usable.
///
/// Exact names only, never prefixes. `AWS_*`-style matching reads as
/// convenient and is how a credential ends up allowed by a rule written for a
/// config variable that happened to share a stem — so a wildcard is refused
/// out loud rather than quietly matched as a literal name that never exists.
fn key_rejection(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("is empty");
    }
    if key.contains('=') {
        return Some("contains '=', which is not part of a variable name");
    }
    if key.contains('*') {
        return Some("looks like a wildcard, and only exact names are matched");
    }
    if key.contains(char::is_whitespace) {
        return Some("contains whitespace, which no shell can export");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;

    fn declared(pairs: &[(&str, &[&str])]) -> SpawnEnvConfig {
        SpawnEnvConfig {
            by_agent: pairs
                .iter()
                .map(|(agent, keys)| {
                    (
                        (*agent).to_string(),
                        keys.iter().map(|key| (*key).to_string()).collect(),
                    )
                })
                .collect(),
        }
    }

    /// The one test that names the real variable, and it names it on purpose.
    /// Every fleet that never writes a `[spawn.env]` runs on this table, so a
    /// typo in it would ship silently and reproduce #359 — a child on the
    /// default profile, in a pane that comes up looking healthy — for exactly
    /// the fleets that never opted into anything.
    #[test]
    fn the_shipped_default_declares_the_claude_profile_selector() {
        let config = SpawnEnvConfig::default();
        assert_eq!(config.keys_for(Some(Agent::Claude)), ["CLAUDE_CONFIG_DIR"]);
        assert!(config.keys_for(Some(Agent::Codex)).is_empty());
        assert!(config.keys_for(None).is_empty());
    }

    /// The ratchet the config exists to avoid: a fleet that needs no selector
    /// carried must be able to say so, and an empty list is how it says it.
    #[test]
    fn an_empty_declaration_removes_the_default_key() {
        let mut config = declared(&[("claude", &[])]);
        assert!(
            config.normalize().is_empty(),
            "no diagnostics for a removal"
        );
        assert!(config.keys_for(Some(Agent::Claude)).is_empty());
    }

    /// Replacement, not union: a fleet whose Claude installs read a different
    /// variable gets that variable and not both.
    #[test]
    fn a_declaration_replaces_the_default_rather_than_adding_to_it() {
        let mut config = declared(&[("claude", &["FLEET_PROFILE_DIR"])]);
        config.normalize();
        assert_eq!(config.keys_for(Some(Agent::Claude)), ["FLEET_PROFILE_DIR"]);
    }

    /// Declaring one agent must not silently un-declare the others, which is
    /// what a bare `#[serde(default)]` replacement of the whole map would do.
    #[test]
    fn declaring_one_agent_leaves_the_others_on_the_default() {
        let mut config = declared(&[("codex", &["CODEX_HOME"])]);
        config.normalize();
        assert_eq!(config.keys_for(Some(Agent::Codex)), ["CODEX_HOME"]);
        assert_eq!(config.keys_for(Some(Agent::Claude)), ["CLAUDE_CONFIG_DIR"]);
    }

    /// An agent this flock does not know is a diagnostic, never a parse
    /// failure: one config is shared across a fleet, and a node running an
    /// older flock must keep the rest of its table rather than lose the
    /// section wholesale.
    #[test]
    fn an_unknown_agent_name_is_a_diagnostic_and_leaves_the_rest_intact() {
        let mut config = declared(&[("not-an-agent", &["X"]), ("codex", &["CODEX_HOME"])]);
        let diagnostics = config.normalize();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("not-an-agent"), "{diagnostics:?}");
        assert_eq!(config.keys_for(Some(Agent::Codex)), ["CODEX_HOME"]);
    }

    /// Two spellings of one agent would otherwise resolve by map order, so one
    /// of two contradictory declarations would win silently.
    #[test]
    fn two_spellings_of_one_agent_report_the_collision() {
        let mut config = declared(&[("claude", &["FIRST"]), ("claude-code", &["SECOND"])]);
        let diagnostics = config.normalize();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("claude-code"), "{diagnostics:?}");
        assert_eq!(
            config.keys_for(Some(Agent::Claude)),
            ["FIRST"],
            "the first spelling in sorted order wins, on every node alike"
        );
    }

    /// A wildcard entry must not reach the allowlist. Matched as a literal it
    /// would never fire; matched as a prefix it is how a credential ends up
    /// allowed by a rule written for the config variable next to it.
    #[test]
    fn a_wildcard_key_is_refused_out_loud() {
        let mut config = declared(&[("claude", &["ANTHROPIC_*", "GOOD_KEY"])]);
        let diagnostics = config.normalize();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0].contains("wildcard"), "{diagnostics:?}");
        assert_eq!(config.keys_for(Some(Agent::Claude)), ["GOOD_KEY"]);
    }

    #[test]
    fn empty_and_malformed_keys_are_dropped_with_a_reason() {
        let mut config = declared(&[("claude", &["", "A=B", "TWO WORDS", " PADDED "])]);
        let diagnostics = config.normalize();
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        assert_eq!(
            config.keys_for(Some(Agent::Claude)),
            ["PADDED"],
            "a padded name is trimmed, not rejected"
        );
    }

    /// Normalizing the default must not perturb it, so a config file with no
    /// `[spawn.env]` at all is byte-identical to one that was never parsed.
    #[test]
    fn normalizing_is_idempotent() {
        let mut config = SpawnEnvConfig::default();
        assert!(config.normalize().is_empty());
        assert_eq!(config, SpawnEnvConfig::default());
    }

    #[test]
    fn an_empty_declaration_carries_nothing_for_any_agent() {
        let config = SpawnEnvConfig::empty();
        assert!(config.keys_for(Some(Agent::Claude)).is_empty());
    }

    /// The `[spawn.env]` table parses from the shape the issue names.
    #[test]
    fn the_section_parses_from_toml() {
        let section: SpawnConfig = toml::from_str(
            "[env]\nclaude = [\"CLAUDE_CONFIG_DIR\", \"FLEET_PROFILE\"]\ncodex = []\n",
        )
        .expect("the section parses");
        assert_eq!(
            section.env.keys_for(Some(Agent::Claude)),
            ["CLAUDE_CONFIG_DIR", "FLEET_PROFILE"]
        );
        assert!(section.env.keys_for(Some(Agent::Codex)).is_empty());
    }
}
