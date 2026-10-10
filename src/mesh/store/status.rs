//! Read-only conversation views, independent of audit retention.
use super::*;
use crate::api::schema::MsgReplyInfo;
use ed25519_dalek::{Signature, Signer, SigningKey};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReference {
    pub key: MessageKey,
    pub correlation_id: String,
    pub signature: Vec<u8>,
}

/// The receiver-side states a read receipt may carry back to the origin.
pub(super) const RECEIPT_STATES: [&str; 6] = [
    "delivered",
    "recipient_gone",
    "refused",
    "read",
    "expired",
    "outcome_retention_elapsed",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum ReceiptImport {
    Applied,
    Duplicate,
    OriginalNotReady,
}

pub struct Status {
    pub state: String,
    pub reference: StatusReference,
    pub reply: Option<MsgReplyInfo>,
    pub detail: Option<String>,
}

impl<D: DiskSpace> Store<D> {
    /// The newest message `origin` minted under `correlation`. Scoped to the
    /// asking node, so a correlation id another node reused for mail it sent
    /// here never answers for this node's own message.
    pub fn status(&self, origin: &str, correlation: &str, wall_ms: i64) -> Result<Option<Status>> {
        let key = self
            .connection
            .query_row(
                "SELECT origin,id FROM envelopes WHERE origin=?1 AND correlation=?2
                 ORDER BY id DESC LIMIT 1",
                params![origin, correlation],
                |r| {
                    Ok(MessageKey {
                        origin_node: r.get(0)?,
                        message_id: r.get(1)?,
                    })
                },
            )
            .optional()?;
        let Some(key) = key else { return Ok(None) };
        self.status_key(&key, wall_ms, None)
    }

    fn signing_key(&self) -> Result<SigningKey> {
        let secret: Vec<u8> = self.connection.query_row(
            "SELECT secret FROM status_signer WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        Ok(SigningKey::from_bytes(
            &secret.try_into().map_err(|_| Error::InvalidState)?,
        ))
    }

    fn reference(&self, key: &MessageKey, correlation: &str) -> Result<StatusReference> {
        let payload = serde_json::to_vec(&("flock-status-v1", key, correlation))?;
        Ok(StatusReference {
            key: key.clone(),
            correlation_id: correlation.into(),
            signature: self.signing_key()?.sign(&payload).to_bytes().to_vec(),
        })
    }

    pub fn referenced_status(
        &self,
        correlation: &str,
        reference: &StatusReference,
        wall_ms: i64,
    ) -> Result<Status> {
        let payload =
            serde_json::to_vec(&("flock-status-v1", &reference.key, &reference.correlation_id))?;
        let signature =
            Signature::from_slice(&reference.signature).map_err(|_| Error::InvalidEnvelope)?;
        if reference.correlation_id != correlation
            || self
                .signing_key()?
                .verifying_key()
                .verify_strict(&payload, &signature)
                .is_err()
        {
            return Err(Error::InvalidEnvelope);
        }
        Ok(self
            .status_key(&reference.key, wall_ms, Some(reference))?
            .unwrap_or_else(|| Status {
                state: "outcome_retention_elapsed".into(),
                reference: reference.clone(),
                reply: None,
                detail: None,
            }))
    }

    pub(crate) fn status_key(
        &self,
        key: &MessageKey,
        wall_ms: i64,
        reference: Option<&StatusReference>,
    ) -> Result<Option<Status>> {
        let Some(record) = self.get(key)? else {
            return Ok(None);
        };
        let now = self.clock()?.advance(wall_ms);
        let (custody, inbox, until, remote, error): (i64, Option<i64>, Option<i64>, Option<String>, Option<String>) = self.connection.query_row(
            "SELECT custody_deadline,inbox_deadline,outcome_until,remote_state,collect_error FROM envelopes WHERE origin=?1 AND id=?2",
            params![key.origin_node,key.message_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let mut state = match record.state.as_str() {
            _ if until.is_some_and(|until| until <= now) => "outcome_retention_elapsed",
            "custody" | "held" | "transferred" if custody <= now => "expired",
            "inbox" if inbox.is_some_and(|until| until <= now) => "expired",
            "custody" if record.envelope.request_key.is_none() => "queued",
            "held" if record.envelope.request_key.is_none() => "queued",
            "inbox" => "delivered",
            "inbox_expired" => "expired",
            "transferred" => "custody",
            "delivered" if record.envelope.request_key.is_some() => "collected",
            state => state,
        }
        .to_string();
        if state == "delivered" {
            if let Some(remote) = remote {
                state = remote;
            }
        }
        if error.is_some() && !matches!(state.as_str(), "refused" | "recipient_gone") {
            state = "collect_failed".into();
        }
        let mut reply = None;
        if state != "outcome_retention_elapsed" {
            for answer_key in self.by_request_key(key)? {
                let Some(answer) = self.get(&answer_key)? else {
                    continue;
                };
                if !matches!(answer.state.as_str(), "inbox" | "read") {
                    continue;
                }
                let envelope = answer.envelope;
                if envelope.kind == Kind::Receipt {
                    continue;
                }
                let deferred = crate::app::mailboxes::is_deferral(&envelope.correlation_id);
                if reply
                    .as_ref()
                    .is_some_and(|r: &MsgReplyInfo| r.kind == "reply")
                {
                    break;
                }
                reply = Some(MsgReplyInfo {
                    correlation_id: envelope.correlation_id,
                    kind: if deferred { "deferral" } else { "reply" }.into(),
                    body: serde_json::from_slice::<serde_json::Value>(&envelope.body)?["message"]
                        ["body"]
                        .as_str()
                        .ok_or(Error::InvalidEnvelope)?
                        .into(),
                    from_agent: (!envelope.sender.is_empty()).then_some(envelope.sender),
                    from_pane: None,
                    from_host: self.origin_name(&answer_key.origin_node)?,
                    held: envelope.target_agent.is_empty(),
                });
            }
        }
        Ok(Some(Status {
            state,
            reference: match reference {
                Some(reference) => reference.clone(),
                None => self.reference(key, &record.envelope.correlation_id)?,
            },
            reply,
            detail: error,
        }))
    }

    /// Called only after the collection edge and conversation token are authenticated.
    pub fn receipt(&self, key: &MessageKey, wall_ms: i64) -> Result<Option<String>> {
        Ok(self
            .status_key(key, wall_ms, None)?
            .map(|s| s.state)
            .filter(|state| RECEIPT_STATES.contains(&state.as_str())))
    }

    pub fn import_receipt(&mut self, key: &MessageKey, state: &str) -> Result<ReceiptImport> {
        self.import_receipt_with_detail(key, state, None)
    }

    /// A terminal refusal carries the recipient's reason, shown as the
    /// sender's status `detail`.
    pub fn import_receipt_with_detail(
        &mut self,
        key: &MessageKey,
        state: &str,
        detail: Option<&str>,
    ) -> Result<ReceiptImport> {
        let incoming = RECEIPT_STATES
            .iter()
            .position(|candidate| *candidate == state)
            .ok_or(Error::InvalidEnvelope)?;
        let original: Option<(String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT state,remote_state FROM envelopes WHERE origin=?1 AND id=?2",
                params![key.origin_node, key.message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((original_state, remote)) = original else {
            return Ok(ReceiptImport::OriginalNotReady);
        };
        let current = remote
            .as_deref()
            .into_iter()
            .chain(std::iter::once(original_state.as_str()))
            .filter_map(|state| {
                RECEIPT_STATES
                    .iter()
                    .position(|candidate| *candidate == state)
            })
            .max();
        if remote
            .as_deref()
            .into_iter()
            .chain(std::iter::once(original_state.as_str()))
            .any(|state| matches!(state, "read" | "recipient_gone" | "refused" | "expired"))
            || (state == "outcome_retention_elapsed"
                && remote.as_deref().unwrap_or(&original_state) != "delivered")
            || current.is_some_and(|current| current >= incoming)
        {
            return Ok(ReceiptImport::Duplicate);
        }
        if !matches!(
            original_state.as_str(),
            "delivered" | "held" | "transferred"
        ) {
            return Ok(ReceiptImport::OriginalNotReady);
        }
        // Delivery evidence does not acknowledge a held offer. Keep its body
        // for retransmission until the custody ack arrives. Final outcomes can
        // settle it immediately, including a read that outran its own ack.
        let settle = original_state != "held" || state != "delivered";
        let changed = self.connection.execute(
            "UPDATE envelopes SET remote_state=?3,delivered=1,
             body=CASE WHEN ?4 AND state IN ('held','transferred') THEN X'' ELSE body END,
             state=CASE WHEN ?4 AND state IN ('held','transferred') THEN 'delivered' ELSE state END,
             collect_error=COALESCE(?5,collect_error)
             WHERE origin=?1 AND id=?2",
            params![key.origin_node, key.message_id, state, settle, detail],
        )?;
        Ok(if changed == 1 {
            ReceiptImport::Applied
        } else {
            ReceiptImport::OriginalNotReady
        })
    }
}

impl<D: DiskSpace> Store<D> {
    /// Every retained mesh attempt, read once at boot to seed the in-memory
    /// registry. Live queries read that registry, never this table.
    pub fn delivery_attempts(&self) -> Result<Vec<crate::api::schema::DeliveryAttempt>> {
        let mut statement = self
            .connection
            .prepare("SELECT evidence FROM delivery_attempts ORDER BY id")?;
        let rows = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(Error::from))
            .collect()
    }

    /// Store one mesh submission transition at its producer, never during a
    /// status read. Each write is keyed: one upsert, and for a new attempt
    /// over the cap one indexed eviction of the oldest evictable row. The
    /// bound and protection rule are [`delivery_attempts::DeliveryAttempts`]'s
    /// own: a pending attempt is never evicted, nor an unconfirmed one while
    /// a correlated message is still in a local inbox.
    pub fn record_attempt(
        &mut self,
        attempt: &crate::api::schema::DeliveryAttempt,
    ) -> Result<bool> {
        let ids = serde_json::to_string(&attempt.correlation_ids)?;
        let (relevant, known): (bool, bool) = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM json_each(?1) j JOIN envelopes e ON e.correlation=j.value),
                    EXISTS(SELECT 1 FROM delivery_attempts WHERE id=?2)",
            params![ids, attempt.attempt_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !relevant && !known {
            return Ok(false);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !known {
            let retained: i64 =
                tx.query_row("SELECT count(*) FROM delivery_attempts", [], |r| r.get(0))?;
            if retained >= crate::app::mailboxes::MAX_SEEN as i64 {
                let evicted = tx.execute(
                    "DELETE FROM delivery_attempts WHERE id=(
                       SELECT a.id FROM delivery_attempts a WHERE a.finished=1
                         AND (a.state!='unconfirmed' OR NOT EXISTS(
                           SELECT 1 FROM json_each(a.correlations) j JOIN envelopes e
                             ON e.correlation=j.value WHERE e.state='inbox'))
                       ORDER BY a.queued_at,a.id LIMIT 1)",
                    [],
                )?;
                if evicted == 0 {
                    return Err(Error::MailStoreFull);
                }
            }
        }
        tx.execute(
            "INSERT INTO delivery_attempts(id,evidence,queued_at,finished,state,correlations)
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET evidence=excluded.evidence,
             finished=excluded.finished,state=excluded.state,correlations=excluded.correlations",
            params![
                attempt.attempt_id,
                serde_json::to_string(attempt)?,
                attempt.queued_at_ms as i64,
                attempt.finished_at_ms.is_some(),
                attempt.state,
                ids
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

/// The receipt a receiver owes for one inbox row, in the same precedence as
/// [`Store::status_key`], except that `recipient_gone` is terminal and wins
/// over retention: the origin may hold the original as `transferred` and
/// applies retention only over `delivered`. Evaluated in SQL so selection and
/// the sent mark can never disagree and a row cannot sit unsendable at the
/// window's head.
const RECEIPT_STATE_SQL: &str = "CASE
    WHEN state='recipient_gone' THEN 'recipient_gone'
    WHEN outcome_until IS NOT NULL AND outcome_until<=?2 THEN 'outcome_retention_elapsed'
    WHEN state='inbox' AND inbox_deadline IS NOT NULL AND inbox_deadline<=?2 THEN 'expired'
    WHEN state='inbox' THEN 'delivered'
    WHEN state='read' THEN 'read'
    ELSE 'expired' END";

impl<D: DiskSpace> Store<D> {
    /// Receipts owed to `origin`: rows whose receipt state changed since the
    /// last one it accepted. Every state is markable, so a row leaves this
    /// window after one successful exchange and id order cannot starve the
    /// newer ones behind it.
    pub fn pending_receipts(
        &mut self,
        origin: &str,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::collect::Receipt>> {
        let now = self.clock()?.advance(wall_ms);
        let mut stmt = self.connection.prepare(&format!(
            "WITH due AS MATERIALIZED (
                SELECT rowid AS record_rowid,* FROM envelopes WHERE origin=?1 AND kind='message' AND state IN ('inbox','read','inbox_expired')
                AND ((outcome_until<=?2 AND receipt_sent IS NOT 'outcome_retention_elapsed')
                  OR ((outcome_until IS NULL OR outcome_until>?2) AND (
                    (state='read' AND receipt_sent IS NOT 'read')
                    OR (state='inbox_expired' AND receipt_sent IS NOT 'expired')
                    OR (state='inbox' AND inbox_deadline<=?2 AND receipt_sent IS NOT 'expired')
                    OR (state='inbox' AND (inbox_deadline IS NULL OR inbox_deadline>?2) AND receipt_sent IS NOT 'delivered'))))
                ORDER BY id LIMIT ?3)
             SELECT record_rowid,id,{RECEIPT_STATE_SQL} FROM due"
        ))?;
        let mut rows = stmt.query(params![origin, now, crate::mesh::collect::BATCH_CAP as i64])?;
        let mut selected = Vec::new();
        let mut bad_rows = Vec::new();
        while let Some(row) = rows.next()? {
            match (row.get::<_, String>(1), row.get::<_, String>(2)) {
                (Ok(message_id), Ok(state)) => selected.push((
                    MessageKey {
                        origin_node: origin.into(),
                        message_id,
                    },
                    state,
                )),
                _ => bad_rows.push(row.get::<_, i64>(0)?),
            }
        }
        drop(rows);
        drop(stmt);
        quarantine::rows(
            &self.connection,
            &bad_rows,
            &self.path,
            "invalid receipt row",
            wall_ms,
        )?;
        let mut receipts = Vec::new();
        for (key, state) in selected {
            if let Some(record) = self.collection_record(&key, wall_ms)? {
                receipts.push(crate::mesh::collect::Receipt {
                    key,
                    token: record.envelope.return_binding.collection_token,
                    state,
                });
            }
        }
        Ok(receipts)
    }

    /// Mark receipts the origin accepted. The mark is the receipt state
    /// itself, so a later change (read, expiry) selects the row again.
    pub fn receipts_sent(&mut self, receipts: &[crate::mesh::collect::Receipt]) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for receipt in receipts {
            tx.execute(
                "UPDATE envelopes SET receipt_sent=?3 WHERE origin=?1 AND id=?2
                 AND receipt_sent IS NOT ?3",
                params![
                    receipt.key.origin_node,
                    receipt.key.message_id,
                    receipt.state
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl<D: DiskSpace> Store<D> {
    /// Read only changed, multihop inbox outcomes. Custody is created before
    /// marking sent. The owed state is [`pending_receipts`]' own, so expiry
    /// and retention reach a multihop origin too, and a late read still does.
    /// Both final receipts settle a row, so the `routed_receipts` index skips
    /// it and the pass does not rescan every multihop row until GC.
    ///
    /// [`pending_receipts`]: Store::pending_receipts
    pub fn routed_receipts(&mut self, wall_ms: i64) -> Result<Vec<crate::mesh::collect::Receipt>> {
        let now = self.clock()?.advance(wall_ms);
        let mut stmt = self.connection.prepare(&routed_receipts_sql())?;
        let mut rows = stmt.query(params![self.local_node, now])?;
        let mut selected = Vec::new();
        let mut bad_rows = Vec::new();
        while let Some(row) = rows.next()? {
            match (
                row.get::<_, String>(1),
                row.get::<_, String>(2),
                row.get::<_, String>(3),
            ) {
                (Ok(origin_node), Ok(message_id), Ok(state)) => selected.push((
                    MessageKey {
                        origin_node,
                        message_id,
                    },
                    state,
                )),
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
        let mut receipts = Vec::new();
        for (key, state) in selected {
            if let Some(record) = self.collection_record(&key, wall_ms)? {
                receipts.push(crate::mesh::collect::Receipt {
                    key,
                    token: record.envelope.return_binding.collection_token,
                    state,
                });
            }
        }
        Ok(receipts)
    }
}

/// Settled rows are excluded in the `routed_receipts` partial index's own
/// words, so the planner can use it.
pub(super) fn routed_receipts_sql() -> String {
    format!(
        "SELECT rowid,origin,id,{RECEIPT_STATE_SQL} FROM envelopes
         WHERE json_valid(visited) AND json_array_length(visited)>1
         AND recipient_node=?1 AND request_origin IS NULL AND kind='message'
         AND state IN ('inbox','read','inbox_expired','recipient_gone')
         AND receipt_sent IS NOT 'outcome_retention_elapsed' AND receipt_sent IS NOT 'recipient_gone'
         AND receipt_sent IS NOT {RECEIPT_STATE_SQL}
         ORDER BY origin,id LIMIT 16"
    )
}
