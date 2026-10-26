//! Internal inbox projection seam. Public caller binding belongs to #2452.
use crate::{
    api::schema::{EventData, EventEnvelope, EventKind},
    app::App,
};
impl App {
    /// Sweep only the independent tool-read window, never queue delivery/custody.
    pub(super) fn sweep_native_inboxes(&mut self) {
        match crate::mesh::hello::with_store(|s| {
            s.native_inbox(None, false, super::messages::now_ms() as i64)
                .map_err(|e| e.to_string())
        }) {
            Ok((_, expired)) => {
                for envelope in expired {
                    self.emit_event(EventEnvelope {
                        event: EventKind::MessageDelivered,
                        data: EventData::MessageDelivered {
                            correlation_id: envelope.correlation_id,
                            delivered: false,
                            outcome: "dropped_undeliverable".into(),
                            delivery_attempts: 0,
                            latency_ms: 0,
                        },
                    });
                }
            }
            Err(_) => {
                crate::logging::mesh_custody_failed("native-inbox-sweep", "mail_store_unavailable")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_inbox_sweep_emits_each_expiry_once() {
        let (_env, _scrub) = crate::config::test_config_env_guard();
        let _store = crate::mesh::runtime_store::TestStore::new();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        crate::mesh::hello::with_store(|s| {
            let e = crate::mesh::sign::tests::signed();
            let now = super::super::messages::now_ms() as i64;
            s.accept_local(
                &e,
                crate::mesh::store::CUSTODY_TTL_MS,
                "recipient",
                "session",
                now,
            )
            .unwrap();
            s.settle_queue_admission(&e.key, None, now).unwrap();
            s.native_inbox_delivered(&e.key, 0, now).unwrap();
            Ok(())
        })
        .unwrap();
        for _ in 0..2 {
            app.sweep_native_inboxes();
        }
        let events = app.event_hub.ring_snapshot_after(0);
        assert_eq!(events.iter().filter(|(_, e)| matches!(&e.data,
            EventData::MessageDelivered { delivered: false, outcome, .. } if outcome == "dropped_undeliverable")).count(), 1);
    }
}
