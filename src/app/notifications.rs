//! The operator's notification log (#372, ADR-0016).
//!
//! A toast is a field on `AppState` that a timer clears: a notification you
//! did not see never existed. This module is the read model that fixes that —
//! it is *derived*, not authoritative. The durable record is
//! `EventKind::NotificationFiled` / `NotificationSeen` on the event log
//! (ADR-0005); everything here is a fold over those two kinds, rebuilt at boot
//! by [`NotificationLog::seed_from_events`] exactly as `MailboxRegistry` does
//! for the agent inbox.
//!
//! # Retention
//!
//! Unbounded is a leak; a time bound drops the thing you were about to read.
//! So neither: the projection holds at most [`MAX_NOTIFICATIONS`] records and
//! **evicts read ones before unread ones**, oldest first within each class.
//! Reading is the operator saying "done with this", so reading is what makes a
//! record cheap to lose. Past the cap with nothing read, the oldest unread
//! record goes — there is no third option that is not a leak, so it is logged
//! rather than hidden.
//!
//! The binding constraint is not this cap but the event log's own rotation: a
//! notification older than the retained window cannot be rebuilt after a
//! restart, whatever the cap says.

use std::collections::VecDeque;

use crate::api::schema::{EventData, EventEnvelope, NotificationRecordKind, NotificationSource};

/// How many records the projection keeps. Sized so a fortnight of a busy
/// fleet's outcomes fits, since the operator's question is "what happened
/// while I was away" rather than "what happened this session".
pub(crate) const MAX_NOTIFICATIONS: usize = 512;

/// One filed outcome, with the acknowledgement folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotificationEntry {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub kind: NotificationRecordKind,
    pub source: NotificationSource,
    /// Where to jump to read the whole story, when there is one.
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    /// The node that produced the outcome. Carried rather than assumed so an
    /// outcome from `sage` stays legible when read on `anvil`.
    pub origin_host: String,
    pub filed_at_ms: u64,
    pub seen: bool,
}

/// Oldest at the front, newest at the back.
#[derive(Debug, Default)]
pub(crate) struct NotificationLog {
    entries: VecDeque<NotificationEntry>,
}

impl NotificationLog {
    /// Rebuild from the durable stream: a `NotificationFiled` with no matching
    /// `NotificationSeen` is still unread.
    ///
    /// The whole stream is folded before the cap is applied, deliberately.
    /// Trimming as we replay would evict a record as "unread" when its
    /// acknowledgement is simply later in the stream, so a restart could
    /// resurrect a read notification by dropping a different one.
    pub(crate) fn seed_from_events<'a>(&mut self, events: impl Iterator<Item = &'a EventEnvelope>) {
        let mut entries: VecDeque<NotificationEntry> = VecDeque::new();
        for envelope in events {
            match &envelope.data {
                EventData::NotificationFiled {
                    notification_id,
                    title,
                    body,
                    kind,
                    source,
                    workspace_id,
                    pane_id,
                    origin_host,
                    filed_at_ms,
                } => {
                    entries.push_back(NotificationEntry {
                        id: notification_id.clone(),
                        title: title.clone(),
                        body: body.clone(),
                        kind: *kind,
                        source: *source,
                        workspace_id: workspace_id.clone(),
                        pane_id: pane_id.clone(),
                        origin_host: origin_host.clone(),
                        filed_at_ms: *filed_at_ms,
                        seen: false,
                    });
                }
                EventData::NotificationSeen { notification_id } => {
                    if let Some(entry) = entries
                        .iter_mut()
                        .find(|entry| &entry.id == notification_id)
                    {
                        entry.seen = true;
                    }
                }
                _ => {}
            }
        }
        self.entries = entries;
        self.trim();
    }

    /// File one outcome. Returns the id of anything the cap pushed out.
    pub(crate) fn file(&mut self, entry: NotificationEntry) -> Option<String> {
        self.entries.push_back(entry);
        self.trim().into_iter().next()
    }

    /// Acknowledge one record. `false` when it is unknown or already read, so
    /// the caller does not write a second `NotificationSeen` for it.
    pub(crate) fn mark_seen(&mut self, id: &str) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|entry| entry.id == id && !entry.seen)
        {
            Some(entry) => {
                entry.seen = true;
                true
            }
            None => false,
        }
    }

    /// Acknowledge everything unread, returning the ids that changed.
    pub(crate) fn mark_all_seen(&mut self) -> Vec<String> {
        self.entries
            .iter_mut()
            .filter(|entry| !entry.seen)
            .map(|entry| {
                entry.seen = true;
                entry.id.clone()
            })
            .collect()
    }

    pub(crate) fn unread(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.seen).count()
    }

    /// Newest first — the order the operator reads in.
    pub(crate) fn newest_first(&self) -> impl Iterator<Item = &NotificationEntry> {
        self.entries.iter().rev()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Enforce the cap, read records first. Returns what was dropped.
    fn trim(&mut self) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.entries.len() > MAX_NOTIFICATIONS {
            let victim = match self.entries.iter().position(|entry| entry.seen) {
                Some(index) => index,
                None => {
                    // Nothing read to give up. Dropping the oldest unread is
                    // the only alternative to an unbounded projection, so say
                    // so rather than losing it quietly.
                    if let Some(entry) = self.entries.front() {
                        crate::logging::notification_unread_evicted(&entry.id);
                    }
                    0
                }
            };
            if let Some(entry) = self.entries.remove(victim) {
                evicted.push(entry.id);
            }
        }
        evicted
    }
}

/// Filing and acknowledgement, kept in one place so the projection and the
/// durable event it is derived from can never drift apart: every mutation
/// queues the event that would rebuild it.
impl crate::app::state::AppState {
    pub(crate) fn file_notification(&mut self, entry: NotificationEntry) {
        self.pending_ui_events
            .push(crate::app::state::PendingUiEvent::NotificationFiled {
                notification_id: entry.id.clone(),
                title: entry.title.clone(),
                body: entry.body.clone(),
                kind: entry.kind,
                source: entry.source,
                workspace_id: entry.workspace_id.clone(),
                pane_id: entry.pane_id.clone(),
                origin_host: entry.origin_host.clone(),
                filed_at_ms: entry.filed_at_ms,
            });
        // An eviction is projection-local and emits nothing: the log is the
        // truth, and a restart re-applies the same cap over the same stream.
        self.notifications.file(entry);
    }

    /// Returns whether anything changed, so a repeated ack does not write a
    /// second `NotificationSeen` for the same record.
    pub(crate) fn acknowledge_notification(&mut self, notification_id: &str) -> bool {
        if !self.notifications.mark_seen(notification_id) {
            return false;
        }
        self.pending_ui_events
            .push(crate::app::state::PendingUiEvent::NotificationSeen {
                notification_id: notification_id.to_string(),
            });
        true
    }

    /// Acknowledge everything unread. Returns how many records changed.
    pub(crate) fn acknowledge_all_notifications(&mut self) -> usize {
        let acknowledged = self.notifications.mark_all_seen();
        let count = acknowledged.len();
        for notification_id in acknowledged {
            self.pending_ui_events
                .push(crate::app::state::PendingUiEvent::NotificationSeen { notification_id });
        }
        count
    }
}

/// The unread count the ambient surfaces should render (#372, #367).
///
/// Not simply `NotificationLog::unread`. The #367 title badge already counts
/// *panes* that want you — blocked, and done-but-unseen — and an agent that is
/// still blocked has both a live `B` and an unread `Attention` record. Adding
/// the two would count the same fact twice and inflate the badge exactly when
/// it matters most.
///
/// So this counts only the outcomes the live tally can no longer speak for:
/// a record with no pane, a record whose pane is gone, or one whose pane has
/// moved on. That is the set the issue is actually about — the outcome that
/// outlived the pane it happened in.
impl crate::app::state::AppState {
    pub(crate) fn unread_notifications_beyond_live_states(&self) -> usize {
        let mut spoken_for: std::collections::HashSet<String> = std::collections::HashSet::new();
        for ws_idx in 0..self.workspaces.len() {
            let wanting: Vec<crate::layout::PaneId> = self.workspaces[ws_idx]
                .tabs
                .iter()
                .flat_map(|tab| tab.panes.iter())
                .filter(|(_, pane)| {
                    let state = self
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .map(|terminal| terminal.state);
                    // The badge's own two attention classes, restated here
                    // rather than imported so this stays a pure `AppState`
                    // query (the title renderer takes only `&AppState`).
                    matches!(state, Some(crate::detect::AgentState::Blocked))
                        || (matches!(state, Some(crate::detect::AgentState::Idle)) && !pane.seen)
                })
                .map(|(pane_id, _)| *pane_id)
                .collect();
            for pane_id in wanting {
                if let Some(public) = self.public_pane_id(ws_idx, pane_id) {
                    spoken_for.insert(public);
                }
            }
        }

        self.notifications
            .newest_first()
            .filter(|entry| !entry.seen)
            .filter(|entry| {
                entry
                    .pane_id
                    .as_ref()
                    .is_none_or(|pane_id| !spoken_for.contains(pane_id))
            })
            .count()
    }
}

/// Mints a session-unique notification id. Host-local by design: an outcome
/// is read on the node that produced it (ADR-0016 decision 4 pulls content
/// rather than replicating it), and `origin_host` carries the rest of the
/// identity.
pub(crate) fn mint_notification_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "ntf:{:x}:{:x}",
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn filed(id: &str) -> EventEnvelope {
        EventEnvelope {
            event: crate::api::schema::EventKind::NotificationFiled,
            data: EventData::NotificationFiled {
                notification_id: id.to_string(),
                title: format!("{id} finished"),
                body: None,
                kind: NotificationRecordKind::Outcome,
                source: NotificationSource::AgentState,
                workspace_id: None,
                pane_id: None,
                origin_host: "host.invalid".into(),
                filed_at_ms: 0,
            },
        }
    }

    fn seen(id: &str) -> EventEnvelope {
        EventEnvelope {
            event: crate::api::schema::EventKind::NotificationSeen,
            data: EventData::NotificationSeen {
                notification_id: id.to_string(),
            },
        }
    }

    #[test]
    fn a_filed_notification_survives_the_restart_that_lost_the_toast() {
        let mut log = NotificationLog::default();
        log.seed_from_events([filed("a"), filed("b"), seen("a")].iter());

        assert_eq!(log.len(), 2);
        assert_eq!(log.unread(), 1);
        assert_eq!(
            log.newest_first()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    /// The durable log is JSONL on disk (ADR-0005), so the record has to
    /// survive the format, not just the type. A nested enum or a skipped
    /// `None` that does not round-trip would lose the notification at exactly
    /// the moment it is meant to be recovered — the restart.
    #[test]
    fn the_record_survives_the_json_the_log_stores_it_as() {
        let original = filed("a");
        let encoded = serde_json::to_string(&original).expect("encode");
        let decoded: EventEnvelope = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, original);

        let mut log = NotificationLog::default();
        log.seed_from_events([decoded].iter());

        let entry = log.newest_first().next().expect("rebuilt").clone();
        assert_eq!(entry.id, "a");
        assert_eq!(entry.kind, NotificationRecordKind::Outcome);
        assert_eq!(entry.source, NotificationSource::AgentState);
        assert_eq!(entry.origin_host, "host.invalid");
        assert!(!entry.seen);
    }

    #[test]
    fn acknowledging_twice_does_not_change_the_unread_count() {
        let mut log = NotificationLog::default();
        log.file(entry("a", false));

        assert!(log.mark_seen("a"));
        assert!(!log.mark_seen("a"), "a second ack has nothing to record");
        assert!(!log.mark_seen("missing"));
        assert_eq!(log.unread(), 0);
    }

    #[test]
    fn the_cap_gives_up_read_records_before_unread_ones() {
        let mut log = NotificationLog::default();
        // Oldest is read, everything after it is not.
        log.file(entry("read-oldest", true));
        for index in 0..MAX_NOTIFICATIONS - 1 {
            log.file(entry(&format!("unread-{index}"), false));
        }

        let evicted = log.file(entry("newest", false));

        assert_eq!(evicted.as_deref(), Some("read-oldest"));
        assert_eq!(log.len(), MAX_NOTIFICATIONS);
        assert_eq!(log.unread(), MAX_NOTIFICATIONS);
    }

    #[test]
    fn past_the_cap_with_nothing_read_the_oldest_unread_goes() {
        let mut log = NotificationLog::default();
        for index in 0..MAX_NOTIFICATIONS {
            log.file(entry(&format!("unread-{index}"), false));
        }

        let evicted = log.file(entry("newest", false));

        assert_eq!(evicted.as_deref(), Some("unread-0"));
        assert_eq!(log.len(), MAX_NOTIFICATIONS);
    }

    /// #367's badge already counts a blocked pane. Counting its notification
    /// too would say `2` when one thing wants you.
    #[test]
    fn a_pane_that_still_wants_you_is_not_counted_twice() {
        let mut state = crate::app::state::AppState::test_new();
        state
            .workspaces
            .push(crate::workspace::Workspace::test_new("background"));
        state.ensure_test_terminals();
        let pane = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&terminal_id).unwrap().state = crate::detect::AgentState::Blocked;
        let public_pane_id = state.public_pane_id(0, pane).expect("public pane id");

        let mut about_that_pane = entry("blocked", false);
        about_that_pane.pane_id = Some(public_pane_id);
        state.file_notification(about_that_pane);
        state.file_notification(entry("closed-pane", false));

        assert_eq!(state.notifications.unread(), 2);
        assert_eq!(
            state.unread_notifications_beyond_live_states(),
            1,
            "only the outcome no live pane can speak for"
        );
    }

    /// And once the pane is no longer blocked, the outcome is the only thing
    /// left that remembers it happened — which is the whole point.
    #[test]
    fn an_outcome_outliving_its_pane_state_starts_counting() {
        let mut state = crate::app::state::AppState::test_new();
        state
            .workspaces
            .push(crate::workspace::Workspace::test_new("background"));
        state.ensure_test_terminals();
        let pane = state.workspaces[0].tabs[0].root_pane;
        let terminal_id = state.workspaces[0]
            .pane_state(pane)
            .unwrap()
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&terminal_id).unwrap().state = crate::detect::AgentState::Blocked;
        let public_pane_id = state.public_pane_id(0, pane).expect("public pane id");
        let mut about_that_pane = entry("blocked", false);
        about_that_pane.pane_id = Some(public_pane_id);
        state.file_notification(about_that_pane);
        assert_eq!(state.unread_notifications_beyond_live_states(), 0);

        state.terminals.get_mut(&terminal_id).unwrap().state = crate::detect::AgentState::Working;

        assert_eq!(state.unread_notifications_beyond_live_states(), 1);
    }

    /// The acknowledgement is later in the stream than the record it
    /// acknowledges, so a replay that trimmed as it went would evict the wrong
    /// one and resurrect a notification the operator had already dealt with.
    #[test]
    fn replay_applies_the_cap_after_the_whole_fold_not_during_it() {
        let mut events: Vec<EventEnvelope> = Vec::new();
        for index in 0..=MAX_NOTIFICATIONS {
            events.push(filed(&format!("n-{index}")));
        }
        // The very first record is read — but only stated at the end.
        events.push(seen("n-0"));

        let mut log = NotificationLog::default();
        log.seed_from_events(events.iter());

        assert_eq!(log.len(), MAX_NOTIFICATIONS);
        assert_eq!(log.unread(), MAX_NOTIFICATIONS);
        assert!(
            log.newest_first().all(|entry| entry.id != "n-0"),
            "the read record is the one the cap takes"
        );
    }
}
