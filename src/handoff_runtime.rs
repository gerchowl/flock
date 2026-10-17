#[cfg(unix)]
use serde::{Deserialize, Serialize};

#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRuntimeState {
    pub pane_id: u32,
    pub child_pid: u32,
    /// Spawn identity survives handoff; old manifests carry no trusted birth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_birth: Option<u64>,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<crate::pane::InputState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_history_ansi: Option<String>,
    /// The pane's agent session was confirmed by a hook from the process
    /// still running in it (#977). The snapshot carries the session ref but
    /// restores it unconfirmed, which is right for a cold restore and wrong
    /// for a live handoff, where the same process keeps the PTY. False, the
    /// safe reading, in an older server's manifest.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_session_hook_confirmed: bool,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    pub fn with_pane_id(mut self, pane_id: crate::layout::PaneId) -> Self {
        self.pane_id = pane_id.raw();
        self
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct ImportedHandoffRuntime {
    pub master_fd: std::os::fd::RawFd,
    pub state: HandoffRuntimeState,
}
