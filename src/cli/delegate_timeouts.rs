//! Transport budgets shared by delegate and its bin-only integration fixture.

use std::time::Duration;

/// Cap for delegate reads, lists and polls. For a cwd delegate whose server
/// freezes during readiness, the timeout path reads pane.read and agent.get
/// for diagnostics, then workspace.get for rollback identity. Each uses this
/// cap without retries.
/// The readiness subscription itself uses the caller's --ready-timeout budget.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
