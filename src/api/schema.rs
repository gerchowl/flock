use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "ping")]
    Ping(PingParams),
    #[serde(rename = "server.stop")]
    ServerStop(EmptyParams),
    #[serde(rename = "server.live_handoff")]
    ServerLiveHandoff(ServerLiveHandoffParams),
    #[serde(rename = "server.reload_config")]
    ServerReloadConfig(EmptyParams),
    #[serde(rename = "notification.show")]
    NotificationShow(NotificationShowParams),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateParams),
    #[serde(rename = "workspace.list")]
    WorkspaceList(EmptyParams),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceTarget),
    #[serde(rename = "workspace.focus")]
    WorkspaceFocus(WorkspaceTarget),
    #[serde(rename = "workspace.rename")]
    WorkspaceRename(WorkspaceRenameParams),
    #[serde(rename = "workspace.close")]
    WorkspaceClose(WorkspaceTarget),
    #[serde(rename = "worktree.list")]
    WorktreeList(WorktreeListParams),
    #[serde(rename = "worktree.create")]
    WorktreeCreate(WorktreeCreateParams),
    #[serde(rename = "worktree.open")]
    WorktreeOpen(WorktreeOpenParams),
    #[serde(rename = "worktree.remove")]
    WorktreeRemove(WorktreeRemoveParams),
    #[serde(rename = "tab.create")]
    TabCreate(TabCreateParams),
    #[serde(rename = "tab.list")]
    TabList(TabListParams),
    #[serde(rename = "tab.get")]
    TabGet(TabTarget),
    #[serde(rename = "tab.focus")]
    TabFocus(TabTarget),
    #[serde(rename = "tab.rename")]
    TabRename(TabRenameParams),
    #[serde(rename = "tab.close")]
    TabClose(TabTarget),
    #[serde(rename = "peers.summary")]
    PeersSummary(EmptyParams),
    #[serde(rename = "peers.checkout_prepare")]
    PeersCheckoutPrepare(PeersCheckoutPrepareParams),
    #[serde(rename = "agent.list")]
    AgentList(EmptyParams),
    #[serde(rename = "agent.get")]
    AgentGet(AgentTarget),
    #[serde(rename = "agent.read")]
    AgentRead(AgentReadParams),
    #[serde(rename = "agent.send")]
    AgentSend(AgentSendParams),
    #[serde(rename = "agent.rename")]
    AgentRename(AgentRenameParams),
    #[serde(rename = "agent.focus")]
    AgentFocus(AgentTarget),
    #[serde(rename = "agent.start")]
    AgentStart(AgentStartParams),
    #[serde(rename = "agent.fork")]
    AgentFork(AgentForkParams),
    #[serde(rename = "agent.hibernate")]
    AgentHibernate(AgentTarget),
    #[serde(rename = "agent.resume")]
    AgentResume(AgentTarget),
    #[serde(rename = "agent.lineage")]
    AgentLineage(LineageParams),
    #[serde(rename = "msg.send")]
    MsgSend(MsgSendParams),
    #[serde(rename = "msg.reply")]
    MsgReply(MsgReplyParams),
    #[serde(rename = "msg.list")]
    MsgList(MsgListParams),
    #[serde(rename = "pane.split")]
    PaneSplit(PaneSplitParams),
    #[serde(rename = "pane.move")]
    PaneMove(PaneMoveParams),
    #[serde(rename = "pane.list")]
    PaneList(PaneListParams),
    #[serde(rename = "pane.get")]
    PaneGet(PaneTarget),
    #[serde(rename = "pane.rename")]
    PaneRename(PaneRenameParams),
    #[serde(rename = "pane.send_text")]
    PaneSendText(PaneSendTextParams),
    #[serde(rename = "pane.send_keys")]
    PaneSendKeys(PaneSendKeysParams),
    #[serde(rename = "pane.send_input")]
    PaneSendInput(PaneSendInputParams),
    #[serde(rename = "pane.read")]
    PaneRead(PaneReadParams),
    #[serde(rename = "pane.report_agent")]
    PaneReportAgent(PaneReportAgentParams),
    #[serde(rename = "pane.report_agent_session")]
    PaneReportAgentSession(PaneReportAgentSessionParams),
    #[serde(rename = "pane.report_prompt")]
    PaneReportPrompt(PaneReportPromptParams),
    #[serde(rename = "pane.report_recap")]
    PaneReportRecap(PaneReportRecapParams),
    #[serde(rename = "pane.report_reply")]
    PaneReportReply(PaneReportReplyParams),
    #[serde(rename = "pane.report_metadata")]
    PaneReportMetadata(PaneReportMetadataParams),
    #[serde(rename = "pane.set_header_field")]
    PaneSetHeaderField(PaneSetHeaderFieldParams),
    #[serde(rename = "pane.clear_header_field")]
    PaneClearHeaderField(PaneClearHeaderFieldParams),
    #[serde(rename = "pane.clear_agent_authority")]
    PaneClearAgentAuthority(PaneClearAgentAuthorityParams),
    #[serde(rename = "pane.release_agent")]
    PaneReleaseAgent(PaneReleaseAgentParams),
    #[serde(rename = "pane.close")]
    PaneClose(PaneTarget),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(EventsSubscribeParams),
    #[serde(rename = "events.wait")]
    EventsWait(EventsWaitParams),
    #[serde(rename = "pane.wait_for_output")]
    PaneWaitForOutput(PaneWaitForOutputParams),
    #[serde(rename = "integration.install")]
    IntegrationInstall(IntegrationInstallParams),
    #[serde(rename = "integration.uninstall")]
    IntegrationUninstall(IntegrationUninstallParams),
    /// #175 phase 4 CLI surface: list runner + built-in check states.
    #[serde(rename = "checks.list")]
    ChecksList(EmptyParams),
    /// Suppress fires from a named check for the next debounce window
    /// (script checks) or for the current in-flight episodes (built-in).
    #[serde(rename = "checks.ack")]
    ChecksAck(ChecksNamedTarget),
    /// Force a named script check to run right now, out of cadence.
    #[serde(rename = "checks.run")]
    ChecksRun(ChecksNamedTarget),
}

/// Target for `checks.ack` / `checks.run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksNamedTarget {
    pub name: String,
}

/// One row in `checks.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecksListEntry {
    pub name: String,
    /// One of `script`, `blocked_alert`, `hibernation`, `issue_guard`.
    pub kind: String,
    /// `enabled` / `disabled` / `errored` — a coarse human-readable state.
    pub state: String,
    /// Consecutive Fire outcomes (script checks only; 0 for built-ins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_fails: Option<u32>,
    /// Last outcome string (`fire` / `pass` / `error`) or `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EmptyParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationShowParams {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<crate::config::ToastFlockPosition>,
    #[serde(default, skip_serializing_if = "NotificationShowSound::is_none")]
    pub sound: NotificationShowSound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationShowSound {
    #[default]
    None,
    Done,
    Request,
}

impl NotificationShowSound {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn to_sound(self) -> Option<crate::sound::Sound> {
        match self {
            Self::None => None,
            Self::Done => Some(crate::sound::Sound::Done),
            Self::Request => Some(crate::sound::Sound::Request),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationShowReason {
    Shown,
    Disabled,
    RateLimited,
    NoForegroundClient,
    Busy,
}

/// Ask this server to prepare one of its OWN workspaces' branches for a
/// cross-machine checkout (#125). The hub sends the peer's workspace id (from
/// `peers.summary`); the spoke resolves the repo + branch from its own state
/// and acts only on its own git — hub-spoke, the hub never touches peer `.git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeersCheckoutPrepareParams {
    /// Workspace id on this (the answering) server, e.g. "ws_3".
    pub workspace_id: String,
    /// `true` performs `git push -u origin <branch>`; `false` is a read-only
    /// probe for the hub's pre-action confirmation (dirty / unpushed warnings).
    #[serde(default)]
    pub push: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTarget {
    pub workspace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTarget {
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabTarget {
    pub tab_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRenameParams {
    pub workspace_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorktreeOpenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoveParams {
    pub workspace_id: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TabListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabRenameParams {
    pub tab_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTarget {
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReadParams {
    pub target: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default)]
    pub format: ReadFormat,
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSendParams {
    pub target: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRenameParams {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStartParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<SplitDirection>,
    #[serde(default)]
    pub focus: bool,
    pub argv: Vec<String>,
}

/// Fork a pane's agent conversation into a new linked worktree (#175 F1):
/// the socket twin of the TUI `branch_session` flow. The new workspace's
/// root pane resumes a fork of the target's session (`--fork-session`),
/// optionally seeded with a pivot prompt as its opening turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentForkParams {
    /// Pane id, terminal id, or agent name — same grammar as `agent.send`.
    pub target: String,
    /// New branch name; a slug is generated when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Base ref for the new branch (default `HEAD`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Absolute checkout path; derived from the worktree directory when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Custom label for the new workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Pivot prompt injected as the fork's opening turn. Omitted ⇒ the
    /// configured `worktrees.branch_pivot_message` template; empty string ⇒
    /// no seed. `<branch>` resolves to the final branch name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pivot: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

/// Walk the fork ancestry of a pane/worktree (#175 O1, US-4). Target uses
/// the `agent.send` grammar for live panes, and falls back to persisted
/// identities (child pane id, worktree path, branch) for reaped ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageParams {
    pub target: String,
}

/// One fork edge in an ancestry chain, deepest (target) first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageEdge {
    pub seq: u64,
    pub ts_ms: u64,
    pub run_id: String,
    pub agent: String,
    pub seeded: bool,
    pub parent: LineageNode,
    pub child: LineageNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageNode {
    pub pane_id: String,
    pub workspace_id: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Destination of a pane-to-pane message (#175 M1, ADR-0006). Structured on
/// the wire: `:` is already load-bearing in pane ids, member labels, and
/// agent-source labels, so no fourth string grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageTarget {
    /// Bare pane: terminal id, public pane id, or unique agent name.
    Pane { pane: String },
    /// Repo-scoped pane: `repo` matches workspace worktree membership
    /// (repo name); `pane` resolves within only those workspaces.
    RepoPane { repo: String, pane: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgSendParams {
    pub to: MessageTarget,
    /// Sanitized like reported prompts: 16 KiB cap, control sequences
    /// stripped.
    pub body: String,
    /// Client-supplied idempotency key. Minted server-side when omitted —
    /// but then the sender loses at-least-once retry ergonomics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Correlation id of a prior message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
}

/// Reply to a delivered message: routed back to the original sender's pane,
/// no addressing needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsgReplyParams {
    /// The original message's correlation id.
    pub correlation_id: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_correlation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MsgListParams {
    /// Restrict to one recipient pane (any bare-pane target shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessageInfo {
    pub correlation_id: String,
    pub to_pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    pub enqueued_at_ms: u64,
    pub delivery_attempts: u32,
    /// Body preview, truncated for listing.
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSplitParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub target_pane_id: String,
    pub direction: SplitDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneMoveParams {
    pub pane_id: String,
    pub destination: PaneMoveDestination,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneMoveDestination {
    Tab {
        tab_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_pane_id: Option<String>,
        split: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratio: Option<f32>,
    },
    NewTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    NewWorkspace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_label: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneMoveResult {
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneMoveReason>,
    pub previous_pane_id: String,
    pub previous_workspace_id: String,
    pub previous_tab_id: String,
    pub pane: Box<PaneInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_workspace: Option<WorkspaceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_tab: Option<TabInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_tab_id: Option<String>,
    pub focused_pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneMoveReason {
    SameTab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRenameParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSendTextParams {
    pub pane_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSendKeysParams {
    pub pane_id: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSendInputParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLiveHandoffParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_exe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadParams {
    pub pane_id: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default)]
    pub format: ReadFormat,
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    pub state: PaneAgentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportPromptParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    /// The user prompt as submitted to the agent.
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportRecapParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    /// Free-text recap from a session lifecycle hook (e.g. Claude Stop).
    pub recap: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportReplyParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    /// Free-text assistant reply from a session lifecycle hook (e.g. the
    /// last assistant message scraped from Claude's transcript on Stop).
    pub reply: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportAgentSessionParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReportMetadataParams {
    pub pane_id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_to_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default)]
    pub clear_title: bool,
    #[serde(default)]
    pub clear_display_agent: bool,
    #[serde(default)]
    pub clear_custom_status: bool,
    #[serde(default)]
    pub clear_state_labels: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSetHeaderFieldParams {
    pub pane_id: String,
    pub key: String,
    pub value: String,
    /// Auto-expire the field after this many seconds (progress that stops
    /// updating shouldn't lie).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneClearHeaderFieldParams {
    pub pane_id: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneClearAgentAuthorityParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReleaseAgentParams {
    pub pane_id: String,
    pub source: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadSource {
    Visible,
    Recent,
    RecentUnwrapped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReadFormat {
    #[default]
    Text,
    Ansi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsSubscribeParams {
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Subscription {
    #[serde(rename = "workspace.created")]
    WorkspaceCreated {},
    #[serde(rename = "workspace.updated")]
    WorkspaceUpdated {},
    #[serde(rename = "workspace.renamed")]
    WorkspaceRenamed {},
    #[serde(rename = "workspace.closed")]
    WorkspaceClosed {},
    #[serde(rename = "workspace.focused")]
    WorkspaceFocused {},
    #[serde(rename = "tab.created")]
    TabCreated {},
    #[serde(rename = "tab.closed")]
    TabClosed {},
    #[serde(rename = "tab.focused")]
    TabFocused {},
    #[serde(rename = "tab.renamed")]
    TabRenamed {},
    #[serde(rename = "pane.created")]
    PaneCreated {},
    #[serde(rename = "pane.closed")]
    PaneClosed {},
    #[serde(rename = "pane.focused")]
    PaneFocused {},
    #[serde(rename = "pane.moved")]
    PaneMoved {},
    #[serde(rename = "pane.exited")]
    PaneExited {},
    #[serde(rename = "pane.agent_detected")]
    PaneAgentDetected {},
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched {
        pane_id: String,
        source: ReadSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
        r#match: OutputMatch,
        #[serde(default = "default_true")]
        strip_ansi: bool,
    },
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_status: Option<AgentStatus>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsWaitParams {
    pub match_event: EventMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneWaitForOutputParams {
    pub pane_id: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    pub r#match: OutputMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInstallParams {
    pub target: IntegrationTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationUninstallParams {
    pub target: IntegrationTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Kimi,
    Opencode,
    Hermes,
    Qodercli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMatch {
    Substring { value: String },
    Regex { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventMatch {
    WorkspaceCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    WorkspaceUpdated {
        workspace_id: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    TabClosed {
        tab_id: String,
    },
    TabRenamed {
        tab_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    TabFocused {
        tab_id: String,
    },
    PaneCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    PaneClosed {
        pane_id: String,
    },
    PaneFocused {
        pane_id: String,
    },
    PaneMoved {
        pane_id: String,
    },
    PaneOutputChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_revision: Option<u64>,
    },
    PaneExited {
        pane_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        agent_status: AgentStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WorkspaceCreated,
    WorkspaceUpdated,
    WorkspaceClosed,
    WorkspaceRenamed,
    WorkspaceFocused,
    TabCreated,
    TabClosed,
    TabRenamed,
    TabFocused,
    PaneCreated,
    PaneClosed,
    PaneFocused,
    PaneMoved,
    PaneOutputChanged,
    PaneExited,
    PaneAgentDetected,
    PaneAgentStatusChanged,
    AgentForked,
    MessageQueued,
    MessageDelivered,
    MessageReplied,
    /// #175 phase 4 check-runner telemetry.
    CheckRan,
    CheckFired,
    CheckErrored,
    ChecksHeartbeat,
    /// #175 S1: scheduled cron predicate fired. See
    /// `EventData::CronFired` for the payload shape.
    CronFired,
    /// #175 S2: a worktree was atomically moved into the session
    /// quarantine (branch preserved). See `EventData::WorktreeQuarantined`.
    WorktreeQuarantined,
    /// #175 C3: hibernation lifecycle. `AgentHibernated` fires once when a
    /// pane transitions to hibernated (child asked to exit + resume plan
    /// stashed). `AgentResumedFromHibernation` fires when the plan runs.
    AgentHibernated,
    AgentResumedFromHibernation,
    /// #175 C4 issue-guard: `TriggerFired` on a matched owner-authored
    /// trigger, `TriggerIgnored` on a non-owner post (audit trail),
    /// `TriggerErrored` on a bad fence / YAML.
    TriggerFired,
    TriggerIgnored,
    TriggerErrored,
}

impl EventKind {
    /// Whether this kind is written to the durable event log (#175 O1).
    /// Exhaustive on purpose: a new kind must make this decision explicitly.
    /// `PaneOutputChanged` fires per terminal revision — far too hot for an
    /// audit log, and the pane's textual state is not part of the audit
    /// story; every lifecycle + lineage kind persists.
    pub fn is_persisted(self) -> bool {
        match self {
            Self::WorkspaceCreated
            | Self::WorkspaceUpdated
            | Self::WorkspaceClosed
            | Self::WorkspaceRenamed
            | Self::WorkspaceFocused
            | Self::TabCreated
            | Self::TabClosed
            | Self::TabRenamed
            | Self::TabFocused
            | Self::PaneCreated
            | Self::PaneClosed
            | Self::PaneFocused
            | Self::PaneMoved
            | Self::PaneExited
            | Self::PaneAgentDetected
            | Self::PaneAgentStatusChanged
            | Self::AgentForked
            | Self::MessageQueued
            | Self::MessageDelivered
            | Self::MessageReplied
            | Self::CheckRan
            | Self::CheckFired
            | Self::CheckErrored
            | Self::ChecksHeartbeat
            | Self::CronFired
            | Self::WorktreeQuarantined
            | Self::AgentHibernated
            | Self::AgentResumedFromHibernation
            | Self::TriggerFired
            | Self::TriggerIgnored
            | Self::TriggerErrored => true,
            Self::PaneOutputChanged => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub id: String,
    pub result: ResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub id: String,
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub live_handoff: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseResult {
    Pong {
        version: String,
        protocol: u32,
        #[serde(default)]
        capabilities: Option<ServerCapabilities>,
    },
    NotificationShow {
        shown: bool,
        reason: NotificationShowReason,
    },
    PeersSummary {
        /// Short hostname of the answering server.
        host: String,
        /// flock version string of the answering server (spot un-deployed peers).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// Wire protocol of the answering server (#58) — lets the poller flag
        /// protocol skew, the mismatch that actually blocks `--remote`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol: Option<u32>,
        /// Self-declared fleet icon NAME of the answering server (#164): a
        /// semantic name (`"laptop"`) the RECEIVER maps to a flat Nerd Font
        /// glyph, so every viewer renders the same server icon. Only an ASCII
        /// name crosses the wire; unknown/absent → no icon. Additive/default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<String>,
        /// Machine health snapshot, piggybacked from the peer's existing
        /// status-line sampler (no extra sampling cost).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system: Option<PeerSystemSummary>,
        workspaces: Vec<PeerWorkspaceSummary>,
        /// Gossip v3 (#101): the answering server's OWN polled peers, so the
        /// polling hub can render two-hop fleet visibility (hub polls spoke2,
        /// spoke1 attaches to hub, spoke1 sees spoke2). One-hop relay only —
        /// the answering server never re-emits entries it received via relay.
        /// Additive with a default so v(N-1) peers degrade gracefully.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        relayed_fleet: Vec<RelayedFleetPeer>,
    },
    PeersCheckoutPrepared {
        /// The branch the spoke prepared (resolved from the workspace), so the
        /// hub fetches exactly what was pushed.
        branch: String,
        /// The peer's working tree had uncommitted changes (not transferred).
        was_dirty: bool,
        /// The branch had unpushed commits / no upstream before this ran.
        was_unpushed: bool,
        /// A push to origin was performed by this call (`push: true`).
        pushed: bool,
    },
    WorkspaceInfo {
        workspace: WorkspaceInfo,
    },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    WorkspaceList {
        workspaces: Vec<WorkspaceInfo>,
    },
    WorktreeList {
        source: WorktreeSourceInfo,
        worktrees: Vec<WorktreeInfo>,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        path: String,
        forced: bool,
    },
    TabInfo {
        tab: TabInfo,
    },
    TabCreated {
        tab: TabInfo,
        root_pane: PaneInfo,
    },
    TabList {
        tabs: Vec<TabInfo>,
    },
    AgentInfo {
        agent: AgentInfo,
    },
    AgentStarted {
        agent: AgentInfo,
        argv: Vec<String>,
    },
    AgentForked {
        /// Unique id for this fork, stamped on the lineage event (#175 O2).
        run_id: String,
        /// The pane whose session was forked, as a public pane id.
        parent_pane_id: String,
        workspace: WorkspaceInfo,
        tab: TabInfo,
        root_pane: PaneInfo,
        worktree: WorktreeInfo,
        argv: Vec<String>,
        /// Whether a pivot prompt was injected as the fork's opening turn.
        seeded: bool,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    Lineage {
        chain: Vec<LineageEdge>,
    },
    MsgQueued {
        correlation_id: String,
        /// "queued" | "duplicate"
        state: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<String>,
    },
    MsgList {
        messages: Vec<QueuedMessageInfo>,
    },
    PaneInfo {
        pane: PaneInfo,
    },
    PaneMove {
        move_result: PaneMoveResult,
    },
    PaneList {
        panes: Vec<PaneInfo>,
    },
    PaneRead {
        read: PaneReadResult,
    },
    SubscriptionStarted {},
    WaitMatched {
        event: EventEnvelope,
    },
    OutputMatched {
        pane_id: String,
        revision: u64,
        matched_line: Option<String>,
        read: PaneReadResult,
    },
    IntegrationInstall {
        target: IntegrationTarget,
        details: IntegrationInstallResult,
    },
    IntegrationUninstall {
        target: IntegrationTarget,
        details: IntegrationUninstallResult,
    },
    ConfigReload {
        status: crate::config::ConfigReloadStatus,
        diagnostics: Vec<String>,
    },
    /// #175 phase 4: reply for `checks.list`.
    ChecksList {
        checks: Vec<ChecksListEntry>,
    },
    Ok {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorkspaceWorktreeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWorktreeInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSourceInfo {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub source_checkout_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
    pub is_linked_worktree: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_workspace_id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub focused: bool,
    pub pane_count: usize,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    /// Whether the operator has seen the agent's latest completed turn
    /// (#175 F3). `agent_status` already folds this bit (`done` is
    /// idle+unseen); the raw value saves clients re-deriving it. Missing on
    /// older servers ⇒ treated as seen.
    #[serde(default = "default_true")]
    pub seen: bool,
    /// Seconds since the agent's semantic state last changed (#175 F3).
    /// Absent when the pane never reported a state transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_secs: Option<u64>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    /// Whether the operator has seen the pane's latest completed turn
    /// (#175 F3). Missing on older servers ⇒ treated as seen.
    #[serde(default = "default_true")]
    pub seen: bool,
    /// Seconds since the pane's semantic agent state last changed (#175 F3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_secs: Option<u64>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadResult {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub source: ReadSource,
    pub format: ReadFormat,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInstallResult {
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationUninstallResult {
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event: EventKind,
    pub data: EventData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscriptionEventKind {
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched,
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionEventEnvelope {
    pub event: SubscriptionEventKind,
    pub data: SubscriptionEventData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubscriptionEventData {
    PaneOutputMatched(PaneOutputMatchedEvent),
    PaneAgentStatusChanged(PaneAgentStatusChangedEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneOutputMatchedEvent {
    pub pane_id: String,
    pub matched_line: String,
    pub read: PaneReadResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentStatusChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        pane: Box<PaneInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_workspace: Option<WorkspaceInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_tab: Option<TabInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_tab_id: Option<String>,
    },
    PaneOutputChanged {
        pane_id: String,
        workspace_id: String,
        revision: u64,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        agent_status: AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_status: Option<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        state_labels: HashMap<String, String>,
    },
    /// Message telemetry (#175 O2 message-side, emitted with the verbs).
    MessageQueued {
        correlation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_pane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_repo: Option<String>,
        to_pane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_repo: Option<String>,
        cross_repo: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        enqueued_at_ms: u64,
        /// Sanitized body — the durable log doubles as the mailbox's
        /// restart source, so the payload must survive here (ADR-0005/0006).
        body: String,
    },
    MessageDelivered {
        correlation_id: String,
        delivered: bool,
        /// "delivered" | "delivered_generic" | "dropped_undeliverable"
        outcome: String,
        delivery_attempts: u32,
        latency_ms: u64,
    },
    MessageReplied {
        /// The original message's correlation id.
        correlation_id: String,
        reply_correlation_id: String,
        reply_latency_ms: u64,
        round_trips: u32,
    },
    /// #175 phase 4 check-runner: one script check completed.
    /// `outcome` is one of `"fire"`, `"pass"`, or `"error"`.
    CheckRan {
        name: String,
        outcome: String,
        duration_ms: u64,
    },
    /// #175 phase 4 check-runner: a check crossed its debounce and the
    /// runner emitted a FireDecision this episode.
    CheckFired {
        name: String,
        episode: String,
    },
    /// #175 phase 4 check-runner: the last outcome was Error; the runner
    /// leaves the debounce counter untouched but records the reason.
    CheckErrored {
        name: String,
        reason: String,
    },
    /// #175 phase 4 check-runner: periodic liveness ping. `runs` and
    /// `errors` are lifetime counters from the App's process start.
    ChecksHeartbeat {
        runs: u64,
        errors: u64,
    },
    /// #175 S1: a scheduled cron predicate fired. `run_id` is the
    /// `run:<name>:<scheduled_ms hex>` shape shared with digest / revert.
    /// `missed_fires` counts scheduled slots that were skipped because the
    /// runner slept past them (asleep-collapse; §8.4).
    CronFired {
        name: String,
        run_id: String,
        scheduled_ms: u64,
        actual_ms: u64,
        missed_fires: u32,
    },
    /// #175 S2: a worktree was atomically moved into the session quarantine
    /// directory. The branch stays local; the checkout can be restored via
    /// `flk worktree unquarantine`. `reason` is the classification fact that
    /// tripped the scheduled reap (dirty / unpushed / detached / stash).
    WorktreeQuarantined {
        workspace_id: String,
        path: String,
        reason: String,
    },
    /// #175 C3: a pane's agent process has been asked to exit and its resume
    /// plan is stashed on the pane; the next focus (or explicit
    /// `agent.resume`) respawns it into the same pane.
    AgentHibernated {
        pane_id: String,
        workspace_id: String,
        agent: String,
        /// The persisted session identity being hibernated, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// #175 C3: the stashed resume plan has been spawned back into the pane.
    AgentResumedFromHibernation {
        pane_id: String,
        workspace_id: String,
        agent: String,
    },
    /// #175 C4 issue-guard: an owner-authored `flk-trigger` block matched
    /// and dispatched. `dedupe_key` is the same key the runner uses to
    /// avoid firing twice on a re-poll (run_id when the block sets one,
    /// hash otherwise).
    TriggerFired {
        repo: String,
        issue: u64,
        dedupe_key: String,
        action: String,
    },
    /// #175 C4 issue-guard: a non-owner post; recorded for the audit
    /// trail but nothing is dispatched.
    TriggerIgnored {
        repo: String,
        issue: u64,
        reason: String,
    },
    /// #175 C4 issue-guard: the body carried a broken `flk-trigger`
    /// block; a comment was posted back on the issue explaining the
    /// error (unless `gh comment` itself failed — captured in `reason`).
    TriggerErrored {
        repo: String,
        issue: u64,
        reason: String,
    },
    /// Fork lineage edge + telemetry (#175 O1/O2, emitted with the verb per
    /// the epic's telemetry design). One event per `agent.fork`.
    AgentForked {
        run_id: String,
        parent_pane_id: String,
        parent_workspace_id: String,
        /// The shared repo key (git common dir) both sides belong to.
        parent_repo: String,
        agent: String,
        child_workspace_id: String,
        child_pane_id: String,
        child_worktree: String,
        child_branch: String,
        /// Whether a pivot prompt seeded the fork's opening turn.
        seeded: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneAgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

/// Machine health for a federated peer's `servers` sidebar row. Sourced from
/// the peer's existing 2s status-line sampler — no extra measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSystemSummary {
    /// Global CPU utilization, 0..=100 (rounded; keeps the response `Eq`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total: Option<u64>,
    /// Free space on the volume holding $HOME, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free: Option<u64>,
}

/// One workspace in a federated peer's `peers.summary` response: just enough
/// for the sidebar's project-folded remote rows and cross-server attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerWorkspaceSummary {
    /// Workspace id on the peer server (e.g. "ws_3"), used to focus it
    /// remotely during switch-on-select.
    #[serde(default)]
    pub id: String,
    /// Workspace display name (custom name or derived label).
    pub workspace: String,
    /// Machine-independent project identity (normalized origin URL or
    /// "dir:<name>"), used to fold remote rows into local project groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// Repo folder name, labels remote-only project groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub is_linked_worktree: bool,
    /// Short agent label of the workspace's attention-leading pane (e.g. "cc").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub status: AgentStatus,
    /// Seconds since the leading pane's state last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_age_secs: Option<u64>,
    /// Live status-line activity while Working (e.g. "Implementing the parser").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

/// One fleet peer relayed through a `peers.summary` response (#101 gossip v3).
///
/// Provenance is explicit via [`origin`](Self::origin): the SHORT HOST NAME of
/// the polling server that last talked to this peer. Loop prevention rides on
/// this field — a receiver drops any relayed entry whose `origin` equals its
/// own short host, and the answering server NEVER re-relays entries it received
/// via relay (only its own polled peers). Result: entries travel exactly one
/// hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayedFleetPeer {
    /// Peer name (config-owned label on the origin's `[[peers]]`).
    pub name: String,
    /// SSH destination the ORIGIN uses to reach this peer.
    pub ssh_target: String,
    /// Hostname the peer reported about itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// flock version the peer reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Wire protocol the peer reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<PeerSystemSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub workspaces: Vec<PeerWorkspaceSummary>,
    /// Seconds since the origin's last successful poll, at relay capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Short host name of the ORIGIN (the polling server). Loop prevention:
    /// receivers drop entries whose origin matches their own short host.
    pub origin: String,
    /// Gossip v3 (#101 part 2): the origin's report age at relay-capture, in
    /// seconds. FROZEN — the receiver uses it as-is for staleness judgement so
    /// a carried entry stays fresh as long as the origin's assertion is fresh.
    /// Additive with `#[serde(default)]` so a v(N-1) peer's relay entries
    /// (never emitting this field) parse as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_last_ok_secs: Option<u64>,
    /// Gossip v3 (#101 part 3): SSH ProxyJump identity for reaching this
    /// peer via THIS hub. Stamped by the hub on relay so a receiver's next
    /// switch dial can route `ssh -o ProxyJump=<value>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    /// The relayed peer's SELF-DECLARED fleet icon name (#164), carried through
    /// the one-hop relay so a two-hop viewer sees the same glyph. Additive with
    /// `#[serde(default)]` so a v(N-1) hub's relay entries parse as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
    /// #175 C3: agent process is gone but a resume plan is stashed on the
    /// pane; the next focus (or explicit `agent.resume`) respawns it.
    /// Ranked at the Idle attention tier — hibernated is a settled state.
    Hibernated,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_for_pane_read() {
        let request = Request {
            id: "req_1".into(),
            method: Method::PaneRead(PaneReadParams {
                pane_id: "p_1".into(),
                source: ReadSource::Recent,
                lines: Some(80),
                format: ReadFormat::Text,
                strip_ansi: true,
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_pane_report_agent() {
        let request = Request {
            id: "req_hook".into(),
            method: Method::PaneReportAgent(PaneReportAgentParams {
                pane_id: "1-1".into(),
                source: "flock:pi".into(),
                agent: "pi".into(),
                state: PaneAgentState::Working,
                message: Some("thinking".into()),
                custom_status: Some("indexing".into()),
                seq: Some(42),
                agent_session_id: Some("pi-session".into()),
                agent_session_path: Some("/tmp/pi-session.jsonl".into()),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_pane_report_agent_session() {
        let request = Request {
            id: "req_session".into(),
            method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
                pane_id: "1-1".into(),
                source: "flock:claude".into(),
                agent: "claude".into(),
                seq: Some(42),
                agent_session_id: Some("claude-session".into()),
                agent_session_path: None,
                session_start_source: None,
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_pane_report_metadata() {
        let request = Request {
            id: "req_metadata".into(),
            method: Method::PaneReportMetadata(PaneReportMetadataParams {
                pane_id: "1-1".into(),
                source: "user:claude-title".into(),
                agent: Some("claude".into()),
                applies_to_source: Some("flock:claude".into()),
                title: Some("Refactor auth".into()),
                display_agent: Some("Claude auth".into()),
                custom_status: Some("refactor auth".into()),
                state_labels: HashMap::from([("working".into(), "deep in the mines".into())]),
                clear_title: false,
                clear_display_agent: false,
                clear_custom_status: false,
                clear_state_labels: false,
                seq: Some(42),
                ttl_ms: Some(3_600_000),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_pane_clear_agent_authority() {
        let request = Request {
            id: "req_clear".into(),
            method: Method::PaneClearAgentAuthority(PaneClearAgentAuthorityParams {
                pane_id: "1-1".into(),
                source: Some("flock:pi".into()),
                seq: Some(42),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_pane_release_agent() {
        let request = Request {
            id: "req_release".into(),
            method: Method::PaneReleaseAgent(PaneReleaseAgentParams {
                pane_id: "1-1".into(),
                source: "flock:pi".into(),
                agent: "pi".into(),
                seq: Some(42),
            }),
        };

        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn peers_checkout_prepare_request_and_response_round_trip() {
        let request = Request {
            id: "cli:peers:checkout_prepare".into(),
            method: Method::PeersCheckoutPrepare(PeersCheckoutPrepareParams {
                workspace_id: "ws_3".into(),
                push: true,
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "peers.checkout_prepare");
        let restored: Request = serde_json::from_value(json).unwrap();
        assert_eq!(restored, request);

        // `push` defaults to false (probe) when omitted.
        let probe: Request = serde_json::from_str(
            r#"{"id":"x","method":"peers.checkout_prepare","params":{"workspace_id":"ws_3"}}"#,
        )
        .unwrap();
        let Method::PeersCheckoutPrepare(params) = probe.method else {
            panic!("wrong method parsed");
        };
        assert!(!params.push);

        let response = SuccessResponse {
            id: "x".into(),
            result: ResponseResult::PeersCheckoutPrepared {
                branch: "feature-x".into(),
                was_dirty: true,
                was_unpushed: true,
                pushed: true,
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"peers_checkout_prepared\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn request_uses_dot_method_names() {
        let request = Request {
            id: "req_1".into(),
            method: Method::WorkspaceCreate(WorkspaceCreateParams {
                cwd: Some("/tmp".into()),
                focus: true,
                label: Some("api".into()),
            }),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "workspace.create");
    }

    #[test]
    fn request_round_trips_for_server_stop() {
        let request = Request {
            id: "req_stop".into(),
            method: Method::ServerStop(EmptyParams::default()),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "server.stop");
        let restored: Request = serde_json::from_value(json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn request_round_trips_for_server_reload_config() {
        let request = Request {
            id: "req_reload".into(),
            method: Method::ServerReloadConfig(EmptyParams::default()),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "server.reload_config");
        let restored: Request = serde_json::from_value(json).unwrap();
        assert_eq!(restored, request);
    }

    #[test]
    fn notification_show_request_parses() {
        let json = r#"{"id":"req_1","method":"notification.show","params":{"title":"build failed","body":"api workspace","position":"top-left","sound":"request"}}"#;
        let request: Request = serde_json::from_str(json).unwrap();
        let Method::NotificationShow(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert_eq!(params.title, "build failed");
        assert_eq!(params.body.as_deref(), Some("api workspace"));
        assert_eq!(
            params.position,
            Some(crate::config::ToastFlockPosition::TopLeft)
        );
        assert_eq!(params.sound, NotificationShowSound::Request);
    }

    #[test]
    fn notification_show_sound_defaults_to_none() {
        let json =
            r#"{"id":"req_1","method":"notification.show","params":{"title":"build failed"}}"#;
        let request: Request = serde_json::from_str(json).unwrap();
        let Method::NotificationShow(params) = request.method else {
            panic!("wrong method parsed");
        };

        assert_eq!(params.sound, NotificationShowSound::None);
    }

    #[test]
    fn unknown_method_is_rejected() {
        let json = r#"{"id":"req_1","method":"nope","params":{}}"#;
        let err = serde_json::from_str::<Request>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant"));
    }

    #[test]
    fn missing_required_params_are_rejected() {
        let json = r#"{"id":"req_1","method":"pane.send_text","params":{"pane_id":"p_1"}}"#;
        let err = serde_json::from_str::<Request>(json)
            .unwrap_err()
            .to_string();
        assert!(err.contains("text"));
    }

    #[test]
    fn pane_send_input_defaults_to_empty_text_and_keys() {
        let json = r#"
        {
            "id": "req_1",
            "method": "pane.send_input",
            "params": {
                "pane_id": "p_1"
            }
        }
        "#;

        let request: Request = serde_json::from_str(json).unwrap();
        let Method::PaneSendInput(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert_eq!(params.pane_id, "p_1");
        assert!(params.text.is_empty());
        assert!(params.keys.is_empty());
    }

    #[test]
    fn pane_wait_for_output_defaults_strip_ansi_to_true() {
        let json = r#"
        {
            "id": "req_1",
            "method": "pane.wait_for_output",
            "params": {
                "pane_id": "p_1",
                "source": "recent",
                "match": { "type": "substring", "value": "ready" }
            }
        }
        "#;

        let request: Request = serde_json::from_str(json).unwrap();
        let Method::PaneWaitForOutput(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert!(params.strip_ansi);
    }

    #[test]
    fn pane_read_defaults_to_text_format() {
        let json = r#"
        {
            "id": "req_1",
            "method": "pane.read",
            "params": {
                "pane_id": "p_1",
                "source": "visible"
            }
        }
        "#;

        let request: Request = serde_json::from_str(json).unwrap();
        let Method::PaneRead(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert_eq!(params.format, ReadFormat::Text);
    }

    #[test]
    fn event_envelope_round_trips() {
        let event = EventEnvelope {
            event: EventKind::PaneOutputChanged,
            data: EventData::PaneOutputChanged {
                pane_id: "p_1".into(),
                workspace_id: "w_1".into(),
                revision: 42,
            },
        };

        let json = serde_json::to_string(&event).unwrap();
        let restored: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, event);
    }

    #[test]
    fn subscribe_request_parses_parameterized_subscriptions() {
        let json = r#"
        {
            "id": "sub_1",
            "method": "events.subscribe",
            "params": {
                "subscriptions": [
                    {
                        "type": "pane.output_matched",
                        "pane_id": "p_1_1",
                        "source": "recent",
                        "lines": 200,
                        "match": { "type": "substring", "value": "auth: received" }
                    },
                    {
                        "type": "pane.agent_status_changed",
                        "pane_id": "p_1_1",
                        "agent_status": "done"
                    }
                ]
            }
        }
        "#;

        let request: Request = serde_json::from_str(json).unwrap();
        let Method::EventsSubscribe(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert_eq!(params.subscriptions.len(), 2);
        assert!(matches!(
            &params.subscriptions[0],
            Subscription::PaneOutputMatched {
                pane_id,
                source: ReadSource::Recent,
                lines: Some(200),
                r#match: OutputMatch::Substring { value },
                strip_ansi: true,
            } if pane_id == "p_1_1" && value == "auth: received"
        ));
        assert!(matches!(
            &params.subscriptions[1],
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status: Some(AgentStatus::Done),
            } if pane_id == "p_1_1"
        ));
    }

    #[test]
    fn subscription_event_envelope_round_trips() {
        let event = SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneOutputMatched,
            data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
                pane_id: "p_1_1".into(),
                matched_line: "auth: received".into(),
                read: PaneReadResult {
                    pane_id: "p_1_1".into(),
                    workspace_id: "w_1".into(),
                    tab_id: "t_1_1".into(),
                    source: ReadSource::Recent,
                    format: ReadFormat::Text,
                    text: "auth: received\n".into(),
                    revision: 0,
                    truncated: false,
                },
            }),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"pane.output_matched\""));
        let restored: SubscriptionEventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, event);
    }

    #[test]
    fn success_response_round_trips() {
        let response = SuccessResponse {
            id: "req_1".into(),
            result: ResponseResult::Pong {
                version: "0.1.2".into(),
                protocol: 6,
                capabilities: Some(ServerCapabilities { live_handoff: true }),
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn worktree_request_and_response_round_trip() {
        let request = Request {
            id: "req_worktree".into(),
            method: Method::WorktreeCreate(WorktreeCreateParams {
                workspace_id: Some("1".into()),
                branch: Some("worktree/api".into()),
                base: Some("HEAD".into()),
                focus: true,
                ..WorktreeCreateParams::default()
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);

        let response = SuccessResponse {
            id: "req_worktree".into(),
            result: ResponseResult::WorktreeCreated {
                workspace: WorkspaceInfo {
                    workspace_id: "w_1".into(),
                    number: 2,
                    label: "flock".into(),
                    focused: true,
                    pane_count: 1,
                    tab_count: 1,
                    active_tab_id: "w_1:1".into(),
                    agent_status: AgentStatus::Unknown,
                    worktree: Some(WorkspaceWorktreeInfo {
                        repo_key: "/repo/flock/.git".into(),
                        repo_name: "flock".into(),
                        repo_root: "/repo/flock".into(),
                        checkout_path: "/worktrees/flock/worktree-api".into(),
                        is_linked_worktree: true,
                    }),
                },
                tab: TabInfo {
                    tab_id: "w_1:1".into(),
                    workspace_id: "w_1".into(),
                    number: 1,
                    label: "flock".into(),
                    focused: true,
                    pane_count: 1,
                    agent_status: AgentStatus::Unknown,
                },
                root_pane: PaneInfo {
                    pane_id: "w_1-1".into(),
                    terminal_id: "term_1".into(),
                    workspace_id: "w_1".into(),
                    tab_id: "w_1:1".into(),
                    focused: true,
                    cwd: Some("/worktrees/flock/worktree-api".into()),
                    foreground_cwd: None,
                    label: None,
                    agent: None,
                    title: None,
                    display_agent: None,
                    agent_status: AgentStatus::Unknown,
                    custom_status: None,
                    state_labels: HashMap::new(),
                    agent_session: None,
                    seen: true,
                    status_age_secs: None,
                    revision: 0,
                },
                worktree: WorktreeInfo {
                    path: "/worktrees/flock/worktree-api".into(),
                    branch: Some("worktree/api".into()),
                    is_bare: false,
                    is_detached: false,
                    is_prunable: false,
                    is_linked_worktree: true,
                    open_workspace_id: Some("w_1".into()),
                    label: "flock".into(),
                },
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"worktree_created\""));
        assert!(json.contains("\"worktree\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn create_response_round_trips_with_root_pane() {
        let response = SuccessResponse {
            id: "req_2".into(),
            result: ResponseResult::TabCreated {
                tab: TabInfo {
                    tab_id: "w_1:2".into(),
                    workspace_id: "w_1".into(),
                    number: 2,
                    label: "review".into(),
                    focused: false,
                    pane_count: 1,
                    agent_status: AgentStatus::Unknown,
                },
                root_pane: PaneInfo {
                    pane_id: "w_1-3".into(),
                    terminal_id: "term_example".into(),
                    workspace_id: "w_1".into(),
                    tab_id: "w_1:2".into(),
                    focused: false,
                    cwd: Some("/tmp/review".into()),
                    foreground_cwd: None,
                    label: None,
                    agent: None,
                    title: None,
                    display_agent: None,
                    agent_status: AgentStatus::Unknown,
                    custom_status: None,
                    state_labels: HashMap::new(),
                    agent_session: None,
                    seen: true,
                    status_age_secs: None,
                    revision: 0,
                },
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"tab_created\""));
        assert!(json.contains("\"root_pane\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn event_kind_persistence_classification() {
        // #175 O1: only the per-revision output firehose stays memory-only.
        assert!(!EventKind::PaneOutputChanged.is_persisted());
        for kind in [
            EventKind::WorkspaceCreated,
            EventKind::PaneClosed,
            EventKind::PaneAgentStatusChanged,
            EventKind::AgentForked,
        ] {
            assert!(kind.is_persisted(), "{kind:?} must persist");
        }
    }

    #[test]
    fn lineage_method_and_response_round_trip() {
        let request = Request {
            id: "req_l".into(),
            method: Method::AgentLineage(LineageParams {
                target: "w2:p1".into(),
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"method\":\"agent.lineage\""));
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);

        let response = SuccessResponse {
            id: "req_l".into(),
            result: ResponseResult::Lineage {
                chain: vec![LineageEdge {
                    seq: 7,
                    ts_ms: 1_754_000_000_000,
                    run_id: "fork:term_1".into(),
                    agent: "claude".into(),
                    seeded: true,
                    parent: LineageNode {
                        pane_id: "w1:p2".into(),
                        workspace_id: "w1".into(),
                        repo: "/repo/.git".into(),
                        worktree: None,
                        branch: None,
                    },
                    child: LineageNode {
                        pane_id: "w2:p1".into(),
                        workspace_id: "w2".into(),
                        repo: "/repo/.git".into(),
                        worktree: Some("/wt/fork-x".into()),
                        branch: Some("fork/x".into()),
                    },
                }],
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"lineage\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn pane_record_seen_and_status_age_round_trip_and_default() {
        // #175 F3: the raw seen bit and status age ride the pane record.
        let pane = PaneInfo {
            pane_id: "w_1-1".into(),
            terminal_id: "term_1".into(),
            workspace_id: "w_1".into(),
            tab_id: "w_1:1".into(),
            focused: false,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: Some("claude".into()),
            title: None,
            display_agent: None,
            agent_status: AgentStatus::Done,
            custom_status: None,
            state_labels: HashMap::new(),
            agent_session: None,
            seen: false,
            status_age_secs: Some(1800),
            revision: 7,
        };
        let json = serde_json::to_string(&pane).unwrap();
        assert!(json.contains("\"seen\":false"));
        assert!(json.contains("\"status_age_secs\":1800"));
        let restored: PaneInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, pane);

        // Records from servers predating the fields parse with seen=true and
        // no age — a missing bit must never look like an unseen turn.
        let legacy: PaneInfo = serde_json::from_str(
            r#"{"pane_id":"w_1-1","terminal_id":"term_1","workspace_id":"w_1",
                "tab_id":"w_1:1","focused":false,"agent_status":"idle","revision":1}"#,
        )
        .unwrap();
        assert!(legacy.seen);
        assert_eq!(legacy.status_age_secs, None);
    }

    #[test]
    fn error_response_round_trips() {
        let response = ErrorResponse {
            id: "req_1".into(),
            error: ErrorBody {
                code: "pane_not_found".into(),
                message: "pane p_1 not found".into(),
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        let restored: ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn event_wait_parses_typed_match() {
        let json = r#"
        {
            "id": "req_9",
            "method": "events.wait",
            "params": {
                "match_event": {
                    "event": "pane_agent_status_changed",
                    "pane_id": "p_1",
                    "agent_status": "done"
                },
                "timeout_ms": 30000
            }
        }
        "#;

        let request: Request = serde_json::from_str(json).unwrap();
        let Method::EventsWait(params) = request.method else {
            panic!("wrong method parsed");
        };
        assert_eq!(
            params.match_event,
            EventMatch::PaneAgentStatusChanged {
                pane_id: "p_1".into(),
                agent_status: AgentStatus::Done,
            }
        );
    }
}
