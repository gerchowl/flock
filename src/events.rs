//! Internal app events delivered via channel.
//!
//! Background tasks (PTY child watchers, future hook listeners, etc.) send
//! events to the main loop through this channel. No polling needed.

use std::time::Instant;

use crate::detect::{Agent, AgentState};
use crate::layout::PaneId;
use crate::workspace::{GitStatusCacheEntry, WorkspaceGitStatus};

#[derive(Debug)]
pub struct WorktreeAddResult {
    pub path: std::path::PathBuf,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct WorktreeKillGateResult {
    pub workspace_id: String,
    pub path: std::path::PathBuf,
    pub branch: Option<String>,
    pub gate: crate::worktree::WorktreeMergeGate,
    /// The merge gate hit its wall-clock bound (`gh`/git wedged) and degraded
    /// to the safe `NotMerged` (#119). Drives the dialog's "unknown (timed
    /// out)" note so the checkout-only fallback reads as intentional, not a
    /// genuine "no merge evidence" verdict.
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeBranchDeleteResult {
    pub branch: String,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRemoveResult {
    pub workspace_id: String,
    pub path: std::path::PathBuf,
    pub result: Result<(), String>,
}

/// Outcome of the fleet-wide kill sweep's git work (#81): one entry per linked
/// row that was acted on, keyed by workspace id. `Ok` means the checkout (and,
/// for merged rows, the branch) was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeKillAllResult {
    pub outcomes: Vec<(String, Result<(), String>)>,
}

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
    },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        /// Free-text activity from the agent's status line while Working.
        activity: Option<String>,
        visible_blocker: bool,
        visible_idle: bool,
        visible_working: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Periodic machine metrics snapshot for the global status line.
    SystemStatsUpdated(crate::system_stats::SystemStats),
    /// The slow PR-state poll interval elapsed; collect targets and query gh.
    PrStatePollDue,
    /// Background gh results: per-workspace PR state for worktree branches.
    PrStatesUpdated(Vec<(String, Option<crate::worktree::PrStateInfo>)>),
    /// The peer-summary poll interval elapsed; spawn SSH fetches per peer.
    PeerPollDue,
    /// Background SSH result: one peer's federated summary (or error).
    PeerSummaryFetched(crate::peers::PeerSummaryFetch),
    /// Cross-machine checkout (#125), read-only probe (`push=false`): the peer
    /// reported its branch's working-tree / push state, feeding the confirm
    /// dialog before any mutation. `generation` discards a stale leg whose
    /// checkout was cancelled (or superseded) while it was in flight.
    PeerCheckoutProbed {
        generation: u64,
        result: Result<crate::peers::PeerCheckoutOutcome, String>,
    },
    /// Cross-machine checkout (#125), push leg (`push=true`): the peer pushed
    /// the branch to origin so the hub can fetch it.
    PeerCheckoutPushed {
        generation: u64,
        result: Result<crate::peers::PeerCheckoutOutcome, String>,
    },
    /// Cross-machine checkout (#125), local leg: the hub fetched the branch
    /// from origin and added a worktree at this path (or failed).
    PeerCheckoutWorktreeReady {
        generation: u64,
        result: Result<std::path::PathBuf, String>,
    },
    /// A user prompt submitted to an agent pane (integration hook report).
    HookPromptReported {
        pane_id: PaneId,
        prompt: String,
    },
    /// A recap entry reported for an agent pane (e.g. Claude Stop hook). The
    /// API simply stores it into the pane's prompt history; recaps render
    /// visually distinct from prompts.
    HookRecapReported {
        pane_id: PaneId,
        recap: String,
    },
    /// A reply entry reported for an agent pane (the agent's last assistant
    /// message, captured by the same Stop hook that fires `HookRecapReported`).
    /// Renders distinct from prompts and recaps in the prompt-history float so
    /// the user can scan the conversation, not just their own side of it.
    HookReplyReported {
        pane_id: PaneId,
        reply: String,
    },
    /// A session promoted (or refreshed) a header field for its own pane.
    PaneHeaderFieldSet {
        pane_id: PaneId,
        key: String,
        value: String,
        ttl: Option<std::time::Duration>,
    },
    /// A session cleared one of its pane's promoted header fields.
    PaneHeaderFieldCleared {
        pane_id: PaneId,
        key: String,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        custom_status: Option<String>,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<crate::agent_resume::AgentSessionRef>,
        /// Normalized Claude Code `SessionStart` source field
        /// (`startup`/`resume`/`clear`/`compact`). `startup` reports are
        /// treated as nested session noise and never replace an existing
        /// restored session id; the other values are real identity changes.
        session_start_source: Option<String>,
    },
    /// Display-only agent metadata was reported for a pane.
    HookMetadataReported {
        pane_id: PaneId,
        source: String,
        agent_label: Option<String>,
        applies_to_source: Option<String>,
        title: Option<String>,
        display_agent: Option<String>,
        custom_status: Option<String>,
        state_labels: std::collections::HashMap<String, String>,
        clear_title: bool,
        clear_display_agent: bool,
        clear_custom_status: bool,
        clear_state_labels: bool,
        seq: Option<u64>,
        ttl: Option<std::time::Duration>,
    },
    /// Hook authority was explicitly cleared for a pane.
    HookAuthorityCleared {
        pane_id: PaneId,
        source: Option<String>,
        seq: Option<u64>,
    },
    /// The current detected agent gracefully released this pane back to the shell.
    HookAgentReleased {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        known_agent: Option<Agent>,
        seq: Option<u64>,
    },
    /// A new version is available through the active installation manager.
    UpdateReady {
        version: String,
        install_command: String,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it through flock's own clipboard writer.
    ClipboardWrite {
        content: Vec<u8>,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
    /// Background `git worktree add` completed.
    WorktreeAddFinished(WorktreeAddResult),
    /// Background `git worktree remove` completed.
    WorktreeRemoveFinished(WorktreeRemoveResult),
    WorktreeKillGateFinished(WorktreeKillGateResult),
    WorktreeBranchDeleteFinished(WorktreeBranchDeleteResult),
    WorktreeKillAllFinished(WorktreeKillAllResult),
}
