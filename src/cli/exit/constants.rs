//! Numeric exit statuses owned by the ADR-0029 §2 registry.
//!
//! Kept free of runtime helpers and tests so bin-only integration tests can
//! include the same values without compiling the registry test suite.

/// Success, effect observed.
pub(crate) const SUCCESS: i32 = 0;

/// Unclassified failure (transport, internal).
pub(crate) const FAILURE: i32 = 1;

/// Config check: loaded with warnings.
pub(crate) const CONFIG_CHECK_ADVISORY: i32 = 1;

/// Usage error.
pub(crate) const USAGE: i32 = 2;

/// Worktree kill / delegate reap: not a linked worktree, or no such workspace.
pub(crate) const WORKTREE_NOT_LINKED: i32 = 2;

/// Config check: the config did not load.
pub(crate) const CONFIG_CHECK_PARSE_FAILURE: i32 = 2;

/// Msg / wait reply: no answer is coming (deferred, expired, gone).
pub(crate) const NO_ANSWER: i32 = 3;

/// Delegate wait: the agent answered BLOCKED.
pub(crate) const REPLY_BLOCKED: i32 = 3;

/// Notification show: recorded but not shown.
pub(crate) const NOT_SHOWN: i32 = 3;

/// Worktree kill --dry-run: the merge gate would not pass.
pub(crate) const WORKTREE_GATE_WOULD_FAIL: i32 = 3;

/// Target not found or gone.
pub(crate) const GONE: i32 = 4;

/// Worktree kill / delegate reap: dirty or has submodules, --force clears it.
pub(crate) const WORKTREE_NEEDS_FORCE: i32 = 4;

/// Delegate wait: finished without the expected result.
pub(crate) const NO_SENTINEL: i32 = 5;

/// Delegate wait: the agent needs a human.
pub(crate) const AGENT_BLOCKED: i32 = 6;

/// Agent wait / wait agent-status: the agent is blocked on a dialog.
pub(crate) const WAIT_BLOCKED: i32 = 6;

/// Waits, delegate wait: working, no progress past the stall window.
pub(crate) const STALLED: i32 = 7;

/// Send: effect uncertain, text may be in the composer.
pub(crate) const UNCONFIRMED: i32 = 8;

/// Send: effect abandoned, composer restored.
pub(crate) const ABANDONED: i32 = 9;

/// Refused before acting, nothing changed.
pub(crate) const REFUSED: i32 = 10;

/// Delegate start / wait: the delegate did not start.
pub(crate) const DELEGATE_NOT_STARTED: i32 = 11;

/// Msg: the receiver refused the message.
pub(crate) const MSG_REFUSED: i32 = 13;

/// Refused by policy: an operator-only verb called from an agent (ADR-0014), an agent widening a delegate's question policy, or a goal write the caller may not make (ADR-0030 §1).
pub(crate) const POLICY_REFUSED: i32 = 13;

/// A dispatched mutation timed out: inspect its outcome before retrying.
pub(crate) const REQUEST_OUTCOME_UNKNOWN: i32 = 14;

/// Client gave up reconnecting to its server (EX_TEMPFAIL).
pub(crate) const RECONNECT_GAVE_UP: i32 = 75;

/// Client/server protocol gap.
pub(crate) const PROTOCOL_GAP: i32 = 78;

/// Timed out.
pub(crate) const TIMEOUT: i32 = 124;
