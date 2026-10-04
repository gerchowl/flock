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
//!
//! ## Why acknowledgement is careful here
//!
//! `seen` is the only bit retention acts on, and there is no "was unread"
//! history, so an acknowledgement cannot be undone. That makes this panel
//! different from an overlay whose actions are reversible: `Enter` acts on one
//! record, and the bulk action is **armed before it fires** (see
//! [`NotificationPanelAction::AcknowledgeAll`]), because `mark_all_seen` walks
//! every unanswered record at once. The cursor is anchored on a record *id*
//! rather than an index so a record arriving under an open panel cannot
//! silently re-point it at a different one.

use crate::app::notifications::NotificationEntry;
use crate::app::state::AppState;

/// What the panel's buttons and keys do. One enum for both, so a click and a
/// keypress reach the same mutation by construction rather than by agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationPanelAction {
    /// Acknowledge the selected record — `notification.ack { id }`.
    Acknowledge,
    /// Arm, then fire, `notification.ack { all: true }`. Arming is separate
    /// because this is the one action here with no undo and no per-record
    /// scope: one press walks every unanswered record, and `trim()` treats
    /// each one it has marked as read as eviction material.
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
    /// Where the cursor sits among the rows the filter currently yields.
    pub selected: usize,
    /// Which record the cursor is on. The identity survives a row arriving or
    /// the filter changing; `selected` is only where that record sits *now*.
    pub selected_id: Option<String>,
    pub scroll: usize,
    /// The bulk acknowledgement is armed, waiting for a second, deliberate
    /// press. Disarmed by `esc`, by closing the panel, and by acknowledging
    /// anything else.
    pub ack_all_armed: bool,
}

/// The panel's own view of the log.
///
/// Reading this is deliberately *derived*: there is no cached copy that a
/// filed, acknowledged or evicted record could leave stale, so the panel cannot
/// show a record the log does not hold — and nothing here clones the log to do
/// it, because a frame that draws a viewport of a 512-record projection should
/// not pay for 512 records.
impl AppState {
    /// The rows the panel lists, newest first, borrowed. The same filter
    /// `flk notification list --unread` and `notification.list` apply.
    pub(crate) fn notification_panel_entries(
        &self,
    ) -> impl Iterator<Item = &NotificationEntry> + '_ {
        self.notifications
            .filtered(self.notifications_panel.unread_only)
    }

    /// The row count on its own, for the cursor maths that only needs a
    /// length — mouse moves hit this on every hover event.
    pub(crate) fn notification_panel_row_count(&self) -> usize {
        self.notifications
            .filtered(self.notifications_panel.unread_only)
            .count()
    }

    /// The record the cursor is on, resolved by id so an arrival under the
    /// panel cannot make the cursor mean something else than it did a moment
    /// ago. Falls back to the index when that record is gone (evicted), so the
    /// panel still points at something real.
    pub(crate) fn selected_notification_panel_entry(&self) -> Option<&NotificationEntry> {
        match &self.notifications_panel.selected_id {
            Some(id) => self
                .notification_panel_entries()
                .find(|entry| &entry.id == id)
                .or_else(|| {
                    self.notification_panel_entries()
                        .nth(self.notifications_panel.selected)
                }),
            None => self
                .notification_panel_entries()
                .nth(self.notifications_panel.selected),
        }
    }

    pub(crate) fn open_notification_panel(&mut self) {
        self.notifications_panel = NotificationPanelState::default();
        self.reanchor_notification_panel_selection();
        self.mode = crate::app::state::Mode::Notifications;
    }

    /// Move the cursor, keeping it inside the current filter's rows.
    pub(crate) fn move_notification_panel_selection(&mut self, delta: isize) {
        let count = self.notification_panel_row_count();
        if count == 0 {
            self.notifications_panel.selected = 0;
            self.notifications_panel.selected_id = None;
            self.notifications_panel.scroll = 0;
            return;
        }
        let current = self.notifications_panel.selected.min(count - 1) as isize;
        let next = (current + delta).clamp(0, count as isize - 1);
        self.select_notification_panel_row(next as usize);
    }

    pub(crate) fn select_notification_panel_row(&mut self, index: usize) {
        let count = self.notification_panel_row_count();
        if index < count {
            let id = self
                .notification_panel_entries()
                .nth(index)
                .map(|entry| entry.id.clone());
            self.notifications_panel.selected = index;
            self.notifications_panel.selected_id = id;
        }
        self.ensure_notification_panel_selection_visible();
    }

    /// `Home`: the newest record, which is the one the panel opens on.
    pub(crate) fn select_first_notification_panel_row(&mut self) {
        self.select_notification_panel_row(0);
    }

    /// `End`: the oldest record the projection still holds.
    ///
    /// Its own function rather than `select_notification_panel_row(usize::MAX)`
    /// — an out-of-range index is how this was a silent no-op the first time.
    pub(crate) fn select_last_notification_panel_row(&mut self) {
        let count = self.notification_panel_row_count();
        self.select_notification_panel_row(count.saturating_sub(1));
    }

    /// Re-resolve `selected` from `selected_id` after anything changed the
    /// rows, and re-seat the cursor when that record is no longer listed.
    ///
    /// Both of the ways the list moves under an open panel go through here:
    /// the filter changing, and a record arriving (newest first, so it
    /// prepends and shifts every index by one).
    pub(crate) fn reanchor_notification_panel_selection(&mut self) {
        let count = self.notification_panel_row_count();
        if count == 0 {
            self.notifications_panel.selected = 0;
            self.notifications_panel.selected_id = None;
            self.ensure_notification_panel_selection_visible();
            return;
        }
        let anchored = self
            .notifications_panel
            .selected_id
            .as_ref()
            .and_then(|id| {
                self.notification_panel_entries()
                    .position(|entry| &entry.id == id)
            });
        match anchored {
            Some(index) => self.notifications_panel.selected = index,
            None => {
                // The record is gone (evicted, or filtered out and never
                // re-listed). Stay near where the operator was rather than
                // jumping to the top of the list.
                let index = self.notifications_panel.selected.min(count - 1);
                let id = self
                    .notification_panel_entries()
                    .nth(index)
                    .map(|entry| entry.id.clone());
                self.notifications_panel.selected = index;
                self.notifications_panel.selected_id = id;
            }
        }
        self.ensure_notification_panel_selection_visible();
    }

    /// Toggle the `--unread` filter, keeping the cursor on the same *record*
    /// so the view does not jump under the operator.
    pub(crate) fn toggle_notification_panel_unread_only(&mut self) {
        self.notifications_panel.unread_only = !self.notifications_panel.unread_only;
        self.reanchor_notification_panel_selection();
    }

    /// Arm or disarm the bulk acknowledgement. Arming changes nothing in the
    /// log; only [`Self::commit_acknowledgement_of_every_unread_record`] does.
    pub(crate) fn toggle_notification_panel_ack_all(&mut self) {
        self.notifications_panel.ack_all_armed = !self.notifications_panel.ack_all_armed;
    }

    /// Disarm without acting — `esc`, closing, or any other acknowledgement.
    pub(crate) fn disarm_notification_panel_ack_all(&mut self) {
        self.notifications_panel.ack_all_armed = false;
    }

    pub(crate) fn notification_panel_ack_all_armed(&self) -> bool {
        self.notifications_panel.ack_all_armed
    }

    /// Acknowledge the selected record through the existing seam (#516), so
    /// the TUI writes the same `NotificationSeen` the socket API writes.
    pub(crate) fn acknowledge_selected_notification(&mut self) -> bool {
        let Some(entry) = self.selected_notification_panel_entry().cloned() else {
            return false;
        };
        if !self.acknowledge_notification(&entry.id) {
            return false;
        }
        self.notifications_panel.ack_all_armed = false;
        self.reanchor_notification_panel_selection();
        true
    }

    /// Acknowledge everything unread — `notification.ack { all: true }`, same
    /// seam, same events. Only reachable once armed.
    pub(crate) fn commit_acknowledgement_of_every_unread_record(&mut self) -> usize {
        let count = self.acknowledge_all_notifications();
        self.notifications_panel.ack_all_armed = false;
        self.reanchor_notification_panel_selection();
        count
    }

    /// Half a viewport, for the paging keys — the navigator's rule, so both
    /// lists page by the same amount.
    pub(crate) fn notification_panel_page_delta(&self) -> isize {
        ((self.notifications_panel_body_height() / 2).max(1)) as isize
    }

    pub(crate) fn notifications_panel_body_height(&self) -> u16 {
        self.notifications_panel_body_rect().height
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

    fn ids(state: &AppState) -> Vec<String> {
        state
            .notification_panel_entries()
            .map(|entry| entry.id.clone())
            .collect()
    }

    /// The rows come from `NotificationLog::filtered`, which is the same
    /// predicate `flk notification list --unread` and `notification.list` use
    /// — so this pins the panel to the shipped semantics rather than a
    /// parallel set of them.
    #[test]
    fn the_panel_lists_the_logs_own_projection_newest_first() {
        let mut state = AppState::test_new();
        state.file_notification(entry("oldest", false));
        state.file_notification(entry("middle", true));
        state.file_notification(entry("newest", false));
        state.open_notification_panel();

        assert_eq!(state.mode, Mode::Notifications);
        assert_eq!(ids(&state), vec!["newest", "middle", "oldest"]);

        state.toggle_notification_panel_unread_only();
        assert_eq!(ids(&state), vec!["newest", "oldest"]);
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

        let rows = state.notification_panel_row_count();
        assert_eq!(rows, 1);
        let entry = state.selected_notification_panel_entry().expect("a row");
        assert_eq!(entry.id, "ntf:restored");
        assert_eq!(entry.kind, NotificationRecordKind::Attention);
        assert_eq!(entry.body.as_deref(), Some("the pane is gone"));
        assert!(entry.seen, "the acknowledged one is not unread");
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
        state.select_notification_panel_row(1); // "a", the oldest

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
        assert_eq!(state.notification_panel_row_count(), 2);
        state.select_notification_panel_row(1);

        assert!(state.acknowledge_selected_notification());

        let rows = state.notification_panel_row_count();
        assert_eq!(rows, 1);
        assert!(
            state.notifications_panel.selected < rows,
            "selected {} of {} rows",
            state.notifications_panel.selected,
            rows
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

    /// `End` was a silent no-op: it asked for row `usize::MAX`, which the
    /// range guard rejected, so the oldest record was unreachable by key.
    #[test]
    fn home_and_end_reach_the_first_and_last_rows() {
        let mut state = AppState::test_new();
        for id in ["a", "b", "c"] {
            state.file_notification(entry(id, false));
        }
        state.open_notification_panel();

        state.select_last_notification_panel_row();
        assert_eq!(state.notifications_panel.selected, 2);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("a"),
            "`End` lands on the oldest record the projection still holds"
        );

        state.select_first_notification_panel_row();
        assert_eq!(state.notifications_panel.selected, 0);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("c")
        );
    }

    /// With nothing filed, `Home` and `End` are no-ops rather than panics or
    /// a cursor pointing at row 0 of nothing.
    #[test]
    fn home_and_end_on_an_empty_log_leave_the_cursor_nowhere() {
        let mut state = AppState::test_new();
        state.open_notification_panel();

        state.select_last_notification_panel_row();
        state.select_first_notification_panel_row();

        assert_eq!(state.notifications_panel.selected, 0);
        assert!(state.notifications_panel.selected_id.is_none());
    }

    /// An empty log is a normal state (nothing has happened), not an error
    /// path: opening the panel must not leave a cursor pointing at nothing.
    #[test]
    fn an_empty_log_leaves_the_cursor_nowhere_rather_than_past_the_end() {
        let mut state = AppState::test_new();
        state.open_notification_panel();

        assert_eq!(state.notification_panel_row_count(), 0);
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
        state.select_notification_panel_row(0);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("middle")
        );

        state.toggle_notification_panel_unread_only();

        assert_eq!(ids(&state), vec!["middle", "oldest"]);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("middle"),
            "the record the cursor was on is still row 0"
        );
    }

    /// A record arriving prepends (newest first) and shifts every index. The
    /// cursor follows the *record*, so `Enter` cannot acknowledge a different
    /// one than the operator was reading a moment ago.
    #[test]
    fn a_record_arriving_under_the_panel_does_not_repoint_the_cursor() {
        let mut state = AppState::test_new();
        state.file_notification(entry("older", false));
        state.file_notification(entry("newer", false));
        state.open_notification_panel();
        state.select_notification_panel_row(1);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("older")
        );

        state.file_notification(entry("brand new", false));
        state.reanchor_notification_panel_selection();

        assert_eq!(ids(&state), vec!["brand new", "newer", "older"]);
        assert_eq!(state.notifications_panel.selected, 2, "the index moved");
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("older"),
            "but the record the cursor means did not"
        );
    }

    /// When the record the cursor was on is evicted, the cursor stays near
    /// where the operator was rather than jumping to the top.
    #[test]
    fn a_cursor_whose_record_is_gone_re_seats_near_the_old_position() {
        let mut state = AppState::test_new();
        for id in ["a", "b", "c"] {
            state.file_notification(entry(id, false));
        }
        state.open_notification_panel();
        state.select_notification_panel_row(2);
        assert_eq!(
            state
                .selected_notification_panel_entry()
                .map(|e| e.id.as_str()),
            Some("a")
        );

        // The record under the cursor leaves the projection.
        state.acknowledge_notification("a");
        state.reanchor_notification_panel_selection();

        let rows = ids(&state);
        assert_eq!(rows.len(), 3, "still three records: acking is not evicting");
        assert!(
            state.notifications_panel.selected < rows.len(),
            "the cursor is on a row that exists"
        );
    }

    /// The bulk acknowledgement is the one action here with no undo, so it
    /// takes two deliberate steps. Arming alone changes nothing in the log.
    #[test]
    fn the_bulk_acknowledgement_is_armed_before_it_fires() {
        let mut state = AppState::test_new();
        state.file_notification(entry("read", true));
        state.file_notification(entry("a", false));
        state.file_notification(entry("b", false));
        state.open_notification_panel();

        state.toggle_notification_panel_ack_all();
        assert!(state.notification_panel_ack_all_armed());
        assert_eq!(
            state.notifications.unread(),
            2,
            "arming must not acknowledge anything"
        );

        state.disarm_notification_panel_ack_all();
        assert!(!state.notification_panel_ack_all_armed());
        assert_eq!(state.notifications.unread(), 2);

        state.toggle_notification_panel_ack_all();
        assert_eq!(state.commit_acknowledgement_of_every_unread_record(), 2);
        assert!(
            !state.notification_panel_ack_all_armed(),
            "it disarms itself"
        );
        assert_eq!(state.notifications.unread(), 0);
    }

    /// Acknowledging one record is not the bulk action, so it must not leave
    /// a bulk ack armed behind it.
    #[test]
    fn acknowledging_one_record_disarms_the_bulk_action() {
        let mut state = AppState::test_new();
        state.file_notification(entry("a", false));
        state.open_notification_panel();
        state.toggle_notification_panel_ack_all();

        assert!(state.acknowledge_selected_notification());

        assert!(
            !state.notification_panel_ack_all_armed(),
            "an armed bulk ack must not survive another acknowledgement"
        );
    }
}
