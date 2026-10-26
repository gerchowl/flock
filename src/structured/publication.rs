//! Provider boundary shared with S7-min's stream owner (#1829, ADR-0036 §5).
//! Providers capture replay and register continuation at one serialization point.
//! Consumers page on readiness hints and never read Runtime directly.

/// Server-internal provider; payloads retain each stream's snapshot/history rules.
pub(crate) trait Provider {
    /// Typed admission or readiness failure.
    type Error;
    /// Owned replay barrier followed by bounded live continuation.
    type Subscription: Subscription;
    /// Request a cut; buffer acceptance is not a completed replay barrier.
    /// File-backed providers require the consumer’s I/O worker.
    fn subscribe(&self, after: Option<u64>) -> Result<Self::Subscription, Self::Error>;
}
/// One consumer's immutable replay and live cursor.
pub(crate) trait Subscription {
    /// Stream-specific record, gap, barrier, pending, lagged or terminal outcome.
    type Page;
    /// Bounded progress. File-backed providers page on a consumer I/O worker.
    fn page(&mut self) -> Self::Page;
    /// Coalesced readiness hint. Enable before paging, then recheck after waking.
    fn waker(&self) -> std::sync::Arc<tokio::sync::Notify>;
}
