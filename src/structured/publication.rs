//! Provider boundary shared with S7-min's stream owner (#1829, ADR-0036 §5).
//! Providers capture replay and register continuation at one serialization point.
//! Consumers page on readiness hints and never read Runtime directly.
use crate::api::error_code::ErrorCode;

/// Server-internal provider; payloads retain each stream's snapshot/history rules.
pub(crate) trait Provider {
    /// Owned replay barrier followed by bounded live continuation.
    type Subscription: Subscription;
    /// Nonblocking request. Buffer acceptance is not a completed replay cut.
    fn subscribe(&self, after: Option<u64>) -> Result<Self::Subscription, ErrorCode>;
}
/// One consumer's immutable replay and live cursor.
pub(crate) trait Subscription {
    /// Stream-specific record, gap, barrier, pending, lagged or terminal outcome.
    type Page;
    /// Bounded in-memory progress; filesystem work stays on the source owner.
    fn page(&mut self) -> Self::Page;
    /// Coalesced readiness hint. Enable before paging, then recheck after waking.
    fn waker(&self) -> std::sync::Arc<tokio::sync::Notify>;
}
