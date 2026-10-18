//! Operator-only list/purge, with durable commit before live registry mutation.
use super::{
    responses::{encode_error, encode_success},
    SpawnCaller,
};
use crate::{
    api::{
        error_code::ErrorCode,
        retained::{PurgeParams, Selection},
        schema::{Method, ResponseResult},
    },
    app::App,
    structured::registry::RetainedKind,
};

impl App {
    pub(super) fn handle_retained(&mut self, id: String, method: Method) -> String {
        if self.current_api_peer_pid.is_none()
            || !self.current_api_peer_uid.flatten().is_some_and(|uid| {
                self.runtime_config.uid_policy.classify(Some(uid))
                    == crate::structured::identity::Principal::Operator
            })
            || !matches!(self.spawn_caller(), SpawnCaller::Operator { .. })
        {
            return encode_error(id, ErrorCode::OperatorOnly.as_str(), "retained records require the operator principal; agent credentials cannot authorize maintenance");
        }
        if let Err(error) = self.state.structured_agents.check_retained_collection() {
            return encode_error(
                id,
                ErrorCode::RetainedCollectionShape.as_str(),
                error.to_string(),
            );
        }
        let mut removed = 0;
        if let Method::SessionRetainedPurge(PurgeParams {
            selection,
            yes,
            older_than_ms,
        }) = method
        {
            if selection == Selection::All && !yes {
                return encode_error(
                    id,
                    ErrorCode::InvalidParams.as_str(),
                    "purging forward-compatible records requires --all --yes",
                );
            }
            let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(elapsed) => elapsed.as_millis().min(u64::MAX as u128) as u64,
                Err(_) => {
                    return encode_error(
                        id,
                        ErrorCode::InternalError.as_str(),
                        "retained purge refused: invalid wall clock",
                    )
                }
            };
            let all = selection == Selection::All;
            let mut candidate = self.state.structured_agents.snapshot();
            removed = candidate.purge_retained(all, older_than_ms, now);
            candidate.prepare_retained_save(now, true);
            if !self.no_session {
                let snapshot = self.capture_session_snapshot(&candidate, Some(now));
                if let Err(error) = crate::persist::save_metadata(&snapshot) {
                    tracing::warn!("retained purge persistence failed: {error}");
                    return encode_error(
                        id,
                        ErrorCode::InternalError.as_str(),
                        "retained purge was not committed; session save failed",
                    );
                }
            }
            // Apply the same selection after commit, without replacing active worker state.
            self.state
                .structured_agents
                .purge_retained(all, older_than_ms, now);
            self.state
                .structured_agents
                .prepare_retained_save(now, true);
        }
        let records = self.state.structured_agents.retained_records();
        let forward = records
            .iter()
            .filter(|r| r.kind == RetainedKind::Forward)
            .count();
        let dead = records.len() - forward;
        let purge_hint = (forward > 0 || dead > crate::structured::registry::RETAINED_DEAD_MAX / 2)
            .then(|| "inspect with flk session retained list; purge with flk session retained purge --dead, or --all --yes after checking rollback needs".into());
        encode_success(
            id,
            ResponseResult::Retained {
                records,
                forward,
                dead,
                removed,
                purge_hint,
            },
        )
    }
}

#[cfg(test)]
mod tests;
