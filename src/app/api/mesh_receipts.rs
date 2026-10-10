//! Signed outcomes return through the same custody routes as answers.
use super::{mesh_mail::error_code, messages::now_ms};
use crate::{
    app::App,
    mesh::{
        delivery::Deliver,
        hello::with_store,
        key::MessageKey,
        store::{Accepted, Admission, Envelope, ReturnBinding, CUSTODY_TTL_MS},
    },
};

pub(super) fn decode_receipt(mail: &Envelope) -> Result<crate::mesh::collect::Receipt, String> {
    if let Some(key) = &mail.request_key {
        #[derive(serde::Deserialize)]
        struct Body {
            state: String,
        }
        let body: Body =
            serde_json::from_slice(&mail.body).map_err(|_| "invalid_envelope".to_string())?;
        Ok(crate::mesh::collect::Receipt {
            key: key.clone(),
            token: mail.return_binding.collection_token.clone(),
            state: body.state,
        })
    } else {
        serde_json::from_slice(&mail.body).map_err(|_| "invalid_envelope".into())
    }
}

/// The recipient's reason for a terminal refusal, carried beside its state.
fn receipt_detail(mail: &Envelope, state: &str) -> Option<String> {
    if !matches!(state, "refused" | "recipient_gone") {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Body {
        detail: Option<String>,
    }
    serde_json::from_slice::<Body>(&mail.body)
        .ok()?
        .detail
        .map(|detail| detail.chars().take(512).collect())
}

/// Seal `state` as a receipt for `original`, signed by this node. Only the
/// recipient's own signature is accepted at the origin.
fn mint_receipt(
    original: &Envelope,
    origin: &str,
    identity: &crate::mesh::identity::NodeIdentity,
    body: serde_json::Value,
    state: &str,
    token: Vec<u8>,
) -> Result<Envelope, String> {
    let mut mail = original.clone();
    mail.key = MessageKey::mint(origin.into(), now_ms()).map_err(|e| e.to_string())?;
    mail.kind = crate::mesh::store::Kind::Receipt;
    mail.request_key = Some(original.key.clone());
    mail.target_agent = mail.sender.clone();
    mail.correlation_id = format!("receipt:{}:{}", original.key.message_id, state);
    mail.in_reply_to = None;
    mail.intent = "\"fyi\"".into();
    mail.return_binding =
        ReturnBinding::mint(mail.key.clone(), original.key.origin_node.clone(), vec![])
            .map_err(|e| e.to_string())?;
    mail.return_binding.collection_token = token;
    mail.body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    crate::mesh::sign::seal(&mut mail, identity);
    Ok(mail)
}

impl App {
    /// The recipient's signed terminal outcome for a forwarded message it
    /// refuses (#872). The upstream hub returns it toward the origin, which
    /// accepts only this node's signature for it, so no hub can mint one.
    pub(super) fn refusal_receipt(&self, delivery: &Deliver, reason: &str) -> Option<Envelope> {
        let envelope = &delivery.envelope;
        let state = match reason.split(':').next() {
            Some("recipient_gone") => "recipient_gone",
            Some("msg_not_allowed") => "refused",
            _ => return None,
        };
        let local = self.node_id.as_deref()?;
        if delivery.visited.len() < 2
            || envelope.kind != crate::mesh::store::Kind::Message
            || envelope.request_key.is_some()
            || envelope.return_binding.recipient_node != local
            || envelope.return_binding.request != envelope.key
            || delivery.visited.first() != Some(&envelope.key.origin_node)
            || crate::mesh::sign::verify(envelope).is_err()
        {
            return None;
        }
        let identity = crate::mesh::identity::NodeIdentity::load().ok()?;
        let detail: String = reason.chars().take(512).collect();
        mint_receipt(
            envelope,
            local,
            &identity,
            serde_json::json!({"state":state,"detail":detail}),
            state,
            envelope.return_binding.collection_token.clone(),
        )
        .ok()
    }

    /// Take custody of the downstream recipient's refusal receipt for
    /// `original` and route it like any forwarded receipt. The origin
    /// authenticates its signer. A receipt for any other message is a
    /// protocol error, reported as transient so the original stays retryable.
    pub(super) fn forward_refusal_receipt(
        &mut self,
        original: &Envelope,
        receipt: &Envelope,
        downstream: &str,
    ) -> Result<(), String> {
        if receipt.kind != crate::mesh::store::Kind::Receipt
            || receipt.request_key.as_ref() != Some(&original.key)
            || receipt.key.origin_node != original.return_binding.recipient_node
            || receipt.return_binding.recipient_node != original.key.origin_node
            || receipt.return_binding.collection_token != original.return_binding.collection_token
            || self.node_id.as_deref() == Some(original.key.origin_node.as_str())
        {
            crate::logging::mesh_custody_failed("refusal_receipt", "receipt_mismatch");
            return Err("receipt_mismatch: refusal receipt is not for this message".into());
        }
        let delivery = Deliver {
            envelope: receipt.clone(),
            remaining_ms: CUSTODY_TTL_MS,
            hops_left: crate::mesh::delivery::hop_limit(),
            visited: vec![downstream.to_owned()],
        };
        self.import_attested_mesh_mail(&delivery, downstream)
            .map(|_| ())
            .inspect_err(|reason| {
                crate::logging::mesh_custody_failed("refusal_receipt", error_code(reason));
            })
    }

    /// Record a terminal refusal of `original`. A hub first takes custody of
    /// the recipient's signed receipt, so a failure or crash never leaves a
    /// refused original whose outcome cannot reach the origin. Without a
    /// receipt, an older recipient or a hub-terminal refusal settles as before.
    pub(super) fn settle_refusal(
        &mut self,
        original: &Envelope,
        reason: &str,
        receipt: Option<&Envelope>,
        downstream: Option<&str>,
    ) -> Result<(), String> {
        if let Some(receipt) =
            receipt.filter(|_| self.node_id.as_deref() != Some(original.key.origin_node.as_str()))
        {
            let downstream = downstream.ok_or("mesh edge is not enrolled")?;
            self.forward_refusal_receipt(original, receipt, downstream)?;
        }
        with_store(|store| {
            store
                .refuse(&original.key, reason, now_ms() as i64)
                .map_err(|e| e.to_string())
        })
    }

    /// Retry durable receipt debt without writing on idle passes.
    pub(super) fn route_mesh_receipts(&mut self) {
        let Ok(receipts) = with_store(|store| {
            store
                .routed_receipts(now_ms() as i64)
                .map_err(|e| e.to_string())
        }) else {
            return;
        };
        for receipt in receipts {
            if let Err(reason) = self.persist_routed_receipt(&receipt) {
                let now = std::time::Instant::now();
                if self
                    .mesh_receipt_log_at
                    .is_none_or(|deadline| now >= deadline)
                {
                    crate::logging::mesh_custody_failed("receipt", error_code(&reason));
                    self.mesh_receipt_log_at = Some(now + std::time::Duration::from_secs(60));
                }
            }
        }
    }

    fn persist_routed_receipt(
        &mut self,
        receipt: &crate::mesh::collect::Receipt,
    ) -> Result<(), String> {
        let origin = self
            .node_id
            .clone()
            .ok_or("mesh node identity unavailable")?;
        let next = self.request_next_hop(&receipt.key.origin_node);
        let identity = crate::mesh::identity::NodeIdentity::load().map_err(|e| e.to_string())?;
        with_store(|store| {
            // Reuse committed custody if a crash interrupted the sent mark.
            for key in store
                .by_request_key(&receipt.key)
                .map_err(|e| e.to_string())?
            {
                if let Some(record) = store
                    .collection_record(&key, now_ms() as i64)
                    .map_err(|e| e.to_string())?
                {
                    if record.envelope.kind == crate::mesh::store::Kind::Receipt
                        && record.envelope.correlation_id
                            == format!("receipt:{}:{}", receipt.key.message_id, receipt.state)
                        && key.origin_node == origin
                    {
                        return store
                            .receipts_sent(std::slice::from_ref(receipt))
                            .map_err(|e| e.to_string());
                    }
                }
            }
            let original = store
                .get(&receipt.key)
                .map_err(|e| e.to_string())?
                .ok_or("message_not_found")?;
            let mail = mint_receipt(
                &original.envelope,
                &origin,
                &identity,
                serde_json::json!({"state":receipt.state}),
                &receipt.state,
                receipt.token.clone(),
            )?;
            store
                .accept_origin(
                    &mail,
                    &next.node,
                    if next.peer.is_some() {
                        Admission::Custody
                    } else {
                        Admission::Held
                    },
                    8,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .receipts_sent(std::slice::from_ref(receipt))
                .map_err(|e| e.to_string())
        })?;
        self.mesh_retry_at = None;
        self.emit_mesh_wake(&next.node);
        Ok(())
    }

    pub(super) fn import_mesh_receipt(
        &mut self,
        mail: &Envelope,
    ) -> Result<(Accepted, bool), String> {
        crate::mesh::sign::verify(mail).map_err(|_| "invalid_signature")?;
        let receipt = decode_receipt(mail)?;
        with_store(|store| {
            if self.node_id.as_deref() != Some(receipt.key.origin_node.as_str())
                || mail.return_binding.request != mail.key
                || mail.return_binding.recipient_node != receipt.key.origin_node
                || mail
                    .request_key
                    .as_ref()
                    .is_some_and(|key| key != &receipt.key)
            {
                return Err("invalid reply binding".into());
            }
            let original = store
                .get(&receipt.key)
                .map_err(|e| e.to_string())?
                .ok_or("receipt_original_not_ready")?;
            if original.envelope.return_binding.recipient_node != mail.key.origin_node
                || original.envelope.return_binding.collection_token != receipt.token
                || mail.request_key.is_some()
                    && (mail.target_agent != original.envelope.sender
                        || mail.return_binding.collection_token != receipt.token)
            {
                return Err("invalid reply binding".into());
            }
            let accepted = store
                .accept(mail, CUSTODY_TTL_MS, Admission::Inbox, now_ms() as i64)
                .map_err(|e| e.to_string())?;
            match store
                .import_receipt_with_detail(
                    &receipt.key,
                    &receipt.state,
                    receipt_detail(mail, &receipt.state).as_deref(),
                )
                .map_err(|e| e.to_string())?
            {
                crate::mesh::store::ReceiptImport::Applied
                | crate::mesh::store::ReceiptImport::Duplicate => Ok((accepted, true)),
                crate::mesh::store::ReceiptImport::OriginalNotReady => {
                    Err("receipt_original_not_ready".into())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn receipt_replay_is_idempotent_and_cannot_rewrite_an_accepted_key() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let signer = crate::mesh::identity::NodeIdentity::fixture([9; 32]);
        let mut original = crate::mesh::sign::tests::signed();
        original.return_binding.recipient_node = signer.node_id();
        original.intent = "\"needs_reply\"".into();
        app.node_id = Some(original.key.origin_node.clone());
        with_store(|store| {
            store
                .accept(
                    &original,
                    CUSTODY_TTL_MS,
                    Admission::Custody,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .finish(
                    &original.key,
                    crate::mesh::store::Outcome::Delivered,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let mut receipt = crate::mesh::collect::Receipt {
            key: original.key.clone(),
            token: original.return_binding.collection_token.clone(),
            state: "delivered".into(),
        };
        let mut mail = original.clone();
        mail.key = MessageKey::mint(signer.node_id(), now_ms()).unwrap();
        mail.kind = crate::mesh::store::Kind::Receipt;
        mail.request_key = Some(original.key.clone());
        mail.return_binding.request = mail.key.clone();
        mail.return_binding.recipient_node = original.key.origin_node.clone();
        mail.target_agent = original.sender.clone();
        mail.body = serde_json::to_vec(&serde_json::json!({"state":receipt.state})).unwrap();
        let forger = crate::mesh::identity::NodeIdentity::fixture([10; 32]);
        let mut forged = mail.clone();
        forged.key = MessageKey::mint(forger.node_id(), now_ms()).unwrap();
        forged.return_binding.request = forged.key.clone();
        forged.body = br#"{"state":"read"}"#.to_vec();
        crate::mesh::sign::seal(&mut forged, &forger);
        crate::mesh::sign::verify(&forged).unwrap();
        assert_eq!(
            app.import_mesh_receipt(&forged).unwrap_err(),
            "invalid reply binding"
        );
        with_store(|store| {
            assert!(store.get(&forged.key).unwrap().is_none());
            assert_eq!(
                store.get(&original.key).unwrap().unwrap().state,
                "delivered"
            );
            Ok(())
        })
        .unwrap();
        crate::mesh::sign::seal(&mut mail, &signer);
        assert_eq!(app.import_mesh_receipt(&mail), Ok((Accepted::New, true)));
        assert_eq!(
            app.import_mesh_receipt(&mail),
            Ok((Accepted::Duplicate, true))
        );
        receipt.state = "read".into();
        mail.body = serde_json::to_vec(&serde_json::json!({"state":receipt.state})).unwrap();
        crate::mesh::sign::seal(&mut mail, &signer);
        assert!(app
            .import_mesh_receipt(&mail)
            .unwrap_err()
            .starts_with("message_key_conflict"));
        with_store(|store| {
            assert!(store.mailbox_keys().unwrap().is_empty());
            assert!(store
                .pending_receipts(&signer.node_id(), now_ms() as i64)
                .unwrap()
                .is_empty());
            assert_eq!(
                store
                    .status(
                        &original.key.origin_node,
                        &original.correlation_id,
                        now_ms() as i64
                    )
                    .unwrap()
                    .unwrap()
                    .state,
                "delivered"
            );
            Ok(())
        })
        .unwrap();
        mail.key = MessageKey::mint(signer.node_id(), now_ms()).unwrap();
        mail.return_binding.request = mail.key.clone();
        crate::mesh::sign::seal(&mut mail, &signer);
        assert_eq!(app.import_mesh_receipt(&mail), Ok((Accepted::New, true)));
        // A separately signed delivered receipt can arrive after the read receipt.
        mail.key = MessageKey::mint(signer.node_id(), now_ms()).unwrap();
        mail.return_binding.request = mail.key.clone();
        mail.body = br#"{"state":"delivered"}"#.to_vec();
        crate::mesh::sign::seal(&mut mail, &signer);
        assert_eq!(app.import_mesh_receipt(&mail), Ok((Accepted::New, true)));
        with_store(|store| {
            assert_eq!(
                store
                    .status(
                        &original.key.origin_node,
                        &original.correlation_id,
                        now_ms() as i64
                    )
                    .unwrap()
                    .unwrap()
                    .state,
                "read"
            );
            Ok(())
        })
        .unwrap();
        with_store(|store| {
            let due = store
                .collect_ready(
                    &original.key.origin_node,
                    now_ms() as i64 + 6000,
                    1,
                    &[],
                    &[signer.node_id()],
                )
                .unwrap();
            assert_eq!(due.len(), 1, "a receipt must not close answer collection");
            assert_eq!(due[0].envelope.key, original.key);
            Ok(())
        })
        .unwrap();
    }

    fn test_app() -> App {
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        )
    }

    fn forwarded(envelope: &Envelope) -> Deliver {
        Deliver {
            visited: vec![envelope.key.origin_node.clone(), "hub.example".into()],
            envelope: envelope.clone(),
            remaining_ms: CUSTODY_TTL_MS,
            hops_left: 7,
        }
    }

    #[tokio::test]
    async fn recipient_signed_refusal_reaches_the_origin_and_a_hub_cannot_mint_one() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut app = test_app();
        let recipient = crate::mesh::identity::NodeIdentity::load().unwrap();
        let sender = crate::mesh::identity::NodeIdentity::fixture([7; 32]);
        let mut original = crate::mesh::sign::tests::signed();
        original.return_binding.recipient_node = recipient.node_id();
        crate::mesh::sign::seal(&mut original, &sender);
        app.node_id = Some(recipient.node_id());
        let mut direct = forwarded(&original);
        direct.visited.truncate(1);
        // A direct sender records its own refusal, and transient failures stay retryable.
        assert!(app.refusal_receipt(&direct, "msg_not_allowed").is_none());
        assert!(app
            .refusal_receipt(&forwarded(&original), "mailbox_full")
            .is_none());
        let receipt = app
            .refusal_receipt(&forwarded(&original), "msg_not_allowed")
            .expect("recipient signs its refusal");
        let gone = app
            .refusal_receipt(&forwarded(&original), "recipient_gone: RecipientGone")
            .unwrap();
        assert_eq!(
            gone.body,
            br#"{"detail":"recipient_gone: RecipientGone","state":"recipient_gone"}"#
        );

        app.node_id = Some(sender.node_id());
        with_store(|store| {
            store
                .accept(
                    &original,
                    CUSTODY_TTL_MS,
                    Admission::Custody,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())?;
            store
                .finish(
                    &original.key,
                    crate::mesh::store::Outcome::Transferred,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let status = || {
            with_store(|store| {
                let status = store
                    .status(&sender.node_id(), &original.correlation_id, now_ms() as i64)
                    .map_err(|e| e.to_string())?
                    .unwrap();
                Ok((status.state, status.detail))
            })
            .unwrap()
        };
        // The forwarding hub re-signs the same refusal under its own key.
        let hub = crate::mesh::identity::NodeIdentity::fixture([10; 32]);
        let mut forged = receipt.clone();
        forged.key = MessageKey::mint(hub.node_id(), now_ms()).unwrap();
        forged.return_binding.request = forged.key.clone();
        crate::mesh::sign::seal(&mut forged, &hub);
        assert_eq!(
            app.import_mesh_receipt(&forged).unwrap_err(),
            "invalid reply binding"
        );
        with_store(|store| {
            assert!(store.get(&forged.key).unwrap().is_none());
            Ok(())
        })
        .unwrap();
        assert_eq!(status(), ("custody".into(), None));

        // The recipient's signature binds its receipt to one original and one reason.
        let mut other = crate::mesh::sign::tests::signed();
        other.correlation_id = "other-thread".into();
        other.return_binding.recipient_node = recipient.node_id();
        crate::mesh::sign::seal(&mut other, &sender);
        with_store(|store| {
            store
                .accept(&other, CUSTODY_TTL_MS, Admission::Custody, now_ms() as i64)
                .map_err(|e| e.to_string())?;
            store
                .finish(
                    &other.key,
                    crate::mesh::store::Outcome::Transferred,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let mut replayed = receipt.clone();
        replayed.request_key = Some(other.key.clone());
        let mut tampered = receipt.clone();
        tampered.body = br#"{"detail":"forged reason","state":"refused"}"#.to_vec();
        for mail in [&replayed, &tampered] {
            assert_eq!(
                app.import_mesh_receipt(mail).unwrap_err(),
                "invalid_signature"
            );
        }
        with_store(|store| {
            assert!(store.get(&receipt.key).unwrap().is_none());
            assert_eq!(
                store
                    .status(&sender.node_id(), "other-thread", now_ms() as i64)
                    .unwrap()
                    .unwrap()
                    .state,
                "custody"
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(status(), ("custody".into(), None));

        assert_eq!(app.import_mesh_receipt(&receipt), Ok((Accepted::New, true)));
        assert_eq!(status(), ("refused".into(), Some("msg_not_allowed".into())));
        assert_eq!(
            app.import_mesh_receipt(&receipt),
            Ok((Accepted::Duplicate, true))
        );
        assert_eq!(app.import_mesh_receipt(&gone), Ok((Accepted::New, true)));
        assert_eq!(status(), ("refused".into(), Some("msg_not_allowed".into())));
    }

    #[tokio::test]
    async fn hub_keeps_the_original_retryable_until_the_refusal_receipt_is_in_custody() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut app = test_app();
        let hub = crate::mesh::identity::NodeIdentity::load().unwrap();
        app.node_id = Some(hub.node_id());
        let recipient = crate::mesh::identity::NodeIdentity::fixture([9; 32]);
        let sender = crate::mesh::identity::NodeIdentity::fixture([7; 32]);
        let custody = |mail: &mut Envelope| {
            mail.return_binding.recipient_node = recipient.node_id();
            crate::mesh::sign::seal(mail, &sender);
            with_store(|store| {
                store
                    .accept_forward(
                        mail,
                        CUSTODY_TTL_MS,
                        7,
                        &[mail.key.origin_node.clone(), hub.node_id()],
                        &recipient.node_id(),
                        Admission::Custody,
                        now_ms() as i64,
                    )
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        };
        let mut original = crate::mesh::sign::tests::signed();
        custody(&mut original);
        let mut other = crate::mesh::sign::tests::signed();
        other.correlation_id = "other-thread".into();
        custody(&mut other);
        let receipt = mint_receipt(
            &original,
            &recipient.node_id(),
            &recipient,
            serde_json::json!({"state":"refused","detail":"msg_not_allowed"}),
            "refused",
            original.return_binding.collection_token.clone(),
        )
        .unwrap();
        let state = |key: &MessageKey| {
            with_store(|store| Ok(store.get(key).unwrap().map(|record| record.state))).unwrap()
        };
        let mut tampered = receipt.clone();
        tampered.body = br#"{"detail":"forged reason","state":"refused"}"#.to_vec();
        // Each failure leaves neither the refusal nor the receipt stored: a
        // receipt for another message, an unverifiable one, or the wrong edge.
        for (mail, receipt, downstream) in [
            (&other, &receipt, recipient.node_id()),
            (&original, &tampered, recipient.node_id()),
            (&original, &receipt, "other.example".to_string()),
        ] {
            assert!(app
                .settle_refusal(mail, "msg_not_allowed", Some(receipt), Some(&downstream))
                .is_err());
            assert_eq!(state(&receipt.key), None);
            assert_eq!(state(&original.key).as_deref(), Some("custody"));
            assert_eq!(state(&other.key).as_deref(), Some("custody"));
        }
        assert_eq!(
            app.settle_refusal(
                &original,
                "msg_not_allowed",
                Some(&receipt),
                Some(&recipient.node_id())
            ),
            Ok(())
        );
        assert_eq!(state(&original.key).as_deref(), Some("refused"));
        with_store(|store| {
            let record = store.get(&receipt.key).unwrap().expect("hub custody");
            assert_eq!(record.state, "custody");
            assert_eq!(record.visited, vec![recipient.node_id(), hub.node_id()]);
            assert_eq!(record.envelope, receipt);
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test]
    async fn held_edge_drops_a_refusal_ack_whose_receipt_cannot_be_kept() {
        let _store = crate::mesh::runtime_store::TestStore::new();
        let mut app = test_app();
        let hub = crate::mesh::identity::NodeIdentity::load().unwrap();
        app.node_id = Some(hub.node_id());
        let recipient = crate::mesh::identity::NodeIdentity::fixture([9; 32]);
        let mut original = crate::mesh::sign::tests::signed();
        original.return_binding.recipient_node = recipient.node_id();
        crate::mesh::sign::seal(
            &mut original,
            &crate::mesh::identity::NodeIdentity::fixture([7; 32]),
        );
        with_store(|store| {
            store
                .accept_forward(
                    &original,
                    CUSTODY_TTL_MS,
                    7,
                    &[original.key.origin_node.clone(), hub.node_id()],
                    &recipient.node_id(),
                    Admission::Held,
                    now_ms() as i64,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let receipt = mint_receipt(
            &original,
            &recipient.node_id(),
            &recipient,
            serde_json::json!({"state":"refused","detail":"msg_not_allowed"}),
            "refused",
            original.return_binding.collection_token.clone(),
        )
        .unwrap();
        let mut tampered = receipt.clone();
        tampered.body = br#"{"detail":"forged reason","state":"refused"}"#.to_vec();
        let ack = |receipt: &Envelope| crate::mesh::collect::OutboundCollect {
            receipts: Vec::new(),
            ack: vec![crate::mesh::collect::OutboundAck {
                key: original.key.clone(),
                token: original.return_binding.collection_token.clone(),
                refusal: Some("msg_not_allowed".into()),
                delivered: false,
                receipt: Some(Box::new(receipt.clone())),
            }],
        };
        let mut outbound = ack(&tampered);
        app.take_refusal_receipts(&mut outbound, &recipient.node_id());
        assert!(
            outbound.ack.is_empty(),
            "the original stays held for reoffer"
        );
        with_store(|store| {
            assert!(store.get(&receipt.key).unwrap().is_none());
            assert_eq!(store.get(&original.key).unwrap().unwrap().state, "held");
            Ok(())
        })
        .unwrap();
        let mut outbound = ack(&receipt);
        app.take_refusal_receipts(&mut outbound, &recipient.node_id());
        assert_eq!(outbound.ack.len(), 1, "the refusal ack proceeds");
        with_store(|store| {
            assert_eq!(store.get(&receipt.key).unwrap().unwrap().state, "custody");
            Ok(())
        })
        .unwrap();
    }
}
