//! Operator drafts an injection moved aside and could not type back (#850).
//!
//! The draft lives in flock's memory only, never the system clipboard, keyed
//! by the pane's public id. Keeping it here is the last resort that makes the
//! stash safe: whatever flock could not confirm in the composer, it can still
//! show and restore on request.
use std::collections::BTreeMap;

use super::api::responses::{encode_error, encode_success};
use crate::api::schema::{PaneTarget, ResponseResult};
use crate::app::App;

#[derive(Debug, Default)]
pub(crate) struct KeptDrafts {
    /// Oldest first, per public pane id.
    drafts: BTreeMap<String, Vec<String>>,
    /// Panes whose new draft the operator has not been told about yet.
    unannounced: Vec<String>,
    /// Drafts an injection has moved aside and not yet settled, by attempt
    /// id: (pane, draft).
    in_flight: BTreeMap<String, (String, String)>,
}

impl KeptDrafts {
    pub(crate) fn drafts(&self, pane: &str) -> &[String] {
        self.drafts.get(pane).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn track_in_flight(&mut self, attempt: &str, pane: &str, draft: &str) {
        self.in_flight
            .insert(attempt.to_owned(), (pane.to_owned(), draft.to_owned()));
    }

    /// The attempt takes its draft back to settle it. False when an exit
    /// already kept it.
    pub(crate) fn claim_in_flight(&mut self, attempt: &str) -> bool {
        self.in_flight.remove(attempt).is_some()
    }

    /// Keep every draft still in flight, returning the panes they belong to.
    pub(crate) fn keep_all_in_flight(&mut self) -> Vec<String> {
        let in_flight = std::mem::take(&mut self.in_flight);
        in_flight
            .into_values()
            .map(|(pane, draft)| {
                self.keep(&pane, draft);
                pane
            })
            .collect()
    }

    fn keep(&mut self, pane: &str, draft: String) {
        self.drafts.entry(pane.to_owned()).or_default().push(draft);
        self.unannounced.push(pane.to_owned());
    }

    /// What a live handoff carries to the next server.
    pub(crate) fn export(&self) -> BTreeMap<String, Vec<String>> {
        self.drafts.clone()
    }

    /// Take a previous server's kept drafts, ahead of any this one keeps.
    pub(crate) fn import(&mut self, drafts: BTreeMap<String, Vec<String>>) {
        for (pane, mut carried) in drafts {
            let entry = self.drafts.entry(pane).or_default();
            carried.append(entry);
            *entry = carried;
        }
    }
}

impl App {
    pub(crate) fn keep_draft(&self, pane: &str, draft: String) {
        crate::logging::pane_draft_kept(pane);
        self.kept_drafts.borrow_mut().keep(pane, draft);
    }

    /// Tell the operator about each newly kept draft. The notification names
    /// the pane and the command, never the draft: the log is durable.
    pub(crate) fn surface_kept_drafts(&mut self) {
        let panes = std::mem::take(&mut self.kept_drafts.borrow_mut().unannounced);
        for pane in panes {
            let _ = self.show_notification(
                format!("pane:stash:{pane}"),
                crate::api::schema::NotificationShowParams {
                    title: "flock kept your draft".to_string(),
                    body: Some(format!(
                        "an injection could not type it back into {pane}; see flk pane stash show {pane}"
                    )),
                    position: None,
                    sound: crate::api::schema::NotificationShowSound::Request,
                },
                crate::app::notifications::NotificationFiling {
                    kind: crate::api::schema::NotificationRecordKind::Attention,
                    pane_id: Some(pane),
                },
            );
        }
    }

    /// The stash is keyed by the public id, which outlives the pane, so a
    /// closed pane's drafts can still be read by the id it had.
    fn stash_key(&self, pane: &str) -> String {
        self.resolve_pane_target(pane)
            .ok()
            .and_then(|(ws, pane_id)| self.public_pane_id(ws, pane_id))
            .unwrap_or_else(|| pane.to_owned())
    }

    pub(crate) fn handle_pane_stash_show(&mut self, id: String, target: PaneTarget) -> String {
        let pane_id = self.stash_key(&target.pane_id);
        let drafts = self.kept_drafts.borrow().drafts(&pane_id).to_vec();
        encode_success(id, ResponseResult::PaneStash { pane_id, drafts })
    }

    /// Type the oldest kept draft into the pane's composer, which must read
    /// empty, as the operator would have. It leaves the stash only once
    /// written.
    pub(crate) fn handle_pane_stash_restore(&mut self, id: String, target: PaneTarget) -> String {
        let pane_id = self.stash_key(&target.pane_id);
        let Some(draft) = self.kept_drafts.borrow().drafts(&pane_id).first().cloned() else {
            return encode_error(id, "stash_empty", "no kept draft for this pane");
        };
        let Some((ws, pane)) = self.parse_pane_id(&pane_id) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        let (Some(terminal), Some(runtime)) = (
            self.terminal_for_pane(ws, pane),
            self.lookup_runtime_sender(ws, pane),
        ) else {
            return encode_error(id, "pane_gone", "pane_gone");
        };
        if self.active_submissions.contains(&pane_id) || runtime.injection_lease_until().is_some() {
            return encode_error(id, "injection_pending", "injection_pending");
        }
        let Some(agent) = terminal.effective_known_agent() else {
            return encode_error(id, "unknown_composer", "unknown_composer");
        };
        match super::guarded_submit::runtime_composer(agent, runtime, "") {
            super::guarded_submit::Composer::Empty => {}
            super::guarded_submit::Composer::Unknown => {
                return encode_error(id, "unknown_composer", "unknown_composer")
            }
            _ => return encode_error(id, "input_not_empty", "input_not_empty"),
        }
        let bytes = super::api_helpers::encode_api_text(runtime, &draft);
        if let Err(err) = runtime.try_send_bytes(bytes::Bytes::from(bytes)) {
            return encode_error(id, "write_failed", err.to_string());
        }
        {
            let mut kept = self.kept_drafts.borrow_mut();
            let emptied = kept.drafts.get_mut(&pane_id).is_some_and(|drafts| {
                drafts.remove(0);
                drafts.is_empty()
            });
            if emptied {
                kept.drafts.remove(&pane_id);
            }
        }
        encode_success(
            id,
            ResponseResult::PaneStash {
                pane_id,
                drafts: vec![draft],
            },
        )
    }
}
