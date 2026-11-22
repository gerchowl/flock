//! Authority scopes shared by authentication and its API projection.
use serde::{Deserialize, Serialize};

/// Explicit token scopes; operate does not silently grant the other scopes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Read authorized streams.
    Read,
    /// Queue authorized user turns.
    Send,
    /// Answer scoped questions without acquiring operator authority.
    Answer,
    /// Act as an operator for explicitly authorized verbs.
    Operate,
}
