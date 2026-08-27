//! Handed-over files, as durable records (#286, ADR-0017).
//!
//! A file dropped on a pane is staged server-side and its path is pasted into
//! the agent's input surface. That path was the whole record: it lived in one
//! client connection's `Vec<PathBuf>`, was deleted when that connection went
//! away, and nothing could enumerate it. An agent could read the file exactly
//! once, in the turn the paste arrived, and only because the bytes had gone
//! through its terminal.
//!
//! This module is the read model that replaces that. It is *derived*, not
//! authoritative: the durable record is `EventKind::FileHandedOver` on the
//! event log (ADR-0005), and everything here is a fold over that one kind,
//! rebuilt at boot by [`HandoffLog::seed_from_events`] exactly as
//! `NotificationLog` and `MailboxRegistry` are.
//!
//! # What is durable and what is not
//!
//! The record is. The bytes are not *in* it — they stay in the staging
//! directory, and the filesystem owns them. So the seed reconciles: a record
//! whose file is gone (a reboot cleared the OS temp dir, someone deleted it)
//! is dropped rather than listed as a resource that cannot be read. That
//! asymmetry is the point of keeping base64 out of a JSONL audit log.
//!
//! # Retention
//!
//! [`MAX_HANDOFFS`] records, oldest evicted first, and eviction deletes the
//! staged file with the record — the two must not outlive each other. The
//! staging directory's own age sweep is the other half; between them a
//! handoff is bounded by both count and age.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::api::schema::{EventData, EventEnvelope};

/// How many handed-over files the projection keeps. Deliberately far below
/// `MAX_NOTIFICATIONS`: a notification is a line of prose, a handoff pins a
/// file on disk that only eviction frees.
pub(crate) const MAX_HANDOFFS: usize = 128;

/// One handed-over file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffEntry {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    pub path: PathBuf,
    /// Who it was handed to, when that was resolvable at the time. A drop
    /// onto a pane with no agent still files a record — the file arrived,
    /// and an unattributed record beats a lost one.
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent_id: Option<String>,
    pub origin_host: String,
    pub received_at_ms: u64,
}

/// Oldest at the front, newest at the back.
#[derive(Debug, Default)]
pub(crate) struct HandoffLog {
    entries: VecDeque<HandoffEntry>,
}

impl HandoffLog {
    /// Rebuild from the durable stream, keeping only records whose staged
    /// file still exists. `exists` is injected so the reconciliation is
    /// testable without touching a filesystem.
    pub(crate) fn seed_from_events<'a>(
        &mut self,
        events: impl Iterator<Item = &'a EventEnvelope>,
        exists: &dyn Fn(&Path) -> bool,
    ) {
        let mut entries: VecDeque<HandoffEntry> = VecDeque::new();
        for envelope in events {
            let EventData::FileHandedOver {
                file_id,
                name,
                mime,
                bytes,
                path,
                workspace_id,
                pane_id,
                agent_id,
                origin_host,
                received_at_ms,
            } = &envelope.data
            else {
                continue;
            };
            let path = PathBuf::from(path);
            if !exists(&path) {
                continue;
            }
            entries.push_back(HandoffEntry {
                id: file_id.clone(),
                name: name.clone(),
                mime: mime.clone(),
                bytes: *bytes,
                path,
                workspace_id: workspace_id.clone(),
                pane_id: pane_id.clone(),
                agent_id: agent_id.clone(),
                origin_host: origin_host.clone(),
                received_at_ms: *received_at_ms,
            });
        }
        self.entries = entries;
        // Trimming here evicts a record without deleting its file. That is
        // correct: this runs at boot, and the file it would delete is one the
        // staging sweep is already responsible for.
        while self.entries.len() > MAX_HANDOFFS {
            self.entries.pop_front();
        }
    }

    /// File one handoff. Returns the entries the cap pushed out, whose staged
    /// files the caller must delete.
    pub(crate) fn record(&mut self, entry: HandoffEntry) -> Vec<HandoffEntry> {
        self.entries.push_back(entry);
        let mut evicted = Vec::new();
        while self.entries.len() > MAX_HANDOFFS {
            if let Some(entry) = self.entries.pop_front() {
                evicted.push(entry);
            }
        }
        evicted
    }

    pub(crate) fn newest_first(&self) -> impl Iterator<Item = &HandoffEntry> {
        self.entries.iter().rev()
    }

    pub(crate) fn get(&self, file_id: &str) -> Option<&HandoffEntry> {
        self.entries.iter().find(|entry| entry.id == file_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Recording, kept beside the projection so the read model and the durable
/// event it is derived from can never drift: the mutation queues the event
/// that would rebuild it.
impl crate::app::state::AppState {
    pub(crate) fn record_handoff(&mut self, entry: HandoffEntry) {
        self.pending_ui_events
            .push(crate::app::state::PendingUiEvent::FileHandedOver {
                file_id: entry.id.clone(),
                name: entry.name.clone(),
                mime: entry.mime.clone(),
                bytes: entry.bytes,
                path: entry.path.to_string_lossy().into_owned(),
                workspace_id: entry.workspace_id.clone(),
                pane_id: entry.pane_id.clone(),
                agent_id: entry.agent_id.clone(),
                origin_host: entry.origin_host.clone(),
                received_at_ms: entry.received_at_ms,
            });
        let evicted = self.handoffs.record(entry);
        for entry in &evicted {
            crate::logging::handoff_evicted(&entry.id, &entry.path.to_string_lossy());
        }
        crate::server::clipboard_image::remove_files(
            evicted.into_iter().map(|entry| entry.path).collect(),
        );
    }
}

/// The producer side: turn a staged file into a durable record addressed to
/// whoever the paste is about to land on.
///
/// Resolving the recipient HERE is deliberate. The drop knows where it is
/// going — a terminal-attached client names its terminal, and a full client's
/// paste routes to the focused pane. Nothing downstream knows that, and by the
/// time the record is read the focus has moved on.
impl crate::app::App {
    pub(crate) fn record_file_handoff(
        &mut self,
        terminal_id: Option<&str>,
        path: std::path::PathBuf,
        extension: &str,
        bytes: u64,
    ) -> String {
        let resolved = match terminal_id {
            Some(terminal_id) => self.resolve_terminal_target(terminal_id).ok(),
            None => self.state.active.and_then(|ws_idx| {
                self.state
                    .workspaces
                    .get(ws_idx)
                    .and_then(|ws| ws.focused_pane_id())
                    .and_then(|pane_id| self.terminal_target_for_pane(ws_idx, pane_id))
            }),
        };
        let (workspace_id, pane_id, agent_id) = match &resolved {
            Some(resolved) => (
                Some(self.public_workspace_id(resolved.ws_idx)),
                self.public_pane_id(resolved.ws_idx, resolved.pane_id),
                self.state
                    .terminals
                    .iter()
                    .find(|(id, _)| id.to_string() == resolved.terminal_id)
                    .map(|(_, terminal)| terminal.agent_id.to_string()),
            ),
            // A drop with no resolvable pane is still a file that arrived.
            // Filing it unattributed beats losing it.
            None => (None, None, None),
        };

        let entry = HandoffEntry {
            id: mint_handoff_id(),
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("handoff.{extension}")),
            mime: mime_for_extension(extension).to_string(),
            bytes,
            path,
            workspace_id,
            pane_id,
            agent_id,
            origin_host: crate::app::short_host_name(),
            received_at_ms: crate::app::notifications::now_ms(),
        };
        let file_id = entry.id.clone();
        self.state.record_handoff(entry);
        file_id
    }
}

/// Mints a handoff id. Host-local, like `mint_notification_id`: the bytes are
/// on the node that staged them, so the identity does not need to be global.
pub(crate) fn mint_handoff_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "file:{:x}:{:x}",
        crate::app::notifications::now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A media type for a staged extension.
///
/// Small and closed on purpose. The extension has already been through
/// `sanitize_extension`, so this maps a known token to a known type and calls
/// everything else a byte stream — guessing wrong is worse than saying
/// `application/octet-stream`, which every MCP client already handles.
pub(crate) fn mime_for_extension(extension: &str) -> &'static str {
    match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "text/xml",
        "md" | "markdown" => "text/markdown",
        "toml" => "text/plain",
        "yaml" | "yml" => "text/plain",
        "txt" | "log" | "rs" | "py" | "js" | "ts" | "sh" | "diff" | "patch" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventEnvelope, EventKind};

    fn envelope(file_id: &str, path: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::FileHandedOver,
            data: EventData::FileHandedOver {
                file_id: file_id.to_string(),
                name: format!("{file_id}.md"),
                mime: "text/markdown".to_string(),
                bytes: 4,
                path: path.to_string(),
                workspace_id: Some("ws_1".to_string()),
                pane_id: Some("ws_1:p1".to_string()),
                agent_id: Some("agent_host_abc".to_string()),
                origin_host: "host".to_string(),
                received_at_ms: 1,
            },
        }
    }

    fn entry(id: &str) -> HandoffEntry {
        HandoffEntry {
            id: id.to_string(),
            name: format!("{id}.md"),
            mime: "text/markdown".to_string(),
            bytes: 4,
            path: PathBuf::from(format!("/nowhere/{id}.md")),
            workspace_id: None,
            pane_id: None,
            agent_id: None,
            origin_host: "host".to_string(),
            received_at_ms: 1,
        }
    }

    #[test]
    fn a_handoff_rebuilds_itself_from_the_durable_log() {
        let events = [
            envelope("file:a", "/staged/a.md"),
            envelope("file:b", "/staged/b.md"),
        ];
        let mut log = HandoffLog::default();
        log.seed_from_events(events.iter(), &|_| true);
        assert_eq!(log.len(), 2);
        assert_eq!(
            log.newest_first()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["file:b", "file:a"]
        );
        assert_eq!(
            log.get("file:a").map(|e| e.mime.as_str()),
            Some("text/markdown")
        );
    }

    #[test]
    fn a_record_whose_bytes_are_gone_is_not_listed() {
        // The log outlives the staging directory — a reboot clears /tmp. A
        // resource that cannot be read must not be offered.
        let events = [
            envelope("file:kept", "/staged/kept.md"),
            envelope("file:gone", "/staged/gone.md"),
        ];
        let mut log = HandoffLog::default();
        log.seed_from_events(events.iter(), &|path| path.ends_with("kept.md"));
        assert_eq!(log.len(), 1);
        assert_eq!(
            log.newest_first().next().map(|e| e.id.as_str()),
            Some("file:kept")
        );
    }

    #[test]
    fn the_cap_evicts_the_oldest_and_names_its_file_for_deletion() {
        let mut log = HandoffLog::default();
        for index in 0..MAX_HANDOFFS {
            assert!(log.record(entry(&format!("file:{index}"))).is_empty());
        }
        let evicted = log.record(entry("file:last"));
        assert_eq!(evicted.len(), 1, "one over the cap evicts exactly one");
        assert_eq!(evicted[0].id, "file:0", "oldest first");
        assert_eq!(
            evicted[0].path,
            PathBuf::from("/nowhere/file:0.md"),
            "the caller needs the path to delete the bytes with the record"
        );
        assert_eq!(log.len(), MAX_HANDOFFS);
    }

    #[test]
    fn the_record_survives_the_json_the_log_stores_it_as() {
        let encoded = serde_json::to_string(&envelope("file:a", "/staged/a.md")).unwrap();
        let decoded: EventEnvelope = serde_json::from_str(&encoded).unwrap();
        let mut log = HandoffLog::default();
        log.seed_from_events([decoded].iter(), &|_| true);
        assert_eq!(log.get("file:a").map(|e| e.bytes), Some(4));
        assert_eq!(
            log.get("file:a").and_then(|e| e.agent_id.clone()),
            Some("agent_host_abc".to_string())
        );
    }

    #[test]
    fn unknown_extensions_are_a_byte_stream_rather_than_a_guess() {
        assert_eq!(mime_for_extension("pdf"), "application/pdf");
        assert_eq!(mime_for_extension("md"), "text/markdown");
        assert_eq!(mime_for_extension("bin"), "application/octet-stream");
        assert_eq!(mime_for_extension("wat"), "application/octet-stream");
    }

    #[test]
    fn ids_are_unique_within_a_process() {
        assert_ne!(mint_handoff_id(), mint_handoff_id());
    }
}
