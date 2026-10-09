//! Durable next-hop custody without persisting the live route table.
use super::*;

impl<D: DiskSpace> Store<D> {
    /// Configure the owning node before using forwarding and removal APIs.
    /// This identity is process state, never a persisted route.
    pub fn set_local_node(&mut self, node: &str) {
        self.local_node = node.into();
    }

    // Transport admission carries the hop state atomically with custody.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_forward(
        &mut self,
        envelope: &Envelope,
        remaining_ms: i64,
        hops_left: u8,
        visited: &[String],
        next_hop: &str,
        admission: Admission,
        wall_ms: i64,
    ) -> Result<Accepted> {
        if self.local_node.is_empty() {
            return Err(Error::InvalidState);
        }
        super::super::sign::verify(envelope).map_err(|error| match error {
            "origin_mismatch" => Error::OriginMismatch,
            _ => Error::InvalidSignature,
        })?;
        if envelope.key.origin_node == self.local_node {
            return Err(Error::OriginMismatch);
        }
        if hops_left > 8 || visited.len() > 8 {
            return Err(Error::InvalidEnvelope);
        }
        if visited
            .iter()
            .enumerate()
            .any(|(i, node)| node == &self.local_node || visited[..i].contains(node))
        {
            return Err(Error::LoopDetected);
        }
        if hops_left == 0 && admission != Admission::Inbox {
            return Err(Error::HopBudgetExhausted);
        }
        self.accept_inner(
            envelope,
            remaining_ms,
            admission,
            wall_ms,
            false,
            Some((hops_left, visited, next_hop)),
        )
    }

    pub fn set_next_hop(&mut self, key: &MessageKey, next_hop: &str) -> Result<()> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        self.connection.execute(
            "UPDATE envelopes SET next_hop=?3 WHERE origin=?1 AND id=?2 AND next_hop!=?3 AND state IN ('custody','held')",
            params![key.origin_node,key.message_id,next_hop],
        )?;
        Ok(())
    }

    pub fn push_ready(
        &mut self,
        wall_ms: i64,
        limit: usize,
        pushable_nodes: &[String],
    ) -> Result<Vec<MessageKey>> {
        let mut clock = self.clock()?;
        if clock.paused {
            return Err(Error::Paused);
        }
        if limit == 0 || pushable_nodes.is_empty() {
            return Ok(Vec::new());
        }
        let now = clock.advance(wall_ms);
        let peers = serde_json::to_string(pushable_nodes)?;
        let condition = "state='custody' AND next_hop!='' AND next_hop IN (SELECT value FROM json_each(?1)) AND retry_at<=?2 AND custody_deadline>?2 AND lease_until<=?2";
        let ready: bool = self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM envelopes WHERE {condition})"),
            params![peers, now],
            |r| r.get(0),
        )?;
        if !ready {
            return Ok(Vec::new());
        }
        let keys = self.routing_keys(&format!("SELECT origin,id,rowid FROM envelopes WHERE {condition} ORDER BY retry_at,origin,id LIMIT ?3"), params![peers, now, limit.min(500) as i64])?;
        let mut valid = Vec::new();
        for key in keys {
            if self.collection_record(&key, wall_ms)?.is_some() {
                valid.push(key);
            }
        }
        if valid.is_empty() {
            return Ok(valid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut clock = tx.query_row(
            "SELECT wall,elapsed,paused FROM clock WHERE singleton=1",
            [],
            |r| {
                Ok(Clock {
                    wall_ms: r.get(0)?,
                    elapsed_ms: r.get(1)?,
                    paused: r.get(2)?,
                })
            },
        )?;
        if clock.paused {
            return Err(Error::Paused);
        }
        let now = clock.advance(wall_ms);
        tx.execute(
            "UPDATE clock SET wall=?1,elapsed=?2 WHERE singleton=1",
            params![clock.wall_ms, now],
        )?;
        let mut leased = Vec::new();
        for key in valid {
            if tx.execute(
                "UPDATE envelopes SET lease_until=?3 WHERE origin=?1 AND id=?2 AND state='custody' AND lease_until<=?4 AND retry_at<=?4 AND custody_deadline>?4 AND next_hop!='' AND next_hop IN (SELECT value FROM json_each(?5))",
                params![key.origin_node, key.message_id, now.saturating_add(60_000),now,peers],
            )? == 1 { leased.push(key); }
        }
        tx.commit()?;
        Ok(leased)
    }

    /// Inspect unrouted custody without leasing or advancing its clock.
    /// Malformed rows alone are quarantined so they cannot stall this window.
    pub fn unrouted(&mut self, limit: usize) -> Result<Vec<MessageKey>> {
        let keys = self.routing_keys("SELECT origin,id,rowid FROM envelopes WHERE next_hop='' AND state IN ('custody','held') ORDER BY origin,id LIMIT ?1", [limit.min(500) as i64])?;
        let wall = self.clock()?.wall_ms;
        let mut valid = Vec::new();
        for key in keys {
            if self.collection_record(&key, wall)?.is_some() {
                valid.push(key);
            }
        }
        Ok(valid)
    }

    /// Keep key decoding separate from record decoding, quarantining damaged
    /// SQL values that cannot even produce a MessageKey.
    pub(super) fn routing_keys(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<MessageKey>> {
        let mut statement = self.connection.prepare(sql)?;
        let mut rows = statement.query(params)?;
        let mut keys = Vec::new();
        let mut bad_rows = Vec::new();
        while let Some(row) = rows.next()? {
            let origin = row.get::<_, String>(0);
            let id = row.get::<_, String>(1);
            match (origin, id) {
                (Ok(origin_node), Ok(message_id)) => keys.push(MessageKey {
                    origin_node,
                    message_id,
                }),
                _ => bad_rows.push(row.get::<_, i64>(2)?),
            }
        }
        drop(rows);
        drop(statement);
        for rowid in bad_rows {
            self.connection.execute(
                "UPDATE envelopes SET state='quarantined',body=X'' WHERE rowid=?1",
                [rowid],
            )?;
        }
        Ok(keys)
    }

    pub fn quarantined_count(&self) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM envelopes WHERE state='quarantined'",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }
}
