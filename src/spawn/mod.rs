//! Agent-initiated spawn: the closed verb and its ceiling (#329, ADR-0014).
//!
//! `agent.fork` lets an agent clone its own conversation into a new worktree.
//! Dispatch needs the opposite — a FRESH child — because a fork copies the
//! parent's whole transcript, and a dispatcher's transcript is never small.
//!
//! The reason this is a new verb rather than `agent.start` exposed over MCP is
//! that `AgentStartParams.argv` is a raw `Vec<String>`: a constraint that lives
//! in an MCP builder is one refactor from being defeated, because the type
//! still permits the bad shape. Here argv is assembled server-side from a
//! CLOSED [`AgentKind`], the same posture `checks::ActionSpec` takes.
//!
//! # The ceiling
//!
//! A single global cap is a footgun on its own: a runaway subtree occupies
//! every slot but one, starves the dispatcher that would have cleaned up, and
//! pushes it to fork instead — turning the cap into a fleet-wide denial
//! switch. Three limits apply, innermost first, so the refusal names the
//! actual constraint rather than always blaming capacity:
//!
//! 1. **depth** — how deep in the spawn tree the child would sit
//! 2. **fanout** — how many live children one parent already has
//! 3. **capacity** — how many agent-initiated agents are live fleet-wide
//!
//! Depth and fanout are answered from state stamped on each terminal at spawn
//! (`spawn_depth`, `spawned_by`), so the check is O(live panes) and never
//! walks the durable event log on a request path.

pub mod allowlist;
pub mod env;
pub mod prompt;

use serde::{Deserialize, Serialize};

/// Which agent a spawn may launch. CLOSED — a free string would let a caller
/// select a weaker-sandboxed profile, or name a binary outright. Adding an
/// agent is a variant here plus its argv assembly, never a config string that
/// reaches a shell (ADR-0014 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
}

impl AgentKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    /// Every kind a caller may name. Echoed in the `unknown_agent_kind`
    /// refusal so a caller learns the set without a docs round-trip.
    pub fn supported() -> &'static [&'static str] {
        &["claude"]
    }

    /// Assemble the child's argv. This is the whole point of the closed enum:
    /// the caller supplies a PROMPT, never a command line.
    ///
    /// The prompt is passed as a single positional argument, not interpolated
    /// into a string — there is no shell between here and `execvp`, so a
    /// prompt containing quotes or newlines is inert.
    ///
    /// It arrives as a [`SpawnPrompt`] rather than a `&str` because that type
    /// is the only thing that has flock's preamble in front of it (ADR-0014
    /// §4). A `&str` parameter would let a future caller assemble an argv
    /// around raw caller text and still typecheck, which is the shape §1
    /// refused `agent.start` over.
    ///
    /// [`SpawnPrompt`]: prompt::SpawnPrompt
    pub fn argv(self, prompt: &prompt::SpawnPrompt) -> Vec<String> {
        match self {
            Self::Claude => vec!["claude".to_string(), prompt.as_argv_element().to_string()],
        }
    }
}

/// Why a spawn was refused. Every variant is machine-readable and carries
/// enough for a caller to decide between backing off and giving up — an agent
/// that cannot tell a transient refusal from a permanent one will retry-loop
/// (ADR-0014 §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    /// Retry is safe once something exits.
    AtCapacity { current: usize, limit: usize },
    /// Retry is safe once this parent's children exit.
    AtFanout { current: usize, limit: usize },
    /// Retry is NEVER safe from this caller — it is too deep in the tree.
    AtDepth { depth: u32, limit: u32 },
    /// The fleet is paused. Retry is safe once an operator resumes.
    FleetPaused,
    /// Agent-initiated spawn is not enabled on this node. Terminal.
    NotEnabled,
    /// The child's agent profile could not be established from the requester
    /// (#359). Terminal — see [`env::ProfileUnresolved`] for why each case is
    /// a refusal rather than a default.
    ProfileUnresolved(env::ProfileUnresolved),
}

impl SpawnRefusal {
    /// The stable `data.refusal` tag.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AtCapacity { .. } => "at_agent_capacity",
            Self::AtFanout { .. } => "at_fanout_limit",
            Self::AtDepth { .. } => "at_lineage_depth",
            Self::FleetPaused => "fleet_paused",
            Self::NotEnabled => "agent_spawn_disabled",
            Self::ProfileUnresolved(unresolved) => unresolved.code(),
        }
    }

    /// Whether retrying the identical request could ever succeed. A caller
    /// that ignores this and polls a terminal refusal is the failure mode the
    /// split exists to prevent.
    pub fn retryable(&self) -> bool {
        match self {
            Self::AtCapacity { .. } | Self::AtFanout { .. } | Self::FleetPaused => true,
            Self::AtDepth { .. } | Self::NotEnabled => false,
            // Delegated rather than restated: whether a profile refusal can be
            // retried is a property of the profile failure, not of this enum.
            Self::ProfileUnresolved(unresolved) => unresolved.retryable(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::AtCapacity { current, limit } => format!(
                "at agent capacity ({current}/{limit} agent-started agents live); retry when one exits"
            ),
            Self::AtFanout { current, limit } => format!(
                "this agent already has {current} live children (limit {limit}); retry when one exits"
            ),
            Self::AtDepth { depth, limit } => format!(
                "spawn depth {depth} exceeds the limit of {limit}; a child this deep may not spawn again"
            ),
            Self::FleetPaused => {
                "the fleet is paused; agent-initiated spawn is refused until an operator resumes"
                    .to_string()
            }
            Self::NotEnabled => {
                "agent-initiated spawn is disabled; set [fleet] agent_spawn_enabled".to_string()
            }
            Self::ProfileUnresolved(unresolved) => unresolved.message(),
        }
    }
}

/// The live counts a ceiling decision needs. Gathered by the caller from App
/// state so this stays a pure function testable without an App.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpawnCensus {
    /// Agent-initiated agents currently alive fleet-wide on this node.
    pub live_agent_started: usize,
    /// Live children already spawned by the requesting parent.
    pub parent_live_children: usize,
    /// Depth the CHILD would sit at (parent depth + 1).
    pub child_depth: u32,
}

/// `[fleet]` — limits on agent-initiated spawn (#329).
///
/// Off by default. Exposing a spawn verb to agents is precisely the
/// "don't autopilot until the operator opts in" surface that `[checks.reap]`
/// already models, so it ships the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct FleetConfig {
    /// Master switch for `agent.spawn`. OFF by default.
    pub agent_spawn_enabled: bool,
    /// Fleet-wide ceiling on live agent-started agents.
    pub max_concurrent_agents: usize,
    /// How deep the spawn tree may go. A child spawned by the operator sits
    /// at depth 1; its child at 2. The default refuses at 2 because there is
    /// no telemetry to justify more, and widening a limit is a decision while
    /// narrowing one is an incident (ADR-0014).
    pub max_spawn_depth: u32,
    /// How many live children one agent may have at once.
    pub max_spawn_fanout: usize,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            agent_spawn_enabled: false,
            max_concurrent_agents: 8,
            max_spawn_depth: 2,
            max_spawn_fanout: 4,
        }
    }
}

/// Decide whether one agent-initiated spawn may proceed.
///
/// `paused` is the fleet pause switch. Pause deliberately exempts human
/// keystrokes — it halts the scheduler and delivery, not human agency — but an
/// MCP-originated spawn is not a human, so it refuses. This is the first place
/// pause distinguishes caller CLASS rather than mechanism (ADR-0014 §7).
pub fn admit(config: &FleetConfig, census: SpawnCensus, paused: bool) -> Result<(), SpawnRefusal> {
    if !config.agent_spawn_enabled {
        return Err(SpawnRefusal::NotEnabled);
    }
    if paused {
        return Err(SpawnRefusal::FleetPaused);
    }
    // Innermost limit first, so the refusal names the binding constraint. A
    // too-deep caller told "at capacity" would wait for a slot that will
    // never help it.
    if census.child_depth > config.max_spawn_depth {
        return Err(SpawnRefusal::AtDepth {
            depth: census.child_depth,
            limit: config.max_spawn_depth,
        });
    }
    if census.parent_live_children >= config.max_spawn_fanout {
        return Err(SpawnRefusal::AtFanout {
            current: census.parent_live_children,
            limit: config.max_spawn_fanout,
        });
    }
    if census.live_agent_started >= config.max_concurrent_agents {
        return Err(SpawnRefusal::AtCapacity {
            current: census.live_agent_started,
            limit: config.max_concurrent_agents,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> FleetConfig {
        FleetConfig {
            agent_spawn_enabled: true,
            ..FleetConfig::default()
        }
    }

    #[test]
    fn spawn_is_disabled_by_default() {
        assert!(!FleetConfig::default().agent_spawn_enabled);
        let refusal = admit(&FleetConfig::default(), SpawnCensus::default(), false)
            .expect_err("default config refuses");
        assert_eq!(refusal.code(), "agent_spawn_disabled");
        assert!(
            !refusal.retryable(),
            "a disabled node will not become enabled by polling"
        );
    }

    /// Pause exempts human keystrokes on purpose. An agent is not a human,
    /// and a paused fleet that still spawns agents is not paused.
    #[test]
    fn a_paused_fleet_refuses_agent_initiated_spawn() {
        let refusal = admit(&enabled(), SpawnCensus::default(), true).expect_err("paused refuses");
        assert_eq!(refusal.code(), "fleet_paused");
        assert!(
            refusal.retryable(),
            "an operator can resume; backing off is correct"
        );
    }

    /// The ordering is the point. A caller at max depth told "at capacity"
    /// would back off and retry forever, waiting for a slot that cannot help
    /// it — depth is a property of the CALLER, not of the fleet's load.
    #[test]
    fn the_refusal_names_the_binding_constraint_not_just_capacity() {
        let census = SpawnCensus {
            live_agent_started: 99,
            parent_live_children: 99,
            child_depth: 99,
        };
        let refusal = admit(&enabled(), census, false).expect_err("refused");
        assert_eq!(refusal.code(), "at_lineage_depth");
        assert!(
            !refusal.retryable(),
            "depth is a property of the caller; retrying cannot fix it"
        );
    }

    /// Fanout before capacity, for the same reason: one runaway parent must
    /// be told it is the problem rather than blaming the fleet.
    #[test]
    fn fanout_is_reported_before_capacity() {
        let config = enabled();
        let census = SpawnCensus {
            live_agent_started: config.max_concurrent_agents,
            parent_live_children: config.max_spawn_fanout,
            child_depth: 1,
        };
        let refusal = admit(&config, census, false).expect_err("refused");
        assert_eq!(refusal.code(), "at_fanout_limit");
    }

    /// The scenario the whole ceiling exists for: a subtree cannot occupy
    /// every slot and starve the dispatcher, because it hits its own depth
    /// and fanout walls first.
    #[test]
    fn a_recursive_subtree_hits_depth_before_it_can_exhaust_the_fleet() {
        let config = enabled();
        // A child at the depth limit tries to spawn again. Capacity is wide
        // open — plenty of slots — and it is still refused.
        let census = SpawnCensus {
            live_agent_started: 0,
            parent_live_children: 0,
            child_depth: config.max_spawn_depth + 1,
        };
        assert_eq!(
            admit(&config, census, false).expect_err("refused").code(),
            "at_lineage_depth"
        );
    }

    #[test]
    fn a_spawn_within_every_limit_is_admitted() {
        assert!(admit(
            &enabled(),
            SpawnCensus {
                live_agent_started: 1,
                parent_live_children: 1,
                child_depth: 1,
            },
            false
        )
        .is_ok());
    }

    #[test]
    fn capacity_refusal_carries_current_and_limit_so_a_caller_can_back_off() {
        let config = enabled();
        let census = SpawnCensus {
            live_agent_started: config.max_concurrent_agents,
            parent_live_children: 0,
            child_depth: 1,
        };
        match admit(&config, census, false).expect_err("refused") {
            SpawnRefusal::AtCapacity { current, limit } => {
                assert_eq!(current, config.max_concurrent_agents);
                assert_eq!(limit, config.max_concurrent_agents);
            }
            other => panic!("expected capacity refusal, got {other:?}"),
        }
    }

    /// A closed kind is the difference between "pick an agent" and "run a
    /// binary". An unknown name must not fall through to anything.
    #[test]
    fn agent_kind_is_closed() {
        assert_eq!(AgentKind::parse("claude"), Some(AgentKind::Claude));
        for hostile in ["sh", "claude; rm -rf /", "CLAUDE", "", "../claude"] {
            assert_eq!(
                AgentKind::parse(hostile),
                None,
                "{hostile:?} must not parse"
            );
        }
    }

    /// The prompt is a positional argument, never interpolated. There is no
    /// shell between here and exec, so quoting metacharacters is inert.
    #[test]
    fn a_hostile_prompt_stays_one_argument() {
        let raw = "'; rm -rf ~ #\nsecond line";
        let argv = AgentKind::Claude.argv(&prompt::SpawnPrompt::compose(raw).expect("valid"));
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "claude");
        assert!(
            argv[1].contains(raw),
            "the caller's text is carried verbatim, quoting and all"
        );
    }

    /// Every argv this enum assembles carries the preamble, because the only
    /// prompt type it accepts is the one that has already been composed. A
    /// future kind cannot opt out of it without changing this signature.
    #[test]
    fn every_agent_kind_puts_the_preamble_ahead_of_the_task() {
        let composed = prompt::SpawnPrompt::compose("review #42").expect("valid");
        for kind in AgentKind::supported() {
            let kind = AgentKind::parse(kind).expect("supported kinds parse");
            let argv = kind.argv(&composed);
            let turn = argv.last().expect("the prompt is the last element");
            assert!(
                turn.starts_with("[flock] You were started by another agent"),
                "{kind:?} must hand the child the preamble first"
            );
            assert!(turn.contains("review #42"));
        }
    }
}
