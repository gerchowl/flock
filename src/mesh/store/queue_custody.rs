//! Observable uncertainty while a native queue owner recovers its journal.
use super::*;
use crate::structured::queue::custody::Unknown;

pub(super) const DELIVERED_AFTER_EXPIRY: &str = "delivered_after_expiry:";

pub(super) const COMMITMENT_UNKNOWN_PREFIX: &str = "input_write_unknown:";
pub(super) const COMMITMENT_UNKNOWN_AT_EXPIRY: &str =
    "input_write_unknown:commitment_unknown_at_expiry";

impl<D: DiskSpace> Store<D> {
    // Only the sole journal owner's positive proof may call this path.
    pub(super) fn commit_queue_admission(&mut self, key: &MessageKey, wall_ms: i64) -> Result<()> {
        let now = self.writable(wall_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, error): (String, Option<String>) = tx
            .query_row(
                "SELECT state,collect_error FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        if matches!(
            state.as_str(),
            "delivered" | "read" | "refused" | "recipient_gone"
        ) {
            return Ok(());
        }
        let late =
            state == "inbox_expired" && error.as_deref() == Some(COMMITMENT_UNKNOWN_AT_EXPIRY);
        if state != "inbox" && !late {
            return Err(Error::InvalidState);
        }
        let detail = late.then(|| format!("{DELIVERED_AFTER_EXPIRY}{}", key.lifecycle_id()));
        // Preserve body and both original deadlines. An expired row stays
        // payload-less, consumes no new quota and gets no reply extension.
        tx.execute(
            "UPDATE envelopes SET state='delivered',delivered=1,
             collect_error=CASE WHEN ?4 THEN ?3
               WHEN substr(collect_error,1,length(?7))=?7 THEN NULL ELSE collect_error END,
             outcome_until=CASE WHEN ?4 THEN outcome_until ELSE ?5 END,
             collect_at=?6,collect_attempts=0 WHERE origin=?1 AND id=?2",
            params![
                key.origin_node,
                key.message_id,
                detail,
                late,
                now.saturating_add(CUSTODY_TTL_MS),
                now.saturating_add(5_000),
                COMMITMENT_UNKNOWN_PREFIX
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a typed cause without settling possibly committed custody. `None`
    /// clears only this diagnostic when the owner can answer again. Returns the
    /// remaining native inbox TTL; unbound or settled rows return `None`.
    /// Expired inboxes use the existing expiry outcome, retaining uncertainty
    /// in its detail until a positive journal proof resolves that expiry label.
    pub fn queue_commitment_unknown(
        &mut self,
        key: &MessageKey,
        cause: Option<Unknown>,
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
