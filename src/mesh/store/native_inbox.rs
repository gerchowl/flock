//! Recipient-owned read acknowledgements, independent of custody (ADR-0036 §3).
use super::*;
impl<D: DiskSpace> Store<D> {
    /// Fix a delivery window in the custody logical clock, including pause policy.
    pub(crate) fn native_inbox_deadline(&mut self, retention_ms: u64, wall_ms: i64) -> Result<i64> {
        Ok(self
            .writable(wall_ms)?
            .saturating_add(retention_ms.min(i64::MAX as u64) as i64))
    }
    /// Replay an explicit journal deadline without restoring deleted payloads.
    /// Native binding is authoritative; unrelated or collected keys add nothing.
    pub(crate) fn native_inbox_project(
        &mut self,
        key: &MessageKey,
        agent: &str,
        until: i64,
        wall_ms: i64,
    ) -> Result<()> {
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_inbox_reads WHERE origin=?1 AND id=?2)",
            params![key.origin_node, key.message_id],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(());
        }
        self.writable(wall_ms)?;
        if self
            .recipient_binding(key)?
            .is_none_or(|(bound, _)| bound != agent)
            || self.get(key)?.is_none_or(|r| r.envelope.body.is_empty())
        {
            return Ok(());
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO native_inbox_reads VALUES(?1,?2,'unread',?3)",
            params![key.origin_node, key.message_id, until],
        )?;
        Ok(())
    }
    /// Record the first harness delivery of already admitted, natively bound mail.
    /// Fixture/internal input only; production replays the journal deadline.
    /// Replays cannot renew the persisted window or resurrect consumed mail.
    pub(crate) fn native_inbox_delivered(
        &mut self,
        key: &MessageKey,
        retention_ms: u64,
        wall_ms: i64,
    ) -> Result<()> {
        let now = self.writable(wall_ms)?;
        if self.recipient_binding(key)?.is_none()
            || self.get(key)?.is_none_or(|r| r.envelope.body.is_empty())
        {
            return Err(Error::InvalidState);
        }
        let until = now.saturating_add(retention_ms.min(i64::MAX as u64) as i64);
        self.connection.execute(
            "INSERT OR IGNORE INTO native_inbox_reads VALUES(?1,?2,'unread',?3)",
            params![key.origin_node, key.message_id, until],
        )?;
        Ok(())
    }
    /// Peek or consume live delivered mail. Expired entries are acknowledged once
    /// and returned separately for the existing sweep event. A missing recipient
    /// sweeps every inbox without consuming live mail. Opaque bodies stay opaque.
    /// Invalid persisted entries are logged/skipped and preserved (ADR-0028 §3a).
    pub(crate) fn native_inbox(
        &mut self,
        recipient: Option<(&str, &str)>,
        consume: bool,
        wall_ms: i64,
    ) -> Result<(Vec<Envelope>, Vec<Envelope>)> {
        self.native_inbox_checked(recipient, consume, wall_ms, |_| true)
    }
    /// Decode the opaque payload before acknowledging it. Only acknowledgement
    /// winners return entries, even across independent SQLite connections.
    pub(crate) fn native_inbox_checked(
        &mut self,
        recipient: Option<(&str, &str)>,
        consume: bool,
        wall_ms: i64,
        valid: impl Fn(&Envelope) -> bool,
    ) -> Result<(Vec<Envelope>, Vec<Envelope>)> {
        let now = self.writable(wall_ms)?;
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT n.origin,n.id,n.until_ms FROM native_inbox_reads n
                 JOIN local_recipients r USING(origin,id) WHERE n.state='unread'
                 AND (?1 IS NULL OR (r.agent=?1 AND r.session=?2))
                 AND (?1 IS NOT NULL OR n.until_ms<=?3) ORDER BY n.rowid",
            )?;
            let rows = stmt
                .query_map(
                    params![recipient.map(|r| r.0), recipient.map(|r| r.1), now],
                    |r| {
                        Ok((
                            MessageKey {
                                origin_node: r.get(0)?,
                                message_id: r.get(1)?,
                            },
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )?
                .filter_map(|row| match row {
                    Ok(row) => Some(Ok(row)),
                    Err(error) if invalid_column_data(&error) => {
                        crate::logging::mesh_custody_failed("native-inbox", "undecodable_record");
                        None
                    }
                    Err(error) => Some(Err(error)),
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let (mut live, mut expired, mut acknowledgements) = (Vec::new(), Vec::new(), Vec::new());
        for (key, until) in keys {
            let record = match self.get(&key) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(error)
                    if matches!(&error, Error::Json(_) | Error::InvalidEnvelope)
                        || matches!(&error, Error::Sql(source) if invalid_column_data(source)) =>
                {
                    crate::logging::mesh_custody_failed("native-inbox", "undecodable_record");
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !record.envelope.body.is_empty() && !valid(&record.envelope) {
                crate::logging::mesh_custody_failed("native-inbox", "undecodable_payload");
                continue;
            }
            let stale = until <= now || record.envelope.body.is_empty();
            if stale || (consume && recipient.is_some()) {
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
            let changed = tx.execute("UPDATE native_inbox_reads SET state=?3 WHERE origin=?1 AND id=?2 AND state='unread'",
                params![key.origin_node, key.message_id, state])?;
            if changed != 1 {
                live.retain(|e| e.key != key);
                expired.retain(|e| e.key != key);
            }
        }
        tx.commit()?;
        Ok((live, expired))
    }
}

fn invalid_column_data(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::InvalidColumnType(..) | rusqlite::Error::FromSqlConversionFailure(..)
    )
}
