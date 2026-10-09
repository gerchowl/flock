//! Collection deadlines share the custody clock, including pause and restart.
use super::*;

impl<D: DiskSpace> Store<D> {
    /// Claim a bounded batch and persist its next attempt before any network I/O.
    /// An idle scan does not advance or write the clock.
    pub fn collect_ready(
        &mut self,
        origin: &str,
        wall_ms: i64,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let mut clock = self.clock()?;
        if clock.paused || limit == 0 {
            return Ok(Vec::new());
        }
        let now = clock.advance(wall_ms);
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT id,collect_attempts FROM envelopes WHERE origin=?1 AND collect_at<=?2
                 AND custody_deadline>?2 AND state IN ('custody','delivered','inbox','read')
                 AND json_extract(metadata,'$.return_binding.recipient_node') NOT IN ('',?1)
                 ORDER BY collect_at,id LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![origin, now, limit.min(16) as i64], |r| {
                    Ok((
                        MessageKey {
                            origin_node: origin.into(),
                            message_id: r.get(0)?,
                        },
                        r.get::<_, u32>(1)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.writable(wall_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut claimed = Vec::new();
        for (key, attempt) in &keys {
            let base = match attempt {
                0 => 60_000,
                1 => 120_000,
                _ => 299_000,
            };
            let mut random = [0; 2];
            getrandom::fill(&mut random).map_err(|_| Error::InvalidState)?;
            let delay = base + i64::from(u16::from_le_bytes(random) % 1001);
            let changed = tx.execute(
                "UPDATE envelopes SET collect_at=?3,collect_attempts=MIN(collect_attempts+1,3)
                WHERE origin=?1 AND id=?2 AND collect_at<=?4",
                params![key.origin_node, key.message_id, now + delay, now],
            )?;
            if changed != 0 {
                claimed.push(key.clone());
            }
        }
        tx.commit()?;
        claimed
            .into_iter()
            .map(|key| self.get(&key)?.ok_or(Error::NotFound))
            .collect()
    }

    /// Authorize before acknowledging anything. A token never grants another
    /// authenticated node access to this origin's conversation.
    pub fn collect_answers(
        &mut self,
        origin: &str,
        query: &crate::mesh::collect::Collect,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::delivery::Deliver>> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        let request = self.get(&query.request)?.ok_or(Error::NotFound)?;
        if origin != query.request.origin_node
            || request.envelope.return_binding.request != query.request
            || request.envelope.return_binding.collection_token != query.token
            || query.ack.len() > 16
        {
            return Err(Error::InvalidEnvelope);
        }
        let keys = self.by_request_key(&query.request)?;
        // Validate the whole ack before changing any custody record.
        for ack in &query.ack {
            if !keys.contains(ack) {
                return Err(Error::InvalidEnvelope);
            }
        }
        for ack in &query.ack {
            let record = self.get(ack)?.ok_or(Error::NotFound)?;
            if record.state == "custody" && record.remaining_ms > 0 {
                self.finish(ack, Outcome::Delivered, wall_ms)?;
            }
        }
        self.writable(wall_ms)?;
        let mut result = Vec::new();
        for key in keys {
            let Some(record) = self.get(&key)? else {
                continue;
            };
            if record.state == "custody" && record.remaining_ms > 0 {
                result.push(crate::mesh::delivery::Deliver {
                    envelope: record.envelope,
                    remaining_ms: record.remaining_ms,
                });
                if result.len() == 16 {
                    break;
                }
            }
        }
        Ok(result)
    }
}
