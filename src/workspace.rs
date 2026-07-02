use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::PaneId;
#[cfg(test)]
use crate::layout::TileLayout;
use crate::pane::PaneState;
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

mod aggregate;
mod git;
mod tab;

#[cfg(test)]
use self::git::git_ahead_behind;
pub(crate) use self::tab::MovedPane;
pub use self::{
    git::{
        derive_label_from_cwd, git_branch, git_space_metadata, git_status_cache_key,
        project_key_for_common_dir, GitSpaceMetadata, GitStatusCacheEntry,
    },
    tab::Tab,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeSpaceMembership {
    pub key: String,
    pub label: String,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
    ) -> WorkspaceGitStatus {
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
        }
    }
}

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);
const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

pub(crate) fn generate_workspace_id() -> String {
    let counter = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    format!("w{}", encode_public_number(counter as usize))
}

/// Encode a 1-based public number as a short, human-readable handle using a
/// Crockford-style base32 alphabet (no `I`, `L`, `O`, `U`). The handle is
/// stable per number and avoids look-alike characters.
pub(crate) fn encode_public_number(mut value: usize) -> String {
    if value == 0 {
        return "0".to_string();
    }

    let mut encoded = Vec::new();
    while value > 0 {
        let digit = (value - 1) % PUBLIC_ID_ALPHABET.len();
        encoded.push(PUBLIC_ID_ALPHABET[digit] as char);
        value = (value - 1) / PUBLIC_ID_ALPHABET.len();
    }
    encoded.iter().rev().collect()
}

pub(crate) fn decode_public_number(value: &str) -> Option<usize> {
    if value.is_empty() {
        return None;
    }
    let mut decoded = 0usize;
    for ch in value.chars() {
        let digit = PUBLIC_ID_ALPHABET
            .iter()
            .position(|candidate| *candidate as char == ch)?;
        decoded = decoded
            .checked_mul(PUBLIC_ID_ALPHABET.len())?
            .checked_add(digit + 1)?;
    }
    Some(decoded)
}

pub(crate) fn public_workspace_number(id: &str) -> Option<usize> {
    id.strip_prefix('w').and_then(decode_public_number)
}

pub(crate) fn public_pane_id_for_number(workspace_id: &str, pane_number: usize) -> String {
    format!("{workspace_id}:p{}", encode_public_number(pane_number))
}

pub(crate) fn public_tab_id_for_number(workspace_id: &str, tab_number: usize) -> String {
    format!("{workspace_id}:t{}", encode_public_number(tab_number))
}

/// After restoring workspaces from a snapshot, bump the global workspace id
/// counter past every restored id so freshly generated ids cannot collide
/// with what's already on disk. Ids that don't decode cleanly (e.g. legacy
/// hex-format ids from older snapshots) are silently ignored — they
/// preserve themselves on disk via direct equality lookup and won't clash
/// with the small numeric ids the new generator hands out.
pub(crate) fn reserve_workspace_ids(workspaces: &[Workspace]) {
    let Some(next) = workspaces
        .iter()
        .filter_map(|workspace| public_workspace_number(&workspace.id))
        .max()
        .and_then(|max| u64::try_from(max.checked_add(1)?).ok())
    else {
        return;
    };

    let mut current = NEXT_WORKSPACE_ID.load(Ordering::Relaxed);
    while current < next {
        match NEXT_WORKSPACE_ID.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

/// A named workspace containing tabs.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: String,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests, old snapshots, or missing runtimes.
    pub identity_cwd: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub(crate) cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub(crate) cached_git_ahead_behind: Option<(usize, usize)>,
    /// GitHub PR state for the current branch (background gh poller).
    pub(crate) pr_state: Option<crate::worktree::PrStateInfo>,
    /// Cached derived Git repo metadata for worktree actions and status display.
    pub(crate) cached_git_space: Option<GitSpaceMetadata>,
    /// Whether a git identity probe has completed for this workspace (the
    /// async status sweep, or the synchronous restore-time check). Until it
    /// has, a workspace without git metadata is "pending", not non-git —
    /// the sidebar must not flash it into the `misc` section (#33).
    pub(crate) git_identity_resolved: bool,
    /// Explicit Flock-managed worktree grouping provenance.
    pub worktree_space: Option<WorktreeSpaceMembership>,
    /// Public pane numbers within this workspace. Numbers are assigned
    /// monotonically on creation and are NEVER reused: closing pane N does
    /// not retarget N to a different pane later (#25).
    pub public_pane_numbers: HashMap<PaneId, usize>,
    pub(crate) next_public_pane_number: usize,
    /// Next stable public tab number. Like pane numbers, closed tab numbers
    /// are not reused, so external references to a closed tab won't silently
    /// land on a new tab.
    pub(crate) next_public_tab_number: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    #[cfg(test)]
    pub(crate) test_runtimes: HashMap<PaneId, TerminalRuntime>,
}

impl Deref for Workspace {
    type Target = Tab;

    fn deref(&self) -> &Self::Target {
        self.active_tab()
            .expect("workspace must always have at least one active tab")
    }
}

impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_tab_mut()
            .expect("workspace must always have at least one active tab")
    }
}

impl Workspace {
    pub fn new(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            events,
            render_notify,
            render_dirty,
            None,
        )
    }

    pub fn new_argv_command(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            events,
            render_notify,
            render_dirty,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tab(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                events,
                render_notify,
                render_dirty,
            )?
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        Ok((
            Self {
                id: generate_workspace_id(),
                custom_name: None,
                identity_cwd: initial_cwd.clone(),
                cached_git_branch: git_branch(&initial_cwd),
                cached_git_ahead_behind: None,
                pr_state: None,
                cached_git_space: None,
                git_identity_resolved: false,
                worktree_space: None,
                public_pane_numbers,
                next_public_pane_number: 2,
                next_public_tab_number: 2,
                tabs: vec![tab],
                active_tab: 0,
                #[cfg(test)]
                test_runtimes: HashMap::new(),
            },
            terminal,
            runtime,
        ))
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub fn active_tab_display_name(&self) -> Option<String> {
        self.active_tab().map(Tab::display_name)
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            if let Some(tab) = self.tabs.get_mut(idx) {
                for pane in tab.panes.values_mut() {
                    pane.seen = true;
                }
            }
        }
    }

    pub fn create_tab(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            None,
        )
    }

    fn create_tab_with_runtime(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_number = self.next_public_pane_number;
        let events = self
            .active_tab()
            .map(|tab| tab.events.clone())
            .expect("workspace must always have at least one tab");
        let render_notify = self
            .active_tab()
            .map(|tab| tab.render_notify.clone())
            .expect("workspace must always have at least one tab");
        let render_dirty = self
            .active_tab()
            .map(|tab| tab.render_dirty.clone())
            .expect("workspace must always have at least one tab");

        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                number,
                cwd,
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                number,
                cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                events,
                render_notify,
                render_dirty,
            )?
        };
        self.register_new_pane_with_number(tab.root_pane, pane_number);
        self.tabs.push(tab);
        Ok((self.tabs.len() - 1, terminal, runtime))
    }

    pub fn close_tab(&mut self, idx: usize) -> bool {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return false;
        }
        let tab = self.tabs.remove(idx);
        for pane_id in tab.panes.keys() {
            self.unregister_pane(*pane_id);
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
        true
    }

    pub fn move_tab(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        if source_idx >= self.tabs.len() || insert_idx > self.tabs.len() {
            return false;
        }

        let target_idx = if source_idx < insert_idx {
            insert_idx.saturating_sub(1)
        } else {
            insert_idx
        }
        .min(self.tabs.len().saturating_sub(1));

        if source_idx == target_idx {
            return false;
        }

        let active_root_pane = self.tabs.get(self.active_tab).map(|tab| tab.root_pane);
        let tab = self.tabs.remove(source_idx);
        self.tabs.insert(target_idx, tab);
        self.active_tab = active_root_pane
            .and_then(|root_pane| self.tabs.iter().position(|tab| tab.root_pane == root_pane))
            .unwrap_or(target_idx);
        true
    }

    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.active_tab)
    }

    pub fn split_focused(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
    ) -> std::io::Result<crate::workspace::tab::NewPane> {
        let new_pane = self
            .active_tab_mut()
            .expect("workspace must always have at least one tab")
            .split_focused(
                direction,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
            )?;
        self.register_new_pane(new_pane.pane_id);
        Ok(new_pane)
    }

    pub fn split_pane(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn split_pane_with_runtime(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        focus_new_pane: bool,
        argv: Option<&[String]>,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let tab = &mut self.tabs[tab_idx];
        let previous_focus = tab.layout.focused();
        tab.layout.focus_pane(pane_id);
        let new_pane = match if let Some(argv) = argv {
            tab.split_focused_argv_command(
                direction,
                rows,
                cols,
                cwd,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
            )
        } else {
            tab.split_focused(
                direction,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
            )
        } {
            Ok(new_pane) => new_pane,
            Err(err) => {
                tab.layout.focus_pane(previous_focus);
                return Some(Err(err));
            }
        };
        if !focus_new_pane {
            tab.layout.focus_pane(previous_focus);
        }
        self.register_new_pane(new_pane.pane_id);
        Some(Ok((tab_idx, new_pane)))
    }

    /// Close the focused pane. Returns true if the workspace should close.
    pub fn close_focused(&mut self) -> bool {
        let pane_count = self
            .active_tab()
            .map(|tab| tab.layout.pane_count())
            .unwrap_or(0);
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            return tab_count <= 1 || self.close_active_tab_and_report();
        }

        if let Some((removed, _terminal_id)) = self.active_tab_mut().and_then(Tab::close_focused) {
            self.unregister_pane(removed);
        }
        false
    }

    /// Remove a specific pane from this workspace without terminating its runtime.
    /// Returns true if the workspace should close.
    pub fn remove_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(tab_idx) = self.find_tab_index_for_pane(pane_id) else {
            return false;
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].remove_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    /// Remove a pane from this workspace for relocation while keeping its PTY
    /// alive. The returned `TakenPane` describes whether the host tab and/or
    /// the workspace itself are now empty so the caller can decide whether to
    /// drop them.
    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<TakenPane> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        if pane_count <= 1 {
            let mut tab = self.tabs.remove(tab_idx);
            let moved = tab.take_pane_for_move(pane_id)?;
            self.adjust_active_tab_after_removal(tab_idx);
            return Some(TakenPane {
                moved,
                removed_tab_idx: Some(tab_idx),
                workspace_empty: self.tabs.is_empty(),
            });
        }

        let moved = self.tabs[tab_idx].take_pane_for_move(pane_id)?;
        Some(TakenPane {
            moved,
            removed_tab_idx: None,
            workspace_empty: false,
        })
    }

    /// Insert an already-moved pane into an existing tab. On success the pane
    /// is registered under this workspace's public-id space (keeping the
    /// per-workspace stable numbering). On failure the moved pane is returned
    /// for caller-side recovery.
    pub(crate) fn insert_moved_pane_into_tab(
        &mut self,
        tab_idx: usize,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
    ) -> Result<PaneId, MovedPane> {
        let pane_id = moved.pane_id;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return Err(moved);
        };
        tab.insert_existing_pane(target_pane_id, moved, direction, ratio)?;
        if !self.public_pane_numbers.contains_key(&pane_id) {
            let number = self.next_public_pane_number;
            self.register_new_pane_with_number(pane_id, number);
        }
        Ok(pane_id)
    }

    /// Create a new tab that wraps an already-moved pane, inheriting render
    /// channels from this workspace's active tab when present (or falling back
    /// to the supplied app-level handles when the workspace has just been
    /// emptied of its last tab).
    pub(crate) fn create_tab_from_existing_pane(
        &mut self,
        moved: MovedPane,
        label: Option<String>,
        fallback_events: mpsc::Sender<AppEvent>,
        fallback_render_notify: Arc<Notify>,
        fallback_render_dirty: Arc<AtomicBool>,
    ) -> usize {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_id = moved.pane_id;
        let (events, render_notify, render_dirty) = self
            .active_tab()
            .map(|tab| {
                (
                    tab.events.clone(),
                    tab.render_notify.clone(),
                    tab.render_dirty.clone(),
                )
            })
            .unwrap_or((
                fallback_events,
                fallback_render_notify,
                fallback_render_dirty,
            ));
        let tab =
            Tab::from_existing_pane(number, label, moved, events, render_notify, render_dirty);
        if !self.public_pane_numbers.contains_key(&pane_id) {
            let pane_number = self.next_public_pane_number;
            self.register_new_pane_with_number(pane_id, pane_number);
        }
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    /// Drop the workspace-scoped public mapping for a pane that has been
    /// moved away. Pairs with `take_pane_for_move` when the destination is a
    /// different workspace — the pane will gain a fresh number in its new home.
    pub(crate) fn unregister_moved_pane(&mut self, pane_id: PaneId) {
        self.unregister_pane(pane_id);
    }

    fn adjust_active_tab_after_removal(&mut self, removed_idx: usize) {
        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if removed_idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
    }

    /// Construct a brand-new workspace around an already-moved pane. Used when
    /// `pane.move` targets `NewWorkspace`. The pane's PTY runtime stays
    /// registered with the same terminal id at the App level — only the
    /// pane's containing workspace/tab changes.
    pub(crate) fn from_existing_pane(
        label: Option<String>,
        tab_label: Option<String>,
        identity_cwd: PathBuf,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> Self {
        let id = generate_workspace_id();
        let root_pane = moved.pane_id;
        let tab = Tab::from_existing_pane(1, tab_label, moved, events, render_notify, render_dirty);
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(root_pane, 1);
        Self {
            id,
            custom_name: label,
            identity_cwd: identity_cwd.clone(),
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            pr_state: None,
            cached_git_space: git_space_metadata(&identity_cwd),
            git_identity_resolved: false,
            worktree_space: None,
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        }
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.public_pane_numbers.get(&pane_id).copied()
    }

    pub fn public_tab_number(&self, tab_idx: usize) -> Option<usize> {
        self.tabs.get(tab_idx).map(|tab| tab.number)
    }

    #[cfg(test)]
    pub fn public_tab_number_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        self.public_tab_number(tab_idx)
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        self.tabs
            .first()
            .and_then(|tab| tab.cwd_for_pane(tab.root_pane, terminals, terminal_runtimes))
            .or_else(|| Some(self.identity_cwd.clone()))
    }

    pub fn display_name(&self) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.resolved_identity_cwd()
            .map(|cwd| derive_label_from_cwd(&cwd))
            .unwrap_or_else(|| "workspace".into())
    }

    pub fn display_name_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.resolved_identity_cwd_from(terminals, terminal_runtimes)
            .map(|cwd| derive_label_from_cwd(&cwd))
            .unwrap_or_else(|| "workspace".into())
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn ahead_behind(&self) -> Option<(usize, usize)> {
        self.cached_git_ahead_behind
    }

    pub fn pr_state(&self) -> Option<crate::worktree::PrStateInfo> {
        self.pr_state
    }

    pub fn git_ahead_behind(&self) -> Option<(usize, usize)> {
        self.cached_git_ahead_behind
    }

    pub fn git_space(&self) -> Option<&GitSpaceMetadata> {
        self.cached_git_space.as_ref()
    }

    /// Identity of this workspace's repo family: the canonical git common
    /// dir, shared by the main checkout and every linked worktree. Sourced
    /// from worktree membership first, then live git metadata.
    pub fn repo_group_key(&self) -> Option<&str> {
        self.worktree_space
            .as_ref()
            .map(|space| space.key.as_str())
            .or_else(|| {
                self.cached_git_space
                    .as_ref()
                    .map(|space| space.key.as_str())
            })
    }

    pub fn worktree_space(&self) -> Option<&WorktreeSpaceMembership> {
        self.worktree_space.as_ref()
    }

    /// Whether this checkout is a linked git worktree (vs the repo's main
    /// checkout): explicit membership provenance first, then live git
    /// metadata. The main checkout is the project section's primary row (#33).
    pub fn is_linked_checkout(&self) -> bool {
        self.worktree_space
            .as_ref()
            .map(|space| space.is_linked_worktree)
            .or_else(|| {
                self.cached_git_space
                    .as_ref()
                    .map(|space| space.is_linked_worktree)
            })
            .unwrap_or(false)
    }

    /// True while the workspace has no git identity AND no probe has
    /// finished — it may still turn out to be a git checkout. Distinct
    /// from "resolved non-git", which files under `misc` (#33). Kept as
    /// an observable predicate for tests / diagnostics after #102 folded
    /// pending and resolved-non-git rows into one merged-stream slot
    /// keyed on the frozen `sort_family_key`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn git_identity_pending(&self) -> bool {
        !self.git_identity_resolved
            && self.worktree_space.is_none()
            && self.cached_git_space.is_none()
    }

    /// Machine-independent project identity (normalized origin URL or
    /// "dir:<name>" fallback) used to fold checkouts of the same project
    /// across federated peer servers. See [[peers]] config.
    pub fn project_key(&self) -> Option<&str> {
        self.cached_git_space
            .as_ref()
            .map(|space| space.project_key.as_str())
    }

    /// #102: a STABLE sort-family key frozen at spawn — the `identity_cwd`
    /// basename lowercased. Used as the sort fallback while the git identity
    /// probe hasn't yet resolved (before `cached_git_space` fills), so a
    /// pending row keeps its slot in the spaces list across probe resolution
    /// instead of jumping when the resolved project key replaces the
    /// display-name fallback. `identity_cwd` is written once at construction
    /// and never mutated afterwards, so the same call before and after the
    /// probe returns the same string.
    pub fn sort_family_key(&self) -> String {
        self.identity_cwd
            .file_name()
            .and_then(|name| name.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn refresh_git_ahead_behind(&mut self) {
        let cwd = self.resolved_identity_cwd();
        self.cached_git_branch = cwd.as_deref().and_then(git_branch);
        self.cached_git_ahead_behind = cwd.as_deref().and_then(git_ahead_behind);
        self.cached_git_space = cwd.as_deref().and_then(git_space_metadata);
        self.git_identity_resolved = true;
    }

    pub fn git_status_snapshot_for_cwd_with_cache(
        resolved_identity_cwd: &std::path::Path,
        cached: Option<&GitStatusCacheEntry>,
    ) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
        self::git::git_status_snapshot_for_cwd(resolved_identity_cwd, cached)
    }

    pub fn find_tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.contains_key(&pane_id))
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.tabs.iter().find_map(|tab| tab.panes.get(&pane_id))
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.tabs.iter().find_map(|tab| tab.terminal_id(pane_id))
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        self.active_tab().map(|tab| tab.layout.focused())
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let tab_idx = match self.find_tab_index_for_pane(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let pane_count = self.tabs[tab_idx].layout.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].close_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    fn register_new_pane(&mut self, pane_id: PaneId) {
        let number = self.next_public_pane_number;
        self.register_new_pane_with_number(pane_id, number);
    }

    fn register_new_pane_with_number(&mut self, pane_id: PaneId, number: usize) {
        self.public_pane_numbers.insert(pane_id, number);
        self.next_public_pane_number = self.next_public_pane_number.max(number + 1);
    }

    fn unregister_pane(&mut self, pane_id: PaneId) {
        // Closed pane numbers are not reused (#25): drop the mapping but
        // leave the surrounding numbers alone so a stale public id from a
        // hook script won't silently retarget a different pane.
        self.public_pane_numbers.remove(&pane_id);
    }

    fn close_active_tab_and_report(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return true;
        }
        self.close_active_tab();
        false
    }
}

/// Result of removing a pane for `pane.move`. Carries the live pane state plus
/// shape hints so the caller can drop the source tab or workspace if they are
/// now empty.
pub(crate) struct TakenPane {
    pub moved: MovedPane,
    pub removed_tab_idx: Option<usize>,
    pub workspace_empty: bool,
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn test_new(name: &str) -> Self {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let identity_cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));
        let tab = Tab {
            custom_name: None,
            number: 1,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane, 1);
        Self {
            id: generate_workspace_id(),
            custom_name: Some(name.to_string()),
            identity_cwd: identity_cwd.clone(),
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            pr_state: None,
            cached_git_space: None,
            git_identity_resolved: false,
            worktree_space: None,
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            test_runtimes: HashMap::new(),
        }
    }

    pub(crate) fn insert_test_runtime(&mut self, pane_id: PaneId, runtime: TerminalRuntime) {
        self.test_runtimes.insert(pane_id, runtime);
    }

    pub(crate) fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab = self.active_tab_mut().expect("workspace must have tab");
        let new_id = tab.layout.split_focused(direction);
        tab.panes
            .insert(new_id, PaneState::new(TerminalId::alloc()));
        self.register_new_pane(new_id);
        new_id
    }

    pub(crate) fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let (layout, root_id) = TileLayout::new();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(TerminalId::alloc()));
        let tab = Tab {
            custom_name: name.map(str::to_string),
            number: self.next_public_tab_number,
            root_pane: root_id,
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        self.next_public_tab_number += 1;
        self.register_new_pane(root_id);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #102: the pending-probe jump fix. `sort_family_key` is frozen at
    /// spawn from `identity_cwd` — the SAME string before and after the git
    /// identity probe finishes, so the fallback sort slot used by
    /// `workspace_list_entries` does not shift under the workspace when
    /// `cached_git_space` fills.
    #[test]
    fn sort_family_key_stays_stable_across_git_probe_resolution() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = std::path::PathBuf::from("/repo/gerchowl/Flock");
        // Pending probe: `cached_git_space` is None, `git_identity_resolved`
        // is false. This is the state where `project_section_sort_ids`
        // yields None and the sort falls back to `sort_family_key`.
        assert!(ws.git_identity_pending());
        let before = ws.sort_family_key();
        assert_eq!(before, "flock");

        // Probe resolves: the workspace gains a full git identity with a
        // very different project key (`github.com/gerchowl/flock`). Before
        // #102, the fallback (`display_name().to_ascii_lowercase()`) is a
        // string the probe never touched but that CAN drift with mutable
        // state. The frozen key must not drift.
        ws.cached_git_space = Some(GitSpaceMetadata {
            key: "/repo/gerchowl/Flock/.git".into(),
            checkout_key: "/repo/gerchowl/Flock".into(),
            label: "Flock".into(),
            repo_root: std::path::PathBuf::from("/repo/gerchowl/Flock"),
            is_linked_worktree: false,
            project_key: "github.com/gerchowl/flock".into(),
        });
        ws.git_identity_resolved = true;
        assert!(!ws.git_identity_pending());
        assert_eq!(ws.sort_family_key(), before);
    }

    #[test]
    fn workspace_identity_follows_first_tab_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.tabs[0].root_pane;
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/flock-test/pion")),
        );
        let terminal_runtimes = TerminalRuntimeRegistry::new();

        assert_eq!(ws.display_name_from(&terminals, &terminal_runtimes), "pion");
        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/flock-test/pion"))
        );
    }

    #[test]
    fn moving_tab_keeps_active_identity_and_stable_tab_numbers() {
        let mut ws = Workspace::test_new("test");
        let moved_root = ws.tabs[0].root_pane;
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        let active_root = ws.tabs[final_auto_idx].root_pane;
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        // Tab numbers travel with the tab itself — moving does not renumber.
        let labels: Vec<_> = ws.tabs.iter().map(|tab| tab.display_name()).collect();
        assert_eq!(labels, vec!["foo", "3", "1"]);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("foo"));
        assert!(ws.tabs[1].custom_name.is_none());
        assert!(ws.tabs[2].custom_name.is_none());
        assert_eq!(ws.tabs[0].number, 2);
        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(ws.tabs[2].number, 1);
        assert_eq!(ws.tabs[2].root_pane, moved_root);
        assert_eq!(ws.tabs[ws.active_tab].root_pane, active_root);
    }

    #[test]
    fn generated_workspace_ids_are_short_base32_handles() {
        let first = generate_workspace_id();
        let second = generate_workspace_id();

        assert!(first.starts_with('w'));
        assert!(second.starts_with('w'));
        assert_ne!(first, second);
        assert!(first.len() <= 4, "unexpectedly long workspace id: {first}");
        assert!(
            second.len() <= 4,
            "unexpectedly long workspace id: {second}"
        );
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        assert_eq!(encode_public_number(1), "1");
        assert_eq!(encode_public_number(9), "9");
        assert_eq!(encode_public_number(10), "A");
        assert_eq!(encode_public_number(31), "Z");
        assert_eq!(encode_public_number(32), "0");
        assert_eq!(encode_public_number(33), "11");

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let encoded = encode_public_number(value);
            assert_eq!(decode_public_number(&encoded), Some(value));
        }
    }

    #[test]
    fn reserving_restored_workspace_ids_prevents_reuse() {
        let mut restored = Workspace::test_new("restored");
        restored.id = "wZ".to_string();

        reserve_workspace_ids(&[restored]);

        let generated = generate_workspace_id();
        assert_ne!(generated, "wZ");
        assert!(public_workspace_number(&generated) > public_workspace_number("wZ"));
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tabs[0].root_pane;
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), Some(2));
        assert_eq!(ws.public_pane_number(third), Some(3));

        assert!(!ws.close_pane(second));

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), None);
        assert_eq!(ws.public_pane_number(third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(ws.public_pane_number(fourth), Some(4));
    }

    #[test]
    fn tab_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let first_root = ws.tabs[0].root_pane;
        let second_tab = ws.test_add_tab(None);
        let second_root = ws.tabs[second_tab].root_pane;
        let third_tab = ws.test_add_tab(None);
        let third_root = ws.tabs[third_tab].root_pane;

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(second_root), Some(2));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        assert!(ws.close_tab(second_tab));

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        let fourth_tab = ws.test_add_tab(None);
        let fourth_root = ws.tabs[fourth_tab].root_pane;
        assert_eq!(ws.public_tab_number_for_pane(fourth_root), Some(4));
    }
}
