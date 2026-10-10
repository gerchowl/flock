//! Signed outcomes return through the same custody routes as answers.
use super::{mesh_mail::error_code, messages::now_ms};
use crate::{
    app::App,
    mesh::{
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

impl App {
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
            let mut mail = original.envelope;
            mail.key = MessageKey::mint(origin.clone(), now_ms()).map_err(|e| e.to_string())?;
            mail.kind = crate::mesh::store::Kind::Receipt;
            mail.request_key = Some(receipt.key.clone());
            mail.target_agent = mail.sender.clone();
            mail.correlation_id = format!("receipt:{}:{}", receipt.key.message_id, receipt.state);
            mail.in_reply_to = None;
            mail.intent = "\"fyi\"".into();
            mail.return_binding =
                ReturnBinding::mint(mail.key.clone(), receipt.key.origin_node.clone(), vec![])
                    .map_err(|e| e.to_string())?;
            mail.return_binding.collection_token = receipt.token.clone();
            mail.body = serde_json::to_vec(&serde_json::json!({"state":receipt.state}))
                .map_err(|e| e.to_string())?;
            crate::mesh::sign::seal(&mut mail, &identity);
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
                .import_receipt(&receipt.key, &receipt.state)
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
}
