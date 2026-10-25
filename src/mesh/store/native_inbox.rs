//! Recipient-owned read acknowledgements, independent of custody (ADR-0036 §3).
use super::*;
impl<D: DiskSpace> Store<D> {
    /// List unread native mail, or consume live delivered mail and sweep expiry.
    /// The acknowledgement survives restart and cannot change a custody outcome.
    /// Returns each expired envelope once for the existing drop event sweep.
    pub(crate) fn native_inbox(
        &mut self,
        recipient: Option<(&str, &str)>,
        consume: bool,
        retention_ms: u64,
        wall_ms: i64,
    ) -> Result<(Vec<Envelope>, Vec<Envelope>)> {
        let now = self.writable(wall_ms)?;
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT e.origin,e.id FROM envelopes e JOIN local_recipients r USING(origin,id)
                 WHERE e.state='delivered' AND e.kind='message'
                 AND (?1 IS NULL OR (r.agent=?1 AND r.session=?2))
                 AND NOT EXISTS(SELECT 1 FROM native_inbox_reads n
                    WHERE n.origin=e.origin AND n.id=e.id) ORDER BY e.rowid",
            )?;
            let rows = stmt
                .query_map(
                    params![recipient.map(|r| r.0), recipient.map(|r| r.1)],
                    |r| {
                        Ok(MessageKey {
                            origin_node: r.get(0)?,
                            message_id: r.get(1)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut live = Vec::new();
        let mut expired = Vec::new();
        let mut acknowledgements = Vec::new();
        for key in keys {
            let Some(record) = self.get(&key)? else {
                continue;
            };
            let settled: i64 = self.connection.query_row(
                "SELECT outcome_until-?3 FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id, CUSTODY_TTL_MS],
                |r| r.get(0),
            )?;
            let elapsed = now.saturating_sub(settled).max(0) as u64;
            let stale = elapsed >= retention_ms || record.envelope.body.is_empty();
            if !stale {
                serde_json::from_slice::<serde_json::Value>(&record.envelope.body)?;
            }
            if stale || consume {
                acknowledgements.push((key, if stale { "expired" } else { "read" }));
            }
            if stale {
                expired.push(record.envelope);
            } else if recipient.is_some() {
                live.push(record.envelope);
            }
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, state) in acknowledgements {
            tx.execute(
                "INSERT INTO native_inbox_reads VALUES(?1,?2,?3)",
                params![key.origin_node, key.message_id, state],
            )?;
        }
        tx.commit()?;
        Ok((live, expired))
    }
}
