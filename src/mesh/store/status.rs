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
pub(super) const RECEIPT_STATES: [&str; 4] =
    ["delivered", "read", "expired", "outcome_retention_elapsed"];

pub struct Status {
    pub state: String,
    pub reference: StatusReference,
    pub reply: Option<MsgReplyInfo>,
    pub detail: Option<String>,
}

impl<D: DiskSpace> Store<D> {
    pub fn status(&self, correlation: &str, wall_ms: i64) -> Result<Option<Status>> {
        let key = self
            .connection
            .query_row(
                "SELECT origin,id FROM envelopes WHERE correlation=?1 ORDER BY id DESC LIMIT 1",
                [correlation],
                |r| {
                    Ok(MessageKey {
                        origin_node: r.get(0)?,
                        message_id: r.get(1)?,
                    })
                },
            )
            .optional()?;
        let Some(key) = key else { return Ok(None) };
        self.status_key(&key, wall_ms)
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
            .status_key(&reference.key, wall_ms)?
            .unwrap_or_else(|| Status {
                state: "outcome_retention_elapsed".into(),
                reference: reference.clone(),
                reply: None,
                detail: None,
            }))
    }

    fn status_key(&self, key: &MessageKey, wall_ms: i64) -> Result<Option<Status>> {
        let Some(record) = self.get(key)? else {
            return Ok(None);
        };
        let now = self.clock()?.advance(wall_ms);
        let (custody, inbox, until, remote, error): (i64, Option<i64>, Option<i64>, Option<String>, Option<String>) = self.connection.query_row(
            "SELECT custody_deadline,inbox_deadline,outcome_until,remote_state,collect_error FROM envelopes WHERE origin=?1 AND id=?2",
            params![key.origin_node,key.message_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let mut state = match record.state.as_str() {
            _ if until.is_some_and(|until| until <= now) => "outcome_retention_elapsed",
            "custody" | "held" if custody <= now => "expired",
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
        if error.is_some() && state != "refused" {
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
            reference: self.reference(key, &record.envelope.correlation_id)?,
            reply,
            detail: error,
        }))
    }

    /// Called only after the collection edge and conversation token are authenticated.
    pub fn receipt(&self, key: &MessageKey, wall_ms: i64) -> Result<Option<String>> {
        Ok(self
            .status_key(key, wall_ms)?
            .map(|s| s.state)
            .filter(|state| RECEIPT_STATES.contains(&state.as_str())))
    }

    pub fn import_receipt(&mut self, key: &MessageKey, state: &str) -> Result<()> {
        if !RECEIPT_STATES.contains(&state) {
            return Err(Error::InvalidEnvelope);
        }
        self.connection.execute(
            "UPDATE envelopes SET remote_state=?3 WHERE origin=?1 AND id=?2
            AND (remote_state IS NULL OR remote_state='delivered') AND remote_state IS NOT ?3
            AND state='delivered'",
            params![key.origin_node, key.message_id, state],
        )?;
        Ok(())
    }
}

impl<D: DiskSpace> Store<D> {
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

    /// Store mesh submission transitions at their producer, never during status reads.
    pub fn record_attempt(
        &mut self,
        attempt: &crate::api::schema::DeliveryAttempt,
    ) -> Result<bool> {
        let ids = serde_json::to_string(&attempt.correlation_ids)?;
        let relevant: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM envelopes WHERE correlation IN (SELECT value FROM json_each(?1)))
             OR EXISTS(SELECT 1 FROM delivery_attempts WHERE id=?2)",
            params![ids,attempt.attempt_id], |r| r.get(0))?;
        if !relevant {
            return Ok(false);
        }
        let mut registry = delivery_attempts::DeliveryAttempts::new(0);
        let previous = self.delivery_attempts()?;
        if previous.iter().any(|a| a == attempt) {
            return Ok(true);
        }
        for old in &previous {
            registry.record(old.clone());
        }
        if !previous.iter().any(|a| a.attempt_id == attempt.attempt_id) {
            let mut statement = self
                .connection
                .prepare("SELECT correlation FROM envelopes WHERE state='inbox'")?;
            let queued = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<std::collections::HashSet<_>, _>>()?;
            registry
                .reserve_id(|id| queued.contains(id))
                .map_err(|_| Error::MailStoreFull)?;
        }
        registry.record(attempt.clone());
        let retained = registry
            .snapshot()
            .into_iter()
            .map(|a| a.attempt_id)
            .collect::<Vec<_>>();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM delivery_attempts WHERE id NOT IN (SELECT value FROM json_each(?1))",
            [serde_json::to_string(&retained)?],
        )?;
        tx.execute("INSERT INTO delivery_attempts VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET evidence=excluded.evidence",
            params![attempt.attempt_id,serde_json::to_string(attempt)?])?;
        tx.commit()?;
        Ok(true)
    }
}

impl<D: DiskSpace> Store<D> {
    pub fn pending_receipts(
        &self,
        origin: &str,
        wall_ms: i64,
    ) -> Result<Vec<crate::mesh::collect::Receipt>> {
        let mut stmt = self.connection.prepare("SELECT id FROM envelopes WHERE origin=?1
            AND state IN ('inbox','read','inbox_expired') AND (receipt_sent IS NULL OR receipt_sent!=state)
            ORDER BY id LIMIT 16")?;
        let keys = stmt
            .query_map([origin], |r| {
                Ok(MessageKey {
                    origin_node: origin.into(),
                    message_id: r.get(0)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut receipts = Vec::new();
        for key in keys {
            if let (Some(record), Some(state)) = (self.get(&key)?, self.receipt(&key, wall_ms)?) {
                receipts.push(crate::mesh::collect::Receipt {
                    key,
                    token: record.envelope.return_binding.collection_token,
                    state,
                });
            }
        }
        Ok(receipts)
    }

    pub fn receipts_sent(&mut self, receipts: &[crate::mesh::collect::Receipt]) -> Result<()> {
        for receipt in receipts {
            let state = match receipt.state.as_str() {
                "delivered" => "inbox",
                "expired" => "inbox_expired",
                state => state,
            };
            self.connection.execute("UPDATE envelopes SET receipt_sent=?3 WHERE origin=?1 AND id=?2 AND state=?3 AND (receipt_sent IS NULL OR receipt_sent!=?3)",
                params![receipt.key.origin_node,receipt.key.message_id,state])?;
        }
        Ok(())
    }
}
