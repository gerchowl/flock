//! Typed durable read-only reasons; neither permits automatic retries.
use std::{fmt, io};

/// A newer journal is inspectable but must only be served by a compatible writer.
/// Retrying without upgrading the writer cannot make this refusal succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct QueueJournalNewer {
    /// Version declared by the journal identity header.
    pub found: u64,
    /// Highest format version understood by this writer.
    pub supported: u64,
}
impl std::fmt::Display for QueueJournalNewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "queue_journal_newer: found {}, supported {}",
            self.found, self.supported
        )
    }
}
impl std::error::Error for QueueJournalNewer {}

/// A supported-format record is corrupt. Only an operator can recover it.
/// Payload text is excluded from the reason so diagnostics cannot expose input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct QueueJournalCorrupt {
    /// Cursor of the bad record, when its sequence is readable.
    pub seq: Option<u64>,
    /// Byte offset of the first bad record or incomplete snapshot boundary.
    pub offset: u64,
    /// Stable diagnostic identifier, never an untrusted payload or parser message.
    pub reason: &'static str,
}
impl fmt::Display for QueueJournalCorrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "queue_journal_corrupt at byte {} (seq {:?}): {}",
            self.offset, self.seq, self.reason
        )
    }
}
impl std::error::Error for QueueJournalCorrupt {}

/// A readable journal that cannot participate in any mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ReadOnly {
    /// Upgrade to a compatible writer before serving a newer format.
    Newer(QueueJournalNewer),
    /// Operator recovery is required; no record or tail may be rewritten.
    Corrupt(QueueJournalCorrupt),
}
impl From<ReadOnly> for io::Error {
    fn from(reason: ReadOnly) -> Self {
        match reason {
            ReadOnly::Newer(reason) => io::Error::other(reason),
            ReadOnly::Corrupt(reason) => io::Error::other(reason),
        }
    }
}
pub(super) fn corrupt(offset: u64, seq: Option<u64>, reason: &'static str) -> io::Error {
    io::Error::other(QueueJournalCorrupt {
        seq,
        offset,
        reason,
    })
}
