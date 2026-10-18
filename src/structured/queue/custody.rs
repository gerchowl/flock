//! Why the sole journal owner cannot prove a custody key absent.
/// Typed uncertainty is diagnostic, never a terminal custody refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unknown {
    /// No live runtime or queue owner can answer.
    OwnerUnavailable,
    /// An accepted admission has not finished.
    AdmissionPending,
    /// A failed write may have reached durable storage.
    WriteUncertain,
    /// Opaque future records make the retained fold incomplete.
    JournalOpaque,
    /// A compatible writer must open the newer journal.
    JournalNewer,
    /// Operator recovery must repair the journal.
    JournalCorrupt,
}
impl Unknown {
    /// Stable status-detail suffix, containing no producer text.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OwnerUnavailable => "owner_unavailable",
            Self::AdmissionPending => "admission_pending",
            Self::WriteUncertain => "write_uncertain",
            Self::JournalOpaque => "journal_opaque",
            Self::JournalNewer => crate::api::error_code::ErrorCode::QueueJournalNewer.as_str(),
            Self::JournalCorrupt => crate::api::error_code::ErrorCode::QueueJournalCorrupt.as_str(),
        }
    }
}
