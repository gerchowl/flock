//! The environment shell init would have handed the child, restored for an
//! argv exec (#359).
//!
//! [`AgentKind::argv`](super::AgentKind::argv) hands `execvp` a bare argv on
//! purpose — there is no shell between here and the exec, which is what makes
//! a hostile prompt inert. The cost of that property is that the child never
//! runs shell init, and some of what an agent CLI needs lives exactly there.
//!
//! `CLAUDE_CONFIG_DIR` is the case that bites now. It relocates Claude Code's
//! whole home — config, credentials, history — and on a fleet that selects a
//! profile with a zsh function it is SHELL-SESSION state, not process-tree
//! state. An interactive pane inherits it because zsh sources the operator's
//! rc file; an argv-spawned pane does not, and falls back to `~/.claude`.
//!
//! That fallback is worse than a clean failure: the default profile may be
//! authenticated to a DIFFERENT account, so the child runs, bills the wrong
//! account, and writes its history into a config dir nobody is watching.
//!
//! The server cannot supply the answer. Its own environment has no
//! `CLAUDE_CONFIG_DIR` at all — nothing strips it, it was never there — so an
//! allowlist at the server end cannot help. The only process that knows which
//! profile is in play is the one ASKING for the spawn: the calling agent, the
//! `flk` CLI the operator ran, or the pane being forked. This module reads it
//! from there.
//!
//! The table is keyed per agent, not global, for the reason ADR-0014 §3 flags
//! about allowlists: knowing what a CLI requires is per-CLI knowledge, and
//! getting it wrong fails as a mysterious startup break rather than a clean
//! refusal. A second agent kind will bring its own keys.

use std::collections::BTreeMap;

/// Claude Code's config-dir selector. Relocating it moves `.claude.json`,
/// stored credentials and history together, which is why a child on the wrong
/// value is a wrong-ACCOUNT bug and not a cosmetic one.
pub const CLAUDE_CONFIG_DIR: &str = "CLAUDE_CONFIG_DIR";

/// How far up the requester's ancestry to look for an attesting environment.
/// Same bound as the pane-ancestry walk in `app::ids` and for the same reason:
/// a cycle or a very deep tree must not turn a spawn into a scan.
const MAX_ANCESTRY: usize = 16;

/// Keys this agent's CLI reads that a LOGIN SHELL supplies and the flock
/// server does not have.
///
/// Deliberately narrow. This is not "everything the child might like" — it is
/// the set whose absence silently changes WHICH ACCOUNT the child runs as.
pub fn shell_supplied_keys(agent: crate::detect::Agent) -> &'static [&'static str] {
    match agent {
        crate::detect::Agent::Claude => &[CLAUDE_CONFIG_DIR],
        _ => &[],
    }
}

/// What flock could learn about the profile the child belongs on.
///
/// The three cases are NOT interchangeable, and collapsing the last two is the
/// mistake that turns this fix into a regression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequesterEnv {
    /// Read from the requester's live process, or from flock's own record of
    /// the session being forked.
    Attested(BTreeMap<String, String>),
    /// A requester exists and its environment could not be read. Something may
    /// well be there — this is the dangerous case, because the guess it would
    /// license is "the default profile", i.e. possibly someone else's account.
    Unreadable,
    /// There is no requester to read and nothing recorded: the pane's process
    /// is gone and no `SessionStart` record survives it.
    ///
    /// Deliberately NOT a refusal. Flock has no evidence a profile was ever in
    /// play, and refusing here would stop an operator who has never used
    /// profiles from forking a hibernated agent — a functional regression to
    /// buy nothing, since there is no selector to get wrong.
    Absent,
}

/// Why a spawn could not establish which profile the child belongs on.
///
/// Both variants are terminal. Retrying an identical request cannot make a
/// requester's environment readable, and cannot conjure a profile directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileUnresolved {
    /// Nothing in the requester's ancestry attested an environment, so which
    /// profile it is on is unknowable. Refusing beats guessing: the guess
    /// lands the child on the default profile, which may be a different
    /// authenticated account.
    RequesterUnreadable { pid: Option<u32> },
    /// The selector is set and names a directory that is not there. Spawning
    /// anyway parks the child at a login prompt in a pane nobody is watching,
    /// which reads as "idle" rather than as a failure.
    NoSuchProfile { key: &'static str, value: String },
}

impl ProfileUnresolved {
    /// The stable `data.refusal` tag, shared by every spawn verb so a caller
    /// sees one code regardless of which door it came in through.
    pub fn code(&self) -> &'static str {
        "agent_profile_unresolved"
    }

    /// Whether retrying the identical request could ever succeed.
    pub fn retryable(&self) -> bool {
        false
    }

    pub fn message(&self) -> String {
        match self {
            Self::RequesterUnreadable { pid } => {
                let who = pid.map_or_else(
                    || "the requester".to_string(),
                    |pid| format!("requester pid {pid}"),
                );
                format!(
                    "cannot read {who}'s environment, so the agent profile it runs under is \
                     unknown; refusing rather than starting the child on the default profile, \
                     which may be a different account"
                )
            }
            Self::NoSuchProfile { key, value } => format!(
                "{key} names {value}, which is not a directory; the child would start \
                 unauthenticated and sit at a login prompt"
            ),
        }
    }
}

/// Which agent an argv launches, or `None` for anything flock does not
/// recognise as an agent CLI.
///
/// Resolution goes through argv rather than through [`super::AgentKind`] so
/// every spawn door shares one answer: `agent.spawn` assembles argv from the
/// closed kind, while `agent.start` and both fork paths only ever have argv to
/// hand.
pub fn agent_for_argv(argv: &[String]) -> Option<crate::detect::Agent> {
    let program = argv.first()?;
    let basename = std::path::Path::new(program).file_name()?.to_str()?;
    crate::detect::identify_agent(basename)
}

/// Decide what the child must carry, given what the requester's environment
/// says. Pure: `profile_exists` is injected so the decision is testable
/// without a filesystem.
///
/// An empty result is the common, correct case — an operator who never set a
/// selector has one profile, and the child inherits it by inheriting nothing.
pub fn resolve<F>(
    agent: Option<crate::detect::Agent>,
    requester_env: &RequesterEnv,
    profile_exists: F,
) -> Result<Vec<(String, String)>, ProfileUnresolved>
where
    F: Fn(&str) -> bool,
{
    let keys = agent.map(shell_supplied_keys).unwrap_or_default();
    if keys.is_empty() {
        // Not an agent flock knows to have shell-supplied state. Nothing to
        // carry, and nothing to refuse over.
        return Ok(Vec::new());
    }
    let env = match requester_env {
        RequesterEnv::Attested(env) => env,
        RequesterEnv::Unreadable => {
            return Err(ProfileUnresolved::RequesterUnreadable { pid: None })
        }
        RequesterEnv::Absent => return Ok(Vec::new()),
    };

    let mut inherited = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(value) = env.get(*key).map(String::as_str).filter(|v| !v.is_empty()) else {
            // Unset means the requester is on the CLI's own default, and so
            // is the child. Same profile, nothing to stamp.
            continue;
        };
        if !profile_exists(value) {
            return Err(ProfileUnresolved::NoSuchProfile {
                key,
                value: value.to_string(),
            });
        }
        inherited.push(((*key).to_string(), value.to_string()));
    }
    Ok(inherited)
}

/// A selector flock already recorded, in the shape [`resolve`] reads.
///
/// Claude's `SessionStart` hook runs INSIDE claude, so it sees the live
/// `CLAUDE_CONFIG_DIR` and records `session_id -> config_dir`
/// (`agent_resume::claude_config_dir_for_session`). A fork can therefore ask
/// what profile the session it is forking actually ran under, without a live
/// process to read — which is the case that matters, because a hibernated
/// agent is forkable and has no child pid at all.
///
/// Claude-shaped by construction: the record only ever holds this one key.
pub fn recorded_claude_profile(config_dir: String) -> BTreeMap<String, String> {
    BTreeMap::from([(CLAUDE_CONFIG_DIR.to_string(), config_dir)])
}

/// Read the environment of `start`, walking up its ancestors while nothing
/// attests.
///
/// The walk exists because the requester is not always the process holding the
/// answer: an `flk mcp serve` bridge is a child of the agent, which is a child
/// of the shell that exported the selector, and any of those may be a process
/// whose environment this platform declines to hand over. A child's
/// environment is a copy of its parent's at exec, so an ancestor's answer is
/// the right one when the child's own is unavailable.
///
/// `stop_before` is the flock server's pid. The walk must not reach it: the
/// server's environment is precisely the one that lacks the selector, and
/// past it lie the launcher and `launchd`, which know even less. Stopping
/// there keeps a missing answer a REFUSAL rather than a wrong answer.
pub fn read_requester_env(start: u32, stop_before: u32) -> Option<BTreeMap<String, String>> {
    let mut system = sysinfo::System::new();
    let refresh = sysinfo::ProcessRefreshKind::nothing().with_environ(sysinfo::UpdateKind::Always);
    let mut current = start;
    for _ in 0..MAX_ANCESTRY {
        if current == stop_before || current <= 1 {
            return None;
        }
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(current)]),
            true,
            refresh,
        );
        let process = system.process(sysinfo::Pid::from_u32(current))?;
        let environ = process.environ();
        if !environ.is_empty() {
            return Some(parse_environ(environ));
        }
        current = process.parent()?.as_u32();
    }
    None
}

/// Split `KEY=VALUE` entries. Entries that are not valid UTF-8, or carry no
/// `=`, are dropped rather than failing the whole read — one odd entry must
/// not cost the profile selector sitting next to it.
fn parse_environ(entries: &[std::ffi::OsString]) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter_map(|entry| {
            let text = entry.to_str()?;
            let (key, value) = text.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;

    fn attested(pairs: &[(&str, &str)]) -> RequesterEnv {
        RequesterEnv::Attested(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        )
    }

    fn always(_: &str) -> bool {
        true
    }

    fn never(_: &str) -> bool {
        false
    }

    /// The bug, stated as a test: an argv-spawned claude must be handed the
    /// same profile the requester is on. Without the carry it starts against
    /// `~/.claude`, which may be a different authenticated account.
    #[test]
    fn a_claude_child_carries_the_requesters_profile() {
        let env = attested(&[("CLAUDE_CONFIG_DIR", "/profiles/work"), ("PATH", "/bin")]);
        let inherited = resolve(Some(Agent::Claude), &env, always).expect("resolved");
        assert_eq!(
            inherited,
            vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/profiles/work".to_string()
            )],
            "the selector, and only the selector, is carried across"
        );
    }

    /// The common case on a machine that never selected a profile. Requester
    /// and child both run against the CLI's own default, so carrying nothing
    /// is what "same profile" means here — a refusal would be a regression
    /// for everyone not using profiles at all.
    #[test]
    fn an_unset_selector_is_not_a_refusal() {
        let env = attested(&[("PATH", "/bin")]);
        assert_eq!(resolve(Some(Agent::Claude), &env, always), Ok(Vec::new()));
    }

    /// An empty value is a degenerate unset, not a profile named "".
    #[test]
    fn an_empty_selector_is_treated_as_unset() {
        let env = attested(&[("CLAUDE_CONFIG_DIR", "")]);
        assert_eq!(
            resolve(Some(Agent::Claude), &env, never),
            Ok(Vec::new()),
            "an empty value must not be probed as a directory, nor refused"
        );
    }

    /// The failure the issue title names: a child parked at a login prompt in
    /// a pane nobody is watching looks merely idle. Refuse at spawn instead.
    #[test]
    fn a_selector_naming_no_directory_refuses_rather_than_spawning() {
        let env = attested(&[("CLAUDE_CONFIG_DIR", "/profiles/gone")]);
        let refusal = resolve(Some(Agent::Claude), &env, never).expect_err("must not spawn");
        assert_eq!(refusal.code(), "agent_profile_unresolved");
        assert!(
            !refusal.retryable(),
            "a missing profile directory will not appear by polling"
        );
        assert!(
            refusal.message().contains("/profiles/gone"),
            "the refusal must name the path so an operator can fix it: {}",
            refusal.message()
        );
    }

    /// A requester whose environment cannot be read is the dangerous case: a
    /// process IS there, so a selector may be there with it, and the guess
    /// this would license is the default profile — possibly someone else's
    /// account.
    #[test]
    fn an_unreadable_requester_refuses_instead_of_defaulting() {
        let refusal = resolve(Some(Agent::Claude), &RequesterEnv::Unreadable, always)
            .expect_err("must not spawn");
        assert_eq!(refusal.code(), "agent_profile_unresolved");
        assert!(!refusal.retryable());
    }

    /// The case that must NOT refuse, and the reason `RequesterEnv` has three
    /// arms rather than two. A pane whose process is gone and whose session
    /// left no record is not evidence of a profile flock is about to get
    /// wrong — it is evidence of nothing. Refusing here would stop an operator
    /// who has never used profiles from forking a hibernated agent.
    #[test]
    fn an_absent_requester_is_not_a_refusal() {
        assert_eq!(
            resolve(Some(Agent::Claude), &RequesterEnv::Absent, never),
            Ok(Vec::new())
        );
    }

    /// An agent with no shell-supplied state has nothing to refuse over —
    /// the per-agent table is what keeps this from becoming a global gate
    /// that breaks every non-claude spawn.
    #[test]
    fn an_agent_with_no_shell_supplied_keys_never_refuses() {
        assert!(shell_supplied_keys(Agent::Codex).is_empty());
        assert_eq!(
            resolve(Some(Agent::Codex), &RequesterEnv::Unreadable, never),
            Ok(Vec::new())
        );
        assert_eq!(
            resolve(None, &RequesterEnv::Unreadable, never),
            Ok(Vec::new())
        );
    }

    /// A fork of a hibernated agent has no live process to read, so the
    /// recorded selector is the only answer — and it must be the same answer.
    #[test]
    fn a_recorded_profile_resolves_the_same_as_a_live_one() {
        let recorded =
            RequesterEnv::Attested(recorded_claude_profile("/profiles/work".to_string()));
        assert_eq!(
            resolve(Some(Agent::Claude), &recorded, always),
            Ok(vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/profiles/work".to_string()
            )])
        );
    }

    #[test]
    fn argv_identifies_the_agent_through_a_path_or_a_bare_name() {
        assert_eq!(
            agent_for_argv(&["claude".to_string(), "prompt".to_string()]),
            Some(Agent::Claude)
        );
        assert_eq!(
            agent_for_argv(&["/opt/tools/claude".to_string()]),
            Some(Agent::Claude)
        );
        assert_eq!(agent_for_argv(&["cat".to_string()]), None);
        assert_eq!(agent_for_argv(&[]), None);
    }

    #[test]
    fn environ_entries_without_an_equals_sign_do_not_cost_their_neighbours() {
        let entries = vec![
            std::ffi::OsString::from("malformed"),
            std::ffi::OsString::from("CLAUDE_CONFIG_DIR=/profiles/work"),
        ];
        let parsed = parse_environ(&entries);
        assert_eq!(
            parsed.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/profiles/work")
        );
        assert_eq!(parsed.len(), 1);
    }

    /// The walk must never fall through to the server's own environment: that
    /// is the environment which lacks the selector, so reading it would turn
    /// "cannot tell" into a confident wrong answer.
    #[test]
    fn the_ancestry_walk_stops_before_the_server() {
        let me = std::process::id();
        assert_eq!(read_requester_env(me, me), None);
    }

    /// End to end on a real process, because the pure tests above cannot see
    /// whether this platform actually hands over another process's
    /// environment — and if it does not, every claude spawn refuses.
    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_live_child_process_environment_is_readable() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .env("FLOCK_TEST_PROFILE_PROBE", "/profiles/probe")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a probe child");

        let mut seen = None;
        for _ in 0..50 {
            if let Some(env) = read_requester_env(child.id(), 0) {
                if let Some(value) = env.get("FLOCK_TEST_PROFILE_PROBE") {
                    seen = Some(value.clone());
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(
            seen.as_deref(),
            Some("/profiles/probe"),
            "this platform must expose a child's environment, or every claude \
             spawn refuses with agent_profile_unresolved"
        );
    }
}
