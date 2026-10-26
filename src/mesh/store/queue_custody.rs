//! Observable uncertainty while a native queue owner recovers its journal.
use super::*;
use crate::api::error_code::ErrorCode;

pub(super) const COMMITMENT_UNKNOWN_PREFIX: &str = "input_write_unknown:";
pub(super) const COMMITMENT_UNKNOWN_AT_EXPIRY: &str =
    "input_write_unknown:commitment_unknown_at_expiry";

impl<D: DiskSpace> Store<D> {
    /// Record a typed cause without settling possibly committed custody. `None`
    /// clears only this diagnostic when the owner can answer again. Returns the
    /// remaining native inbox TTL; unbound or settled rows return `None`.
    /// Expired inboxes use the existing expiry outcome, retaining uncertainty
    /// in its detail. A late proof cannot erase uncertainty held at expiry.
    pub fn queue_commitment_unknown(
        &mut self,
        key: &MessageKey,
        cause: Option<ErrorCode>,
        wall_ms: i64,
    ) -> Result<Option<u64>> {
        let now = self.writable(wall_ms)?;
        let deadline: Option<i64> = self
            .connection
            .query_row(
                "SELECT inbox_deadline FROM envelopes WHERE origin=?1 AND id=?2 AND state='inbox'
             AND EXISTS(SELECT 1 FROM local_recipients r WHERE r.origin=?1 AND r.id=?2)",
                params![key.origin_node, key.message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(deadline) = deadline else {
            return Ok(None);
        };
        let detail = cause.map(|code| format!("{COMMITMENT_UNKNOWN_PREFIX}{}", code.as_str()));
        self.connection.execute(
            "UPDATE envelopes SET collect_error=?3 WHERE origin=?1 AND id=?2
             AND collect_error IS NOT ?3 AND (?3 IS NOT NULL OR
               (inbox_deadline>?5 AND substr(collect_error,1,length(?4))=?4))",
            params![
                key.origin_node,
                key.message_id,
                detail,
                COMMITMENT_UNKNOWN_PREFIX,
                now
            ],
        )?;
        if deadline <= now {
            self.finish(key, Outcome::InboxExpired, wall_ms)?;
            Ok(None)
        } else {
            Ok(Some((deadline - now) as u64))
        }
    }
}
