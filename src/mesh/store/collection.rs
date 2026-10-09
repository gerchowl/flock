//! Conversation-scoped collection with durable deadlines and acknowledgement debt.
use super::*;

pub(super) fn record_answer(
    tx: &rusqlite::Transaction<'_>,
    answer: &Envelope,
    admission: Admission,
    ack: bool,
) -> Result<()> {
    let Some(request) = &answer.request_key else {
        return Ok(());
    };
    if admission != Admission::Inbox {
        return Ok(());
    }
    if !crate::app::mailboxes::is_deferral(&answer.correlation_id) {
        tx.execute(
            "UPDATE envelopes SET collect_done=1,collect_error=NULL WHERE origin=?1 AND id=?2",
            params![request.origin_node, request.message_id],
        )?;
    }
    if ack {
        tx.execute(
            "INSERT OR IGNORE INTO collect_acks VALUES(?1,?2,?3,?4)",
            params![
                request.origin_node,
                request.message_id,
                answer.key.origin_node,
                answer.key.message_id
            ],
        )?;
    }
    Ok(())
}

impl<D: DiskSpace> Store<D> {
    /// Read TTL using the current logical time without committing the clock.
    pub fn collection_record(&mut self, key: &MessageKey, wall_ms: i64) -> Result<Option<Record>> {
        let mut clock = self.clock()?;
        let elapsed = clock.elapsed_ms;
        let delta = clock.advance(wall_ms).saturating_sub(elapsed);
        match self.get(key) {
            Ok(Some(mut record)) => {
                record.remaining_ms = record.remaining_ms.saturating_sub(delta).max(0);
                if record.state == "quarantined" {
                    return Ok(None);
                }
                Ok(Some(record))
            }
            Err(Error::Json(_)) => {
                self.quarantine(key)?;
                Ok(None)
            }
            result => result,
        }
    }

    /// At most one due conversation per recipient, excluding occupied lanes.
    /// Only confirmed delivery opens collection; a final answer closes it.
    /// Ack debt remains eligible after closing, including after a lost ack.
    pub fn collect_ready(
        &mut self,
        origin: &str,
        wall_ms: i64,
        limit: usize,
        busy: &[String],
    ) -> Result<Vec<Record>> {
        let mut clock = self.clock()?;
        if clock.paused || limit == 0 {
            return Ok(Vec::new());
        }
        let now = clock.advance(wall_ms);
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT id,collect_attempts FROM (
                  SELECT id,collect_attempts,collect_at,
                    row_number() OVER (PARTITION BY recipient_node ORDER BY collect_at,id) AS rank
                  FROM envelopes e WHERE origin=?1 AND collect_at<=?2
                  AND custody_deadline>?2 AND delivered=1 AND state='delivered'
                  AND recipient_node NOT IN ('',?1)
                  AND recipient_node NOT IN (SELECT value FROM json_each(?4))
                  AND collect_error IS NULL AND (
                    (reply_expected=1 AND collect_done=0) OR EXISTS (
                      SELECT 1 FROM collect_acks a WHERE a.request_origin=e.origin AND a.request_id=e.id)))
                 WHERE rank=1 ORDER BY collect_at,id LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(
                    params![
                        origin,
                        now,
                        limit.min(16) as i64,
                        serde_json::to_string(busy)?
                    ],
                    |r| {
                        Ok((
                            MessageKey {
                                origin_node: origin.into(),
                                message_id: r.get(0)?,
                            },
                            r.get::<_, u32>(1)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut records = Vec::new();
        for (key, attempt) in keys {
            let Some(record) = self.collection_record(&key, wall_ms)? else {
                continue;
            };
            let mut random = [0; 2];
            getrandom::fill(&mut random).map_err(|_| Error::InvalidState)?;
            let base = match attempt {
                0 => 5_000,
                1 => 60_000,
                _ => 299_000,
            };
            let delay = base + i64::from(u16::from_le_bytes(random) % 1001);
            let changed = self.connection.execute(
                "UPDATE envelopes SET collect_at=?3,collect_attempts=MIN(collect_attempts+1,3)
                 WHERE origin=?1 AND id=?2 AND collect_at<=?4",
                params![key.origin_node, key.message_id, now + delay, now],
            )?;
            if changed != 0 {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// A live waiter or a newly enrolled edge shortens a backed-off deadline.
    pub fn collect_fast(
        &mut self,
        origin: &str,
        correlations: &[String],
        peers: &[String],
        wall_ms: i64,
    ) -> Result<()> {
        let mut clock = self.clock()?;
        if clock.paused || (correlations.is_empty() && peers.is_empty()) {
            return Ok(());
        }
        let due = clock.advance(wall_ms).saturating_add(5_000);
        self.connection.execute(
            "UPDATE envelopes SET collect_at=?2,collect_attempts=0 WHERE origin=?1
             AND state='delivered' AND reply_expected=1 AND collect_done=0 AND collect_error IS NULL
             AND collect_at>?2 AND (correlation IN (SELECT value FROM json_each(?3))
               OR recipient_node IN (SELECT value FROM json_each(?4)))",
            params![
                origin,
                due,
                serde_json::to_string(correlations)?,
                serde_json::to_string(peers)?
            ],
        )?;
        Ok(())
    }

    pub fn collection_acks(&self, request: &MessageKey) -> Result<Vec<MessageKey>> {
        let mut stmt = self.connection.prepare(
            "SELECT answer_origin,answer_id FROM collect_acks WHERE request_origin=?1 AND request_id=?2 LIMIT 16")?;
        let keys = stmt
            .query_map(params![request.origin_node, request.message_id], |r| {
                Ok(MessageKey {
                    origin_node: r.get(0)?,
                    message_id: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    pub fn collection_acked(&mut self, request: &MessageKey, keys: &[MessageKey]) -> Result<()> {
        for key in keys {
            self.connection.execute("DELETE FROM collect_acks WHERE request_origin=?1 AND request_id=?2 AND answer_origin=?3 AND answer_id=?4",
                params![request.origin_node,request.message_id,key.origin_node,key.message_id])?;
        }
        Ok(())
    }

    /// Invalid bindings are permanent. Other import errors get three attempts.
    /// Backpressure and suspended/paused writers are not failed imports.
    pub fn collection_failed(
        &mut self,
        request: &MessageKey,
        reason: &str,
        permanent: bool,
    ) -> Result<()> {
        if matches!(reason, "mailbox_full" | "mail_store_full" | "fleet_paused") {
            return Ok(());
        }
        self.connection.execute(
            "UPDATE envelopes SET collect_failures=collect_failures+1,
             collect_error=CASE WHEN ?3 OR collect_failures>=2 THEN ?4 ELSE NULL END
             WHERE origin=?1 AND id=?2 AND collect_done=0",
            params![request.origin_node, request.message_id, permanent, reason],
        )?;
        Ok(())
    }

    pub fn collection_error(&self, origin: &str, correlation: &str) -> Result<Option<String>> {
        Ok(self.connection.query_row(
            "SELECT collect_error FROM envelopes WHERE origin=?1 AND correlation=?2 AND collect_error IS NOT NULL LIMIT 1",
            params![origin,correlation], |r| r.get(0)).optional()?)
    }

    /// Authorize the complete acknowledgement batch before changing custody.
    /// Empty polls do not write the clock or any other state.
    pub fn collect_answers(
        &mut self,
        origin: &str,
        query: &crate::mesh::collect::Collect,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::delivery::Deliver>> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        if origin != query.request.origin_node || query.ack.len() > 16 {
            return Err(Error::InvalidEnvelope);
        }
        let request = self
            .collection_record(&query.request, wall_ms)?
            .ok_or(Error::NotFound)?;
        if request.envelope.return_binding.request != query.request
            || request.envelope.return_binding.collection_token != query.token
        {
            return Err(Error::InvalidEnvelope);
        }
        // Indexed membership checks bound validation to the 16 supplied keys.
        for ack in &query.ack {
            let valid = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM envelopes WHERE origin=?1 AND id=?2 AND request_origin=?3 AND request_id=?4)",
                params![ack.origin_node,ack.message_id,query.request.origin_node,query.request.message_id],
                |r|r.get::<_,bool>(0))?;
            if !valid {
                return Err(Error::InvalidEnvelope);
            }
        }
        for ack in &query.ack {
            if self
                .collection_record(ack, wall_ms)?
                .is_some_and(|r| r.state == "custody" && r.remaining_ms > 0)
            {
                self.finish(ack, Outcome::Delivered, wall_ms)?;
            }
        }
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT origin,id FROM envelopes WHERE request_origin=?1 AND request_id=?2 AND state='custody'
                 ORDER BY origin,id LIMIT 16")?;
            let rows = stmt
                .query_map(
                    params![query.request.origin_node, query.request.message_id],
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
        let mut answers = Vec::new();
        for key in keys {
            if let Some(record) = self.collection_record(&key, wall_ms)? {
                if record.remaining_ms > 0 {
                    answers.push(crate::mesh::delivery::Deliver {
                        envelope: record.envelope,
                        remaining_ms: record.remaining_ms,
                    });
                }
            }
        }
        Ok(answers)
    }
}
