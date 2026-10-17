//! ADR-0028 §5a: bounded response ownership until the final PTY write.
use super::api::responses::{encode_error_with_data, encode_success};
use crate::api::schema::ResponseResult;
use crate::pty::write_receipt::{WriteFailure, WriteReceipt};
use std::{
    collections::BTreeMap,
    sync::{mpsc::Sender, Arc},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

/// One internal ticket per admission, independent of reusable JSON request ids.
#[derive(Default)]
pub(crate) struct InputWrites {
    pub(crate) notify: Arc<Notify>,
    next_ticket: u64,
    staged: Option<u64>,
    pending: BTreeMap<u64, PendingWrite>,
}

struct PendingWrite {
    id: String,
    pane: String,
    deadline: Instant,
    receipt: WriteReceipt,
    respond_to: Option<Sender<String>>,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.receipt.cancel();
    }
}

impl InputWrites {
    pub(super) fn has_capacity(&self, pane: &str) -> bool {
        self.next_ticket < u64::MAX
            && self.pending.len() < 1024
            && self
                .pending
                .values()
                .filter(|write| write.pane == pane)
                .count()
                < 32
    }

    pub(super) fn stage(
        &mut self,
        id: String,
        pane: String,
        deadline: Instant,
        receipt: WriteReceipt,
    ) {
        self.next_ticket += 1;
        receipt.notify_on_change(self.notify.clone());
        self.pending.insert(
            self.next_ticket,
            PendingWrite {
                id,
                pane,
                deadline,
                receipt,
                respond_to: None,
            },
        );
        self.staged = Some(self.next_ticket);
    }

    /// Abandon a dispatch whose caller never attached its response owner.
    pub(super) fn abandon_staged(&mut self) {
        if let Some(ticket) = self.staged.take() {
            self.pending.remove(&ticket);
        }
    }

    pub(super) fn park(&mut self, respond_to: Sender<String>) -> Result<(), Sender<String>> {
        let Some(ticket) = self.staged.take() else {
            return Err(respond_to);
        };
        if let Some(write) = self.pending.get_mut(&ticket) {
            write.respond_to = Some(respond_to);
            self.service();
            Ok(())
        } else {
            Err(respond_to)
        }
    }

    /// Called on receipt notification or the original deadline, never a polling timer.
    pub(crate) fn service(&mut self) {
        self.pending.retain(|_, write| {
            if write.respond_to.is_none() {
                return true;
            }
            let Some(response) = completed_reply(&write.id, &write.receipt) else {
                return true;
            };
            if let Some(respond_to) = write.respond_to.take() {
                let _ = respond_to.send(response);
            }
            false
        });
    }

    /// Resolve every saved caller before handing off or leaving the app loop.
    pub(crate) fn retire_all(&mut self, reason: WriteFailure) {
        for write in self.pending.values() {
            write.receipt.retire(reason);
        }
        self.service();
        self.abandon_staged();
    }

    pub(crate) fn service_due(&mut self, now: Instant) {
        if self.next_deadline().is_some_and(|deadline| now >= deadline) {
            self.service();
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        // Equality remains timely; wake at the first representable expired instant.
        self.pending
            .values()
            .map(|write| write.deadline + Duration::from_nanos(1))
            .min()
    }
}

/// Optional JSON detail is validated here so malformed values remain invalid_params.
pub(super) fn deadline(timeout: Option<&serde_json::Value>) -> Option<Instant> {
    let millis = match timeout {
        None => 10000,
        Some(value) => value.as_u64()?,
    };
    (1..=30000)
        .contains(&millis)
        .then(|| Instant::now() + Duration::from_millis(millis))
}

pub(super) fn failure(id: String, written: usize, total: usize, reason: &str) -> String {
    encode_error_with_data(
        id,
        if written == 0 {
            "input_write_cancelled"
        } else {
            "input_write_failed"
        },
        "PTY input did not complete; inspect byte evidence before retrying",
        serde_json::json!({"input_write": {"bytes_written": written, "bytes_total": total,
            "reason": reason, "remainder_discarded": true}}),
    )
}

pub(super) fn completed_reply(id: &str, receipt: &WriteReceipt) -> Option<String> {
    let progress = receipt.progress();
    if let Some(reason) = progress.failure {
        let reason = match reason {
            WriteFailure::Cancelled => "cancelled",
            WriteFailure::Deadline => "deadline",
            WriteFailure::WriterBroken => "writer_broken",
            WriteFailure::OwnerChanged => "owner_changed",
            WriteFailure::PaneGone => "pane_gone",
            WriteFailure::WriteFailed => "write_failed",
        };
        Some(failure(
            id.into(),
            progress.bytes_written,
            progress.bytes_total,
            reason,
        ))
    } else {
        progress
            .completed_at
            .map(|_| encode_success(id.into(), ResponseResult::Ok {}))
    }
}

impl super::App {
    pub(super) fn begin_input_write(
        &mut self,
        id: String,
        ws: usize,
        pane: crate::layout::PaneId,
        bytes: bytes::Bytes,
        deadline: Instant,
        operator_interrupt: bool,
    ) -> String {
        let Some(terminal) = self.state.terminal_id_for_pane(ws, pane) else {
            return super::api::responses::encode_error(
                id,
                "pane_not_found",
                "pane no longer exists",
            );
        };
        if bytes.is_empty() {
            return if Instant::now() <= deadline {
                encode_success(id, ResponseResult::Ok {})
            } else {
                failure(id, 0, 0, "deadline")
            };
        }
        self.input_writes.service();
        let key = terminal.to_string();
        if !self.input_writes.has_capacity(&key) {
            return failure(id, 0, bytes.len(), "admission_limit");
        }
        let Some(runtime) = self.lookup_runtime_sender(ws, pane) else {
            return super::api::responses::encode_error(
                id,
                "pane_not_found",
                "pane runtime no longer exists",
            );
        };
        let receipt = runtime.try_send_tracked(bytes, deadline, operator_interrupt);
        if let Some(response) = completed_reply(&id, &receipt) {
            return response;
        }
        self.input_writes.stage(id, key, deadline, receipt);
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    fn register(
        registry: &mut InputWrites,
        pane: &str,
        until: Instant,
    ) -> (
        crate::pty::write_receipt::WriteCompletion,
        WriteReceipt,
        mpsc::Receiver<String>,
    ) {
        let (ticket, receipt) = WriteReceipt::channel(Bytes::from_static(b"paste"), Some(until));
        let (sender, response) = mpsc::channel();
        registry.stage("same".into(), pane.into(), until, receipt.clone());
        registry.park(sender).unwrap();
        (ticket, receipt, response)
    }

    #[tokio::test]
    async fn s3b_registration_covers_completion_on_both_sides_of_registration() {
        for complete_before_registration in [true, false] {
            let mut registry = InputWrites::default();
            let until = Instant::now() + Duration::from_secs(2);
            let (ticket, receipt) =
                WriteReceipt::channel(Bytes::from_static(b"native"), Some(until));
            let (mut writer, mut peer) = UnixStream::pair().unwrap();
            let (sender, response) = mpsc::channel();
            if complete_before_registration {
                ticket.write(|bytes| writer.write(bytes)).unwrap().unwrap();
            }
            registry.stage("same".into(), "pane".into(), until, receipt.clone());
            registry.park(sender).unwrap();
            if !complete_before_registration {
                tokio::time::timeout(Duration::from_secs(2), registry.notify.notified())
                    .await
                    .unwrap();
                assert!(response.try_recv().is_err());
                ticket.write(|bytes| writer.write(bytes)).unwrap().unwrap();
                tokio::time::timeout(Duration::from_secs(2), registry.notify.notified())
                    .await
                    .unwrap();
                registry.service();
            }
            let mut native = [0; 6];
            peer.read_exact(&mut native).unwrap();
            assert_eq!(&native, b"native");
            assert!(
                serde_json::from_str::<serde_json::Value>(&response.try_recv().unwrap())
                    .unwrap()
                    .get("result")
                    .is_some()
            );
            registry.service();
            assert!(response.try_recv().is_err());
            assert!(registry.pending.is_empty());
            ticket.retire(WriteFailure::WriterBroken);
            assert_eq!(receipt.progress().failure, None);
        }
    }

    #[tokio::test]
    async fn s3b_silent_and_partial_producers_expire_without_other_traffic() {
        for prefix in [0, 2] {
            let mut registry = InputWrites::default();
            let (ticket, receipt, response) = register(
                &mut registry,
                "pane",
                Instant::now() + Duration::from_millis(200),
            );
            let (mut writer, mut peer) = UnixStream::pair().unwrap();
            if prefix > 0 {
                ticket
                    .write(|bytes| writer.write(&bytes[..prefix]))
                    .unwrap()
                    .unwrap();
                let mut observed = [0; 2];
                peer.read_exact(&mut observed).unwrap();
                assert_eq!(&observed, b"pa");
            }
            assert!(response.try_recv().is_err());
            tokio::time::timeout(
                Duration::from_secs(2),
                super::super::sleep_until_or_pending(registry.next_deadline()),
            )
            .await
            .unwrap();
            registry.service();
            let value: serde_json::Value =
                serde_json::from_str(&response.try_recv().unwrap()).unwrap();
            assert_eq!(
                value["error"]["code"],
                if prefix == 0 {
                    "input_write_cancelled"
                } else {
                    "input_write_failed"
                }
            );
            assert_eq!(
                value["error"]["data"]["input_write"]["bytes_written"],
                prefix
            );
            assert_eq!(value["error"]["data"]["input_write"]["reason"], "deadline");
            assert_eq!(receipt.progress().bytes_written, prefix);
            assert!(receipt.progress().remainder_discarded);
            assert!(ticket
                .write(|_| panic!("expired bytes must never reach a syscall"))
                .is_none());
            assert!(registry.has_capacity("pane"));
        }
    }

    #[test]
    fn s3b_native_prefix_failure_and_competing_cancel_preserve_exact_reply() {
        for reason in [
            WriteFailure::WriterBroken,
            WriteFailure::OwnerChanged,
            WriteFailure::PaneGone,
            WriteFailure::Cancelled,
        ] {
            for prefix in [0, 2] {
                let mut registry = InputWrites::default();
                let (ticket, receipt, response) = register(
                    &mut registry,
                    "pane",
                    Instant::now() + Duration::from_secs(2),
                );
                let (mut writer, mut peer) = UnixStream::pair().unwrap();
                if prefix > 0 {
                    ticket
                        .write(|bytes| writer.write(&bytes[..prefix]))
                        .unwrap()
                        .unwrap();
                    let mut native = [0; 2];
                    peer.read_exact(&mut native).unwrap();
                    assert_eq!(&native, b"pa");
                }
                ticket.retire(reason);
                registry.service();
                let value: serde_json::Value =
                    serde_json::from_str(&response.try_recv().unwrap()).unwrap();
                assert_eq!(
                    value["error"]["code"],
                    if prefix == 0 {
                        "input_write_cancelled"
                    } else {
                        "input_write_failed"
                    }
                );
                assert_eq!(
                    value["error"]["data"]["input_write"]["bytes_written"],
                    prefix
                );
                assert_eq!(value["error"]["data"]["input_write"]["bytes_total"], 5);
                assert_eq!(
                    value["error"]["data"]["input_write"]["remainder_discarded"],
                    true
                );
                assert_eq!(receipt.progress().failure, Some(reason));
                assert!(ticket
                    .write(|_| panic!("retired bytes must never reach a syscall"))
                    .is_none());
                registry.service();
                assert!(response.try_recv().is_err());
            }
        }
    }

    #[test]
    fn s3b_duplicate_ids_and_both_quota_limits_release_on_retirement() {
        let mut registry = InputWrites::default();
        let until = Instant::now() + Duration::from_secs(10);
        let mut writes = Vec::new();
        for pane in 0..32 {
            for _ in 0..32 {
                assert!(registry.has_capacity(&pane.to_string()));
                writes.push(register(&mut registry, &pane.to_string(), until));
            }
            assert!(!registry.has_capacity(&pane.to_string()));
        }
        assert_eq!(registry.pending.len(), 1024);
        assert!(!registry.has_capacity("other"));
        for (ticket, _, _) in &writes[..32] {
            ticket.retire(WriteFailure::WriterBroken);
        }
        registry.service();
        assert!(registry.has_capacity("0"));
        assert!(registry.has_capacity("other"));
        assert!(!registry.has_capacity("1"));
        for (_, _, response) in &writes[..32] {
            assert!(response.try_recv().is_ok());
        }
        for (_, _, response) in &writes[32..] {
            assert!(response.try_recv().is_err());
        }
        drop(registry);
        for (ticket, receipt, _) in &writes {
            assert!(receipt.progress().remainder_discarded);
            assert!(ticket
                .write(|_| panic!("registry shutdown must cancel the remaining payload"))
                .is_none());
        }
    }

    #[test]
    fn s3b_exit_retirement_answers_partial_completed_and_untouched_tickets_once() {
        for reason in [WriteFailure::OwnerChanged, WriteFailure::PaneGone] {
            let mut registry = InputWrites::default();
            let until = Instant::now() + Duration::from_secs(2);
            let (partial, _, partial_reply) = register(&mut registry, "pane", until);
            let (complete, complete_receipt, complete_reply) =
                register(&mut registry, "pane", until);
            let (untouched, _, untouched_reply) = register(&mut registry, "pane", until);
            let (mut writer, mut peer) = UnixStream::pair().unwrap();
            partial
                .write(|bytes| writer.write(&bytes[..2]))
                .unwrap()
                .unwrap();
            complete
                .write(|bytes| writer.write(bytes))
                .unwrap()
                .unwrap();
            let mut native = [0; 7];
            peer.read_exact(&mut native).unwrap();
            assert_eq!(&native, b"papaste");
            registry.retire_all(reason);
            for (reply, count) in [(partial_reply, 2), (untouched_reply, 0)] {
                let value: serde_json::Value =
                    serde_json::from_str(&reply.try_recv().unwrap()).unwrap();
                assert_eq!(
                    value["error"]["data"]["input_write"]["bytes_written"],
                    count
                );
                assert_eq!(
                    value["error"]["data"]["input_write"]["reason"],
                    if reason == WriteFailure::OwnerChanged {
                        "owner_changed"
                    } else {
                        "pane_gone"
                    }
                );
                assert!(reply.try_recv().is_err());
            }
            let value: serde_json::Value =
                serde_json::from_str(&complete_reply.try_recv().unwrap()).unwrap();
            assert!(value.get("result").is_some());
            assert_eq!(complete_receipt.progress().failure, None);
            assert!(registry.pending.is_empty());
            for ticket in [partial, untouched] {
                assert!(ticket
                    .write(|_| panic!("exit must retire all outstanding bytes"))
                    .is_none());
            }
        }
    }

    #[test]
    fn s3b_abandoned_dispatch_cancels_before_releasing_its_quota() {
        let mut registry = InputWrites::default();
        let until = Instant::now() + Duration::from_secs(2);
        let (ticket, receipt) =
            WriteReceipt::channel(Bytes::from_static(b"abandoned"), Some(until));
        registry.stage("same".into(), "pane".into(), until, receipt.clone());
        registry.abandon_staged();
        assert!(registry.pending.is_empty());
        assert_eq!(receipt.progress().failure, Some(WriteFailure::Cancelled));
        assert!(ticket
            .write(|_| panic!("abandoned input must not execute"))
            .is_none());
    }
}
