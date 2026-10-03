//! The operator's reading surface for the notification log (#516).
//!
//! ADR-0016 §6 promised "a place to read them in the TUI" and #381 deferred
//! it without filing; this is that surface. It is a **reader**: it renders what
//! [`crate::app::notifications::NotificationLog`] already holds and
//! acknowledges through the same
//! [`crate::app::state::AppState::acknowledge_notification`] seam the socket
//! API uses, so there is no second counter and no second write path.
//!
//! ## What it deliberately does not do
//!
//! It changes no retention. `trim()` evicts read records first and then the
//! oldest **unread**, and an eviction emits nothing into the log, so an
//! unanswered record that was evicted is indistinguishable from one that was
//! never filed. That is ADR-0016 §3's premise and it is not settled
//! (ADR-0022 §3), so this module fixes none of it — it *reports* it instead:
//! [`NotificationLog::at_cap`](crate::app::notifications::NotificationLog::at_cap)
//! and the age of the oldest surviving record drive a standing footer line, so
//! the panel never implies it is showing the whole history.

use crate::app::notifications::NotificationEntry;
use crate::app::state::AppState;

/// What the panel's buttons and keys do. One enum for both, so a click and a
/// keypress reach the same mutation by construction rather than by agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationPanelAction {
    /// Acknowledge the selected record — `notification.ack { id }`.
    Acknowledge,
    /// Acknowledge everything unread — `notification.ack { all: true }`.
    AcknowledgeAll,
    Close,
}

/// Panel-only state. Every field is a view concern: nothing here is part of
/// the log, and dropping the panel loses nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationPanelState {
    /// Mirrors `flk notification list --unread`: the same predicate over the
    /// same projection, read through
    /// [`NotificationLog::filtered`](crate::app::notifications::NotificationLog::filtered).
    pub unread_only: bool,
    /// Index into [`AppState::notification_panel_rows`].
    pub selected: usize,
    pub scroll: usize,
}

/// The panel's own view of the log.
///
/// Reading this is deliberately *derived*: there is no cached copy that a
/// filed, acknowledged or evicted record could leave stale, so the panel cannot
/// show a record the log does not hold.
impl AppState {
    /// The rows the panel lists, newest first — the same filter
    /// `flk notification list --unread` and `notification.list` apply, read
    /// through [`crate::app::notifications::NotificationLog::filtered`].
    pub(crate) fn notification_panel_rows(&self) -> Vec<NotificationEntry> {
        self.notifications
            .filtered(self.notifications_panel.unread_only)
            .cloned()
            .collect()
    }

    /// The row count on its own, for the cursor maths that only needs a
    /// length — mouse moves hit this on every hover event.
    pub(crate) fn notification_panel_row_count(&self) -> usize {
        self.notifications
            .filtered(self.notifications_panel.unread_only)
            .count()
    }

    pub(crate) fn open_notification_panel(&mut self) {
        self.notifications_panel = NotificationPanelState::default();
        self.clamp_notification_panel_selection();
        self.mode = crate::app::state::Mode::Notifications;
    }

    /// Move the cursor, keeping it inside the current filter's rows.
    pub(crate) fn move_notification_panel_selection(&mut self, delta: isize) {
        let count = self.notification_panel_row_count();
        if count == 0 {
            self.notifications_panel.selected = 0;
            self.notifications_panel.scroll = 0;
            return;
        }
        let current = self.notifications_panel.selected.min(count - 1) as isize;
        let next = (current + delta).clamp(0, count as isize - 1);
        self.notifications_panel.selected = next as usize;
        self.ensure_notification_panel_selection_visible();
    }

    pub(crate) fn select_notification_panel_row(&mut self, index: usize) {
        let count = self.notification_panel_row_count();
        if index < count {
            self.notifications_panel.selected = index;
        }
        self.ensure_notification_panel_selection_visible();
    }

    /// The filter can shrink the list under the cursor — acknowledging a row
    /// while `unread_only` is on drops it out of the view — so the cursor is
    /// re-seated on every mutation rather than left pointing past the end.
    pub(crate) fn clamp_notification_panel_selection(&mut self) {
        let count = self.notification_panel_row_count();
        if count == 0 {
            self.notifications_panel.selected = 0;
        } else if self.notifications_panel.selected >= count {
            self.notifications_panel.selected = count - 1;
        }
        self.ensure_notification_panel_selection_visible();
    }

    /// Toggle the `--unread` filter, keeping the cursor on the same *record*
    /// where it still exists so the view does not jump under the operator.
    pub(crate) fn toggle_notification_panel_unread_only(&mut self) {
        let anchored = self
            .notification_panel_rows()
            .get(self.notifications_panel.selected)
            .map(|entry| entry.id.clone());
        self.notifications_panel.unread_only = !self.notifications_panel.unread_only;
        if let Some(id) = anchored {
            if let Some(index) = self
                .notifications
                .filtered(self.notifications_panel.unread_only)
                .position(|entry| entry.id == id)
            {
                self.notifications_panel.selected = index;
            }
        }
        self.clamp_notification_panel_selection();
    }

    /// The row the primary action applies to.
    pub(crate) fn selected_notification_panel_entry(&self) -> Option<NotificationEntry> {
        self.notification_panel_rows()
            .get(self.notifications_panel.selected)
            .cloned()
    }

    /// Acknowledge the selected record through the existing seam (#516), so
    /// the TUI writes the same `NotificationSeen` the socket API writes.
    pub(crate) fn acknowledge_selected_notification(&mut self) -> bool {
        let Some(entry) = self.selected_notification_panel_entry() else {
            return false;
        };
        if !self.acknowledge_notification(&entry.id) {
            return false;
        }
        self.clamp_notification_panel_selection();
        true
    }

    /// Acknowledge everything unread — `notification.ack { all: true }`, same
    /// seam, same events.
    pub(crate) fn acknowledge_all_notifications_from_panel(&mut self) -> usize {
        let count = self.acknowledge_all_notifications();
        self.clamp_notification_panel_selection();
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{NotificationRecordKind, NotificationSource};
    use crate::app::state::Mode;

    fn entry(id: &str, seen: bool) -> NotificationEntry {
        NotificationEntry {
            id: id.to_string(),
            title: format!("{id} finished"),
            body: None,
            kind: NotificationRecordKind::Outcome,
            source: NotificationSource::AgentState,
            workspace_id: None,
            pane_id: None,
            origin_host: "host.invalid".into(),
            filed_at_ms: 0,
            seen,
        }
    }

    /// The rows come from `NotificationLog::list`, which is the same reader
    /// `flk notification list` and `notification.list` use — so this test
    /// pins the panel to the shipped semantics rather than a parallel set of
    /// them.
    #[test]
    fn the_panel_lists_the_logs_own_projection_newest_first() {
        let mut state = AppState::test_new();
        state.file_notification(entry("oldest", false));
        state.file_notification(entry("middle", true));
        state.file_notification(entry("newest", false));
        state.open_notification_panel();

        assert_eq!(state.mode, Mode::Notifications);
        let rows = state.notification_panel_rows();
        let ids: Vec<&str> = rows.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec!["newest", "middle", "oldest"]);

        state.toggle_notification_panel_unread_only();
        let rows = state.notification_panel_rows();
        let unread: Vec<&str> = rows.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(unread, vec!["newest", "oldest"]);
    }

    /// #328's lesson: a test that constructs the state it asserts on cannot
    /// tell you whether anything produces it. This one starts at the durable
    /// event — the fold `NotificationLog::seed_from_events` performs at boot —
    /// and ends at what the panel lists, so the panel is shown reading a log
    /// rebuilt from the event log rather than one a test filled in.
    #[test]
    fn the_panel_reads_a_log_rebuilt_from_the_durable_event_stream() {
        let mut state = AppState::test_new();
        let events: Vec<crate::api::schema::EventEnvelope> = vec![
            crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::NotificationFiled,
                data: crate::api::schema::EventData::NotificationFiled {
                    notification_id: "ntf:restored".into(),
                    title: "restored from the event log".into(),
                    body: Some("the pane is gone".into()),
                    kind: NotificationRecordKind::Attention,
                    source: NotificationSource::AgentState,
                    workspace_id: None,
                    pane_id: None,
                    origin_host: "host.invalid".into(),
                    filed_at_ms: 1_700_000_000_000,
                },
            },
            crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::NotificationSeen,
                data: crate::api::schema::EventData::NotificationSeen {
                    notification_id: "ntf:restored".into(),
                },
            },
        ];
        state.notifications.seed_from_events(events.iter());
        state.open_notification_panel();

        let rows = state.notification_panel_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "ntf:restored");
        assert_eq!(rows[0].kind, NotificationRecordKind::Attention);
        assert_eq!(rows[0].body.as_deref(), Some("the pane is gone"));
        assert!(rows[0].seen, "the acknowledged one is not unread");
        assert_eq!(state.notifications.unread(), 0);
    }

    /// The ack must be the shipped mutation, not a panel-local flag: the
    /// pending event is what survives a restart, so a test that asserts on
    /// `pending_ui_events` is asserting on the durable write the socket API
    /// also makes.
    #[test]
    fn acknowledging_from_the_panel_writes_the_durable_seen_event() {
        let mut state = AppState::test_new();
        state.file_notification(entry("a", false));
        state.file_notification(entry("b", false));
        state.open_notification_panel();
        state.notifications_panel.selected = 1; // "a", the oldest

        assert!(state.acknowledge_selected_notification());

        assert_eq!(state.notifications.unread(), 1);
        assert!(
            state.pending_ui_events.iter().any(|event| matches!(
                event,
                crate::app::state::PendingUiEvent::NotificationSeen { notification_id }
                    if notification_id == "a"
            )),
            "the ack queued the same event `notification.ack` queues"
        );
    }

    /// Under the unread filter the acknowledged row leaves the view, so the
    /// cursor must not be left past the end of a shorter list.
    #[test]
    fn acknowledging_under_the_unread_filter_keeps_the_cursor_on_a_row() {
        let mut state = AppState::test_new();
        state.file_notification(entry("oldest", false));
        state.file_notification(entry("newest", false));
        state.open_notification_panel();
        state.toggle_notification_panel_unread_only();
        assert_eq!(state.notification_panel_rows().len(), 2);
        state.notifications_panel.selected = 1;

        assert!(state.acknowledge_selected_notification());

        let rows = state.notification_panel_rows();
        assert_eq!(rows.len(), 1);
        assert!(
            state.notifications_panel.selected < rows.len(),
            "selected {} of {} rows",
            state.notifications_panel.selected,
            rows.len()
        );
    }

    /// Acknowledging something already read is not a second write — the same
    /// rule `notification.ack` follows, and the reason the panel reports the
    /// row as unchanged rather than pretending it did something.
    #[test]
    fn acknowledging_a_read_row_reports_no_change() {
        let mut state = AppState::test_new();
        state.file_notification(entry("a", true));
        state.open_notification_panel();

        assert!(!state.acknowledge_selected_notification());
    }

    #[test]
    fn moving_the_cursor_stops_at_both_ends() {
        let mut state = AppState::test_new();
        state.file_notification(entry("a", false));
        state.file_notification(entry("b", false));
        state.open_notification_panel();

        state.move_notification_panel_selection(-1);
        assert_eq!(state.notifications_panel.selected, 0);
        state.move_notification_panel_selection(-1);
        assert_eq!(state.notifications_panel.selected, 0);

        state.move_notification_panel_selection(1);
        state.move_notification_panel_selection(1);
        assert_eq!(state.notifications_panel.selected, 1);
    }

    /// An empty log is a normal state (nothing has happened), not an error
    /// path: opening the panel must not leave a cursor pointing at nothing.
    #[test]
    fn an_empty_log_leaves_the_cursor_nowhere_rather_than_past_the_end() {
        let mut state = AppState::test_new();
        state.open_notification_panel();
        state.notifications_panel.selected = 9;

        assert!(state.notification_panel_rows().is_empty());
        assert!(state.selected_notification_panel_entry().is_none());
        assert!(!state.acknowledge_selected_notification());
        state.move_notification_panel_selection(-1);
        assert_eq!(state.notifications_panel.selected, 0);
    }

    #[test]
    fn toggling_the_filter_keeps_the_operator_on_the_same_record() {
        let mut state = AppState::test_new();
        state.file_notification(entry("oldest", true));
        state.file_notification(entry("middle", false));
        state.open_notification_panel();
        state.toggle_notification_panel_unread_only();
        state.notifications_panel.selected = 0;

        state.toggle_notification_panel_unread_only();

        let rows = state.notification_panel_rows();
        let ids: Vec<&str> = rows.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ids, vec!["middle", "oldest"]);
        assert_eq!(
            state.notifications_panel.selected, 0,
            "the record the cursor was on is still row 0"
        );
    }

    /// Acknowledging everything unread is the `all` variant of the same verb,
    /// so it takes the same path and reports the same count.
    #[test]
    fn acknowledging_all_from_the_panel_walks_every_unread_record() {
        let mut state = AppState::test_new();
        state.file_notification(entry("read", true));
        state.file_notification(entry("a", false));
        state.file_notification(entry("b", false));
        state.open_notification_panel();

        assert_eq!(state.acknowledge_all_notifications_from_panel(), 2);
        assert_eq!(state.notifications.unread(), 0);
        assert_eq!(
            state
                .pending_ui_events
                .iter()
                .filter(|event| matches!(
                    event,
                    crate::app::state::PendingUiEvent::NotificationSeen { .. }
                ))
                .count(),
            2,
            "the record that was already read is not rewritten"
        );
    }
}
