//! Authoritative local removals and last-known owner hints.
use super::*;

impl<D: DiskSpace> Store<D> {
    pub fn is_tombstoned(&self, agent: &str, session: &str, wall_ms: i64) -> Result<bool> {
        let now = self.clock()?.advance(wall_ms);
        Ok(self.connection.query_row("SELECT EXISTS(SELECT 1 FROM agent_tombstones WHERE agent_id=?1 AND session=?2 AND until>?3)", params![agent, session, now], |r| r.get(0))?)
    }

    pub fn tombstone(
        &mut self,
        agent: &str,
        session: &str,
        reason: &str,
        wall_ms: i64,
    ) -> Result<Vec<MessageKey>> {
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
        let mut affected = Vec::new();
        let mut bad_rows = Vec::new();
        {
            let mut statement = tx.prepare("SELECT rowid,origin,id,metadata FROM envelopes WHERE state IN ('inbox','custody','held') AND (next_hop='' OR next_hop=?1)")?;
            let mut rows = statement.query([&self.local_node])?;
            while let Some(row) = rows.next()? {
                let decoded = (|| -> Result<(MessageKey, Envelope)> {
                    let key = MessageKey {
                        origin_node: row.get(1)?,
                        message_id: row.get(2)?,
                    };
                    let envelope: Envelope = serde_json::from_str(&row.get::<_, String>(3)?)?;
                    if key != envelope.key || !key.is_valid() {
                        return Err(Error::InvalidEnvelope);
                    }
                    Ok((key, envelope))
                })();
                match decoded {
                    Ok((key, envelope))
                        if envelope.target_agent == agent && envelope.target_session == session =>
                    {
                        affected.push(key)
                    }
                    Ok(_) => (),
                    Err(_) => bad_rows.push(row.get::<_, i64>(0)?),
                }
            }
        }
        for rowid in bad_rows {
            quarantine::row(
                &tx,
                rowid,
                &self.path,
                "invalid tombstone candidate",
                wall_ms,
            )?;
        }
        tx.execute(
            "UPDATE clock SET wall=?1,elapsed=?2 WHERE singleton=1",
            params![clock.wall_ms, now],
        )?;
        tx.execute("INSERT INTO agent_tombstones VALUES(?1,?2,?3,?4,?5) ON CONFLICT(agent_id,session) DO UPDATE SET reason=excluded.reason,at=excluded.at,until=excluded.until", params![agent,session,reason,now,now.saturating_add(CUSTODY_TTL_MS)])?;
        for key in &affected {
            tx.execute("UPDATE envelopes SET state='recipient_gone',body=X'',outcome_until=?3,collect_error=?4 WHERE origin=?1 AND id=?2", params![key.origin_node,key.message_id,now.saturating_add(CUSTODY_TTL_MS),reason])?;
        }
        tx.commit()?;
        Ok(affected)
    }

    pub fn note_owner(
        &mut self,
        agent: &str,
        node: &str,
        name: &str,
        wall_ms: i64,
    ) -> Result<bool> {
        if self
            .owner(agent)?
            .is_some_and(|owner| owner.node_id == node && owner.name == name)
        {
            return Ok(false);
        }
        let now = self.clock()?.advance(wall_ms);
        Ok(self.connection.execute("INSERT INTO agent_owners VALUES(?1,?2,?3,?4) ON CONFLICT(agent_id) DO UPDATE SET node_id=excluded.node_id,name=excluded.name,seen=excluded.seen WHERE node_id!=excluded.node_id OR name!=excluded.name", params![agent,node,name,now])? != 0)
    }

    pub fn owner(&self, agent: &str) -> Result<Option<Owner>> {
        Ok(self
            .connection
            .query_row(
                "SELECT node_id,name,seen FROM agent_owners WHERE agent_id=?1",
                [agent],
                |r| {
                    Ok(Owner {
                        node_id: r.get(0)?,
                        name: r.get(1)?,
                        seen: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }
}
