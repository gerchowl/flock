//! Outcomes a forwarding hub decides on its own, owed to the origin (#876).
//!
//! The owner never sees a message that a hub refuses or lets expire, so only
//! the hub can report it. The hub signs an `undeliverable` receipt, a state
//! no recipient receipt uses, and the origin applies it only while no
//! recipient outcome is known. Requests, answers and routed receipts are all
//! owed one (#902). An `undeliverable` receipt is never owed one itself, so
//! an outcome that dies in turn ends there instead of bouncing between hubs.
use super::*;

/// One forwarded envelope whose custody ended at this hub without transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubOutcome {
    pub key: MessageKey,
    /// The hub's reason, such as `hop_budget_exhausted` or `no_route`.
    pub detail: String,
}

/// The receipt state of a hub-decided outcome.
pub const UNDELIVERABLE: &str = "undeliverable";

/// How many times a signer routes one receipt before it gives up: the
/// original and one retry once a hub reports the first undeliverable.
pub const RECEIPT_ROUTES: i64 = 2;

impl<D: DiskSpace> Store<D> {
    /// Remember why the next hop refused a route, and clear it for rerouting.
    /// Only a forwarded row keeps the reason, as its hub outcome detail.
    pub fn refuse_route(&mut self, key: &MessageKey, reason: &str) -> Result<()> {
        self.set_next_hop(key, "")?;
        self.connection.execute(
            "UPDATE envelopes SET collect_error=?3 WHERE origin=?1 AND id=?2
             AND origin!=?4 AND state IN ('custody','held') AND collect_error IS NOT ?3",
            params![
                key.origin_node,
                key.message_id,
                reason.split(':').next().unwrap_or(reason),
                self.local_node
            ],
        )?;
        Ok(())
    }

    /// Forwarded requests, answers and signed receipts whose custody ended
    /// here: refused by this hub or downstream without a recipient receipt,
    /// or past the custody deadline. A row leaves this window once its
    /// receipt is in custody. A hub outcome never owes one of its own.
    pub fn hub_outcomes(&mut self, wall_ms: i64) -> Result<Vec<HubOutcome>> {
        if self.local_node.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.clock()?.advance(wall_ms);
        let mut stmt = self.connection.prepare(
            "SELECT rowid,origin,id,CASE WHEN state='refused' THEN COALESCE(collect_error,'refused')
                ELSE COALESCE(collect_error,CASE WHEN next_hop='' THEN 'no_route' ELSE 'custody_expired' END) END
             FROM envelopes e WHERE origin!=?1 AND recipient_node NOT IN ('',?1)
             AND (kind='message' OR (kind='receipt' AND request_origin IS NOT NULL
                AND correlation IS NOT 'receipt:' || request_id || ':' || ?3))
             AND json_valid(visited) AND json_array_length(visited)>1
             AND receipt_sent IS NULL AND (outcome_until IS NULL OR outcome_until>?2)
             AND (state IN ('refused','expired') OR (state IN ('custody','held') AND custody_deadline<=?2))
             AND NOT EXISTS (SELECT 1 FROM envelopes r WHERE r.request_origin=e.origin
                AND r.request_id=e.id AND r.kind='receipt')
             ORDER BY origin,id LIMIT 16",
        )?;
        let mut rows = stmt.query(params![self.local_node, now, UNDELIVERABLE])?;
        let mut outcomes = Vec::new();
        let mut bad_rows = Vec::new();
        while let Some(row) = rows.next()? {
            match (
                row.get::<_, String>(1),
                row.get::<_, String>(2),
                row.get::<_, String>(3),
            ) {
                (Ok(origin_node), Ok(message_id), Ok(detail)) => outcomes.push(HubOutcome {
                    key: MessageKey {
                        origin_node,
                        message_id,
                    },
                    detail,
                }),
                _ => bad_rows.push(row.get::<_, i64>(0)?),
            }
        }
        drop(rows);
        drop(stmt);
        quarantine::rows(
            &self.connection,
            &bad_rows,
            &self.path,
            "invalid routing key",
            wall_ms,
        )?;
        Ok(outcomes)
    }

    /// Mark a hub outcome whose receipt is now in custody.
    pub fn hub_outcome_sent(&mut self, key: &MessageKey) -> Result<()> {
        self.connection.execute(
            "UPDATE envelopes SET receipt_sent=?3 WHERE origin=?1 AND id=?2 AND receipt_sent IS NULL",
            params![key.origin_node, key.message_id, UNDELIVERABLE],
        )?;
        Ok(())
    }

    /// Apply a hub's authenticated `undeliverable` outcome at the origin. A
    /// recipient outcome always stands, and replay changes nothing. The
    /// origin's own expiry gives way, because the hub's reason is the news.
    pub fn import_hub_outcome(
        &mut self,
        key: &MessageKey,
        detail: &str,
        wall_ms: i64,
    ) -> Result<ReceiptImport> {
        let now = self.writable(wall_ms)?;
        let row: Option<(String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT state,remote_state FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((state, remote)) = row else {
            return Ok(ReceiptImport::OriginalNotReady);
        };
        if remote.is_some() || !matches!(state.as_str(), "held" | "transferred" | "expired") {
            return Ok(if state == "custody" {
                ReceiptImport::OriginalNotReady
            } else {
                ReceiptImport::Duplicate
            });
        }
        self.connection.execute(
            "UPDATE envelopes SET state=?3,remote_state=?3,body=X'',collect_error=?4,
             outcome_until=COALESCE(outcome_until,?5),retry_at=0,lease_until=0
             WHERE origin=?1 AND id=?2 AND remote_state IS NULL",
            params![
                key.origin_node,
                key.message_id,
                UNDELIVERABLE,
                detail,
                now.saturating_add(CUSTODY_TTL_MS)
            ],
        )?;
        Ok(ReceiptImport::Applied)
    }

    /// A hub reported this node's routed `receipt` undeliverable (#902).
    /// Owe the request's receipt again, so the next routing pass mints a
    /// fresh one on the current route, unless it already took
    /// [`RECEIPT_ROUTES`] routes. A request that has since owed a newer
    /// state needs nothing: that receipt supersedes this one. Returns false
    /// only when the routes are spent.
    pub fn retry_receipt(&mut self, receipt: &crate::mesh::collect::Receipt) -> Result<bool> {
        let routes: i64 = self.connection.query_row(
            "SELECT count(*) FROM envelopes WHERE origin=?1 AND kind='receipt' AND state=?5
             AND request_origin=?2 AND request_id=?3 AND correlation=?4",
            params![
                self.local_node,
                receipt.key.origin_node,
                receipt.key.message_id,
                format!("receipt:{}:{}", receipt.key.message_id, receipt.state),
                UNDELIVERABLE
            ],
            |row| row.get(0),
        )?;
        if routes >= RECEIPT_ROUTES {
            return Ok(false);
        }
        self.connection.execute(
            "UPDATE envelopes SET receipt_sent=NULL WHERE origin=?1 AND id=?2 AND receipt_sent=?3",
            params![
                receipt.key.origin_node,
                receipt.key.message_id,
                receipt.state
            ],
        )?;
        Ok(true)
    }
}
