//! Durable next-hop custody without persisting the live route table.
use super::*;

impl<D: DiskSpace> Store<D> {
    /// Reserve a durable advert epoch once per process. Every sequence in this
    /// epoch sorts below the next boot, even after rollback or a paused restart.
    /// This updates only metadata, leaving custody's paused clock untouched.
    pub fn reserve_route_boot(&mut self, wall_ms: i64) -> Result<u64> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<i64> = tx
            .query_row(
                "SELECT value FROM mesh_meta WHERE name='route_boot_ms'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let clock: i64 =
            tx.query_row("SELECT wall FROM clock WHERE singleton=1", [], |r| r.get(0))?;
        let next = previous
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::InvalidState)?
            .max(clock)
            .max(wall_ms)
            .max(0);
        tx.execute("INSERT INTO mesh_meta VALUES('route_boot_ms',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value", [next])?;
        tx.commit()?;
        Ok(next as u64)
    }

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
        if hops_left > 8 || visited.len() > 9 || visited.last() != Some(&self.local_node) {
            return Err(Error::InvalidEnvelope);
        }
        if visited
            .iter()
            .enumerate()
            .any(|(i, node)| visited[..i].contains(node))
        {
            return Err(Error::LoopDetected);
        }
        self.accept_inner(
            envelope,
            remaining_ms,
            admission,
            wall_ms,
            false,
            Some((hops_left, visited, next_hop)),
            None,
        )
    }

    /// Persist the origin's route and hop budget in the acceptance transaction.
    pub fn accept_origin(
        &mut self,
        envelope: &Envelope,
        next_hop: &str,
        admission: Admission,
        hops_left: u8,
        wall_ms: i64,
    ) -> Result<Accepted> {
        self.accept_inner(
            envelope,
            CUSTODY_TTL_MS,
            admission,
            wall_ms,
            false,
            Some((
                hops_left,
                std::slice::from_ref(&envelope.key.origin_node),
                next_hop,
            )),
            None,
        )
    }

    /// Upgrade unsigned local custody just before sending it. Foreign mail can
    /// never acquire this node's signature, and terminal metadata is untouched.
    pub(crate) fn seal_local_record(&mut self, record: &mut Record) -> Result<()> {
        if !record.envelope.signature.is_empty()
            || record.envelope.key.origin_node != self.local_node
        {
            return Ok(());
        }
        let identity = super::super::identity::NodeIdentity::load()?;
        if identity.node_id() != record.envelope.key.origin_node {
            return Err(Error::OriginMismatch);
        }
        super::super::sign::seal(&mut record.envelope, &identity);
        if record.visited.is_empty() {
            record.visited.push(record.envelope.key.origin_node.clone());
        }
        let mut metadata = record.envelope.clone();
        metadata.body.clear();
        self.connection.execute(
            "UPDATE envelopes SET metadata=?3,fingerprint=?4,visited=?5 WHERE origin=?1 AND id=?2",
            params![
                record.envelope.key.origin_node,
                record.envelope.key.message_id,
                serde_json::to_string(&metadata)?,
                fingerprint(&record.envelope)?,
                serde_json::to_string(&record.visited)?
            ],
        )?;
        Ok(())
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

    pub fn route_custody(
        &mut self,
        key: &MessageKey,
        next_hop: &str,
        admission: Admission,
    ) -> Result<()> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        self.connection.execute("UPDATE envelopes SET next_hop=?3,state=?4,retry_at=0,lease_until=0 WHERE origin=?1 AND id=?2 AND next_hop!=?3 AND state IN ('custody','held')",
            params![key.origin_node, key.message_id, next_hop, if admission == Admission::Held { "held" } else { "custody" }])?;
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
        let condition = "state IN ('custody','held') AND next_hop!='' AND next_hop IN (SELECT value FROM json_each(?1)) AND retry_at<=?2 AND custody_deadline>?2 AND lease_until<=?2";
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
                "UPDATE envelopes SET state='custody',lease_until=?3 WHERE origin=?1 AND id=?2 AND state IN ('custody','held') AND lease_until<=?4 AND retry_at<=?4 AND custody_deadline>?4 AND next_hop!='' AND next_hop IN (SELECT value FROM json_each(?5))",
                params![key.origin_node, key.message_id, now.saturating_add(60_000),now,peers],
            )? == 1 { leased.push(key); }
        }
        tx.commit()?;
        Ok(leased)
    }

    /// A withdrawn adjacent edge cannot retain a request's routing lease.
    /// Called only after topology changes, and writes only affected custody.
    pub fn withdraw_request_hops(&mut self, live: &[String]) -> Result<()> {
        if self.clock()?.paused {
            return Err(Error::Paused);
        }
        let live = serde_json::to_string(live)?;
        let condition = "state IN ('custody','held') AND request_origin IS NULL AND kind='message' AND next_hop!='' AND next_hop NOT IN (SELECT value FROM json_each(?1))";
        let any: bool = self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM envelopes WHERE {condition})"),
            [&live],
            |r| r.get(0),
        )?;
        if any {
            self.connection.execute(
                &format!("UPDATE envelopes SET next_hop='',lease_until=0 WHERE {condition}"),
                [&live],
            )?;
        }
        Ok(())
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
        quarantine::rows(
            &self.connection,
            &bad_rows,
            &self.path,
            "invalid routing key",
            self.clock()?.wall_ms,
        )?;
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
