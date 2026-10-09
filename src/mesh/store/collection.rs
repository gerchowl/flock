//! Conversation-scoped collection with durable deadlines and acknowledgement debt.
use super::*;
use crate::mesh::collect::BATCH_CAP;

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
    /// Enrollment makes held answers pushable without changing their identity.
    /// No writable clock or transaction is opened when no row can be sent.
    pub fn activate_held(&mut self, origin: &str, peers: &[String], wall_ms: i64) -> Result<usize> {
        let mut clock = self.clock()?;
        if clock.paused || peers.is_empty() {
            return Ok(0);
        }
        let now = clock.advance(wall_ms);
        let peers = serde_json::to_string(peers)?;
        let ready: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE state='held' AND origin=?1
             AND recipient_node IN (SELECT value FROM json_each(?2)) AND custody_deadline>?3)",
            params![origin, peers, now],
            |r| r.get(0),
        )?;
        if !ready {
            return Ok(0);
        }
        Ok(self.connection.execute(
            "UPDATE envelopes SET state='custody',retry_at=?3,lease_until=0 WHERE state='held'
             AND origin=?1 AND recipient_node IN (SELECT value FROM json_each(?2)) AND custody_deadline>?3",
            params![origin,peers,now])?)
    }

    pub fn hold_answer(&mut self, key: &MessageKey) -> Result<()> {
        self.connection.execute(
            "UPDATE envelopes SET state='held',lease_until=0
            WHERE origin=?1 AND id=?2 AND state='custody' AND request_origin IS NOT NULL",
            params![key.origin_node, key.message_id],
        )?;
        Ok(())
    }

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
                        limit.min(BATCH_CAP) as i64,
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
        let mut ready = Vec::new();
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
            ready.push((record, now + delay));
        }
        if ready.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut records = Vec::new();
        for (record, deadline) in ready {
            let key = &record.envelope.key;
            let changed = tx.execute(
                "UPDATE envelopes SET collect_at=?3,collect_attempts=MIN(collect_attempts+1,3)
                 WHERE origin=?1 AND id=?2 AND collect_at<=?4",
                params![key.origin_node, key.message_id, deadline, now],
            )?;
            if changed != 0 {
                records.push(record);
            }
        }
        tx.commit()?;
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
            "SELECT answer_origin,answer_id FROM collect_acks WHERE request_origin=?1 AND request_id=?2 LIMIT ?3")?;
        let keys = stmt
            .query_map(
                params![request.origin_node, request.message_id, BATCH_CAP as i64],
                |r| {
                    Ok(MessageKey {
                        origin_node: r.get(0)?,
                        message_id: r.get(1)?,
                    })
                },
            )?
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

    pub fn outbound_refused(&self, origin: &str, correlation: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE origin=?1 AND correlation=?2 AND state='refused')",
            params![origin,correlation], |r| r.get(0))?)
    }

    /// Authorize the complete acknowledgement batch before changing custody.
    /// Empty polls do not write the clock or any other state.
    pub fn collect_answers(
        &mut self,
        origin: &str,
        query: &crate::mesh::collect::AnswerCollect,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::delivery::Deliver>> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        if origin != query.request.origin_node || query.ack.len() > BATCH_CAP {
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
        // Indexed membership checks bound validation to the capped supplied keys.
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
            if self.collection_record(ack, wall_ms)?.is_some_and(|r| {
                matches!(r.state.as_str(), "custody" | "held") && r.remaining_ms > 0
            }) {
                self.finish(ack, Outcome::Delivered, wall_ms)?;
            }
        }
        let keys = {
            let mut stmt = self.connection.prepare(
                "SELECT origin,id FROM envelopes WHERE request_origin=?1 AND request_id=?2 AND state IN ('custody','held')
                 ORDER BY origin,id LIMIT ?3")?;
            let rows = stmt
                .query_map(
                    params![
                        query.request.origin_node,
                        query.request.message_id,
                        BATCH_CAP as i64
                    ],
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

impl<D: DiskSpace> Store<D> {
    /// The existing summary carries only ready work, so a backed-off row cannot cause hot polling.
    pub fn has_outbound(&self, local: &str, hub: &str, wall_ms: i64) -> Result<bool> {
        let mut clock = self.clock()?;
        if clock.paused {
            return Ok(false);
        }
        let now = clock.advance(wall_ms);
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM envelopes WHERE origin=?1 AND request_origin IS NULL AND state='held' AND recipient_node=?3 AND retry_at<=?2 AND custody_deadline>?2)", params![local, now, hub], |r| r.get(0))?)
    }

    /// The enrolled hub can collect only local requests addressed to itself.
    /// A lost ack reoffers the same immutable envelope for idempotent import.
    pub fn collect_outbound(
        &mut self,
        local: &str,
        hub: &str,
        query: &crate::mesh::collect::OutboundCollect,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::delivery::Deliver>> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        if query.ack.len() > BATCH_CAP {
            return Err(Error::InvalidEnvelope);
        }
        for ack in &query.ack {
            let record = self
                .collection_record(&ack.key, wall_ms)?
                .ok_or(Error::NotFound)?;
            if ack.key.origin_node != local
                || record.envelope.request_key.is_some()
                || record.envelope.return_binding.recipient_node != hub
                || record.envelope.return_binding.collection_token != ack.token
                || ack
                    .refusal
                    .as_deref()
                    .is_some_and(|r| r.is_empty() || r.len() > 2048)
            {
                return Err(Error::InvalidEnvelope);
            }
        }
        for ack in &query.ack {
            if self
                .collection_record(&ack.key, wall_ms)?
                .is_some_and(|r| r.state == "held" && r.remaining_ms > 0)
            {
                if let Some(reason) = &ack.refusal {
                    let now = self.writable(wall_ms)?;
                    self.connection.execute(
                        "UPDATE envelopes SET state=?3,body=X'',collect_error=?4,outcome_until=?5
                         WHERE origin=?1 AND id=?2 AND state='held' AND custody_deadline>?6",
                        params![
                            ack.key.origin_node,
                            ack.key.message_id,
                            Outcome::Refused.name(),
                            reason,
                            now.saturating_add(CUSTODY_TTL_MS),
                            now
                        ],
                    )?;
                } else {
                    self.finish(&ack.key, Outcome::Delivered, wall_ms)?;
                }
            }
        }
        let now = self.clock()?.advance(wall_ms);
        let keys = {
            let mut stmt = self.connection.prepare("SELECT id FROM envelopes WHERE origin=?1 AND recipient_node=?2 AND request_origin IS NULL AND state='held' AND retry_at<=?3 ORDER BY retry_at,id LIMIT ?4")?;
            let rows = stmt
                .query_map(params![local, hub, now, BATCH_CAP as i64], |r| {
                    Ok(MessageKey {
                        origin_node: local.into(),
                        message_id: r.get(0)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut outbound = Vec::new();
        for key in keys {
            if let Some(record) = self.collection_record(&key, wall_ms)? {
                if record.remaining_ms > 0 {
                    // Lease each offer with durable backoff, including lost acks and bad wire records.
                    self.connection.execute("UPDATE envelopes SET retry_at=?3 + CASE WHEN collect_attempts=0 THEN 5000 WHEN collect_attempts=1 THEN 60000 ELSE 299000 END, collect_attempts=collect_attempts+1 WHERE origin=?1 AND id=?2",
                        params![key.origin_node, key.message_id, now])?;
                    outbound.push(crate::mesh::delivery::Deliver {
                        envelope: record.envelope,
                        remaining_ms: record.remaining_ms,
                    });
                }
            }
        }
        Ok(outbound)
    }
}
