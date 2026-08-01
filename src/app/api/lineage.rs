use crate::api::schema::{EventData, LineageEdge, LineageNode, LineageParams, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

/// `agent.lineage` (#175 O1, US-4): walk the fork ancestry of a target from
/// the durable event log, so "why does this worktree exist" is answerable
/// across server restarts and after the panes involved are gone.
///
/// Resolution is persisted-first by child identity: a target matches an
/// `agent_forked` edge by child pane id, child worktree path (or its
/// basename), or child branch. A live-pane target is first normalized to its
/// public pane id via the regular agent grammar so `flk lineage <agent name>`
/// works too.
impl App {
    pub(super) fn handle_agent_lineage(&mut self, id: String, params: LineageParams) -> String {
        let mut target = params.target.trim().to_string();
        if target.is_empty() {
            return encode_error(id, "invalid_request", "target is required");
        }
        // Live resolution first: agent name / terminal id → public pane id.
        if let Ok(resolved) = self.resolve_terminal_target(&target) {
            if let Some(pane_id) = self.public_pane_id(resolved.ws_idx, resolved.pane_id) {
                target = pane_id;
            }
        }

        let forks: Vec<(u64, u64, ForkEdge)> = self
            .event_hub
            .persisted_events_after(0)
            .into_iter()
            .filter_map(|(seq, ts_ms, envelope)| match envelope.data {
                EventData::AgentForked {
                    run_id,
                    parent_pane_id,
                    parent_workspace_id,
                    parent_repo,
                    agent,
                    child_workspace_id,
                    child_pane_id,
                    child_worktree,
                    child_branch,
                    seeded,
                } => Some((
                    seq,
                    ts_ms,
                    ForkEdge {
                        run_id,
                        parent_pane_id,
                        parent_workspace_id,
                        parent_repo,
                        agent,
                        child_workspace_id,
                        child_pane_id,
                        child_worktree,
                        child_branch,
                        seeded,
                    },
                )),
                _ => None,
            })
            .collect();

        let Some(start) = forks
            .iter()
            .rev()
            .find(|(_, _, edge)| edge.child_matches(&target) || edge.parent_pane_id == target)
        else {
            return encode_error(
                id,
                "lineage_not_found",
                format!("no fork lineage recorded for target {target}"),
            );
        };

        // Walk child → parent, newest matching edge first. A parent-only
        // match starts the walk at that edge (the target IS the parent, so
        // its own ancestry is what remains).
        let mut chain: Vec<LineageEdge> = Vec::new();
        let mut cursor: Option<&(u64, u64, ForkEdge)> = Some(start);
        while let Some((seq, ts_ms, edge)) = cursor {
            chain.push(edge.to_wire(*seq, *ts_ms));
            // Guard against pathological cycles (a pane id reused as its own
            // ancestor can only happen with a corrupted log): cap the walk.
            if chain.len() > 64 {
                break;
            }
            let parent = edge.parent_pane_id.clone();
            cursor = forks.iter().rev().find(|(candidate_seq, _, candidate)| {
                *candidate_seq < *seq && candidate.child_pane_id == parent
            });
        }

        encode_success(id, ResponseResult::Lineage { chain })
    }
}

struct ForkEdge {
    run_id: String,
    parent_pane_id: String,
    parent_workspace_id: String,
    parent_repo: String,
    agent: String,
    child_workspace_id: String,
    child_pane_id: String,
    child_worktree: String,
    child_branch: String,
    seeded: bool,
}

impl ForkEdge {
    fn child_matches(&self, target: &str) -> bool {
        if self.child_pane_id == target || self.child_worktree == target {
            return true;
        }
        if self.child_branch == target && !self.child_branch.is_empty() {
            return true;
        }
        std::path::Path::new(&self.child_worktree)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == target)
    }

    fn to_wire(&self, seq: u64, ts_ms: u64) -> LineageEdge {
        LineageEdge {
            seq,
            ts_ms,
            run_id: self.run_id.clone(),
            agent: self.agent.clone(),
            seeded: self.seeded,
            parent: LineageNode {
                pane_id: self.parent_pane_id.clone(),
                workspace_id: self.parent_workspace_id.clone(),
                repo: self.parent_repo.clone(),
                worktree: None,
                branch: None,
            },
            child: LineageNode {
                pane_id: self.child_pane_id.clone(),
                workspace_id: self.child_workspace_id.clone(),
                repo: self.parent_repo.clone(),
                worktree: Some(self.child_worktree.clone()),
                branch: Some(self.child_branch.clone()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{
        ErrorResponse, EventData, EventEnvelope, EventKind, LineageParams, Method, Request,
        ResponseResult, SuccessResponse,
    };
    use crate::config::Config;

    fn forked(
        run: &str,
        parent_pane: &str,
        child_pane: &str,
        worktree: &str,
        branch: &str,
    ) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::AgentForked,
            data: EventData::AgentForked {
                run_id: run.to_string(),
                parent_pane_id: parent_pane.to_string(),
                parent_workspace_id: "w1".into(),
                parent_repo: "/repo/.git".into(),
                agent: "claude".into(),
                child_workspace_id: "wX".into(),
                child_pane_id: child_pane.to_string(),
                child_worktree: worktree.to_string(),
                child_branch: branch.to_string(),
                seeded: true,
            },
        }
    }

    fn app_with_hub(hub: crate::api::EventHub) -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::app::App::new(&Config::default(), true, None, api_rx, hub)
    }

    fn lineage_request(target: &str) -> Request {
        Request {
            id: "req".into(),
            method: Method::AgentLineage(LineageParams {
                target: target.into(),
            }),
        }
    }

    #[tokio::test]
    async fn lineage_walks_fork_chain_across_a_hub_restart() {
        // #175 O1 / US-4: two chained fork edges recorded, hub restarted,
        // the walker reconstructs grandchild → child → root from disk alone.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir()
            .join(format!("flock-lineage-{}-{nanos}", std::process::id()))
            .join("event-log.jsonl");

        let hub = crate::api::EventHub::with_persistence(path.clone());
        hub.push(forked(
            "fork:t1",
            "w1:p1",
            "w2:p1",
            "/wt/first",
            "fork/first",
        ));
        hub.push(forked(
            "fork:t2",
            "w2:p1",
            "w3:p1",
            "/wt/second",
            "fork/second",
        ));
        drop(hub);

        let restarted = crate::api::EventHub::with_persistence(path.clone());
        let mut app = app_with_hub(restarted);

        // Target by branch name — the identity that survives pane teardown.
        let response = app.handle_api_request(lineage_request("fork/second"));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::Lineage { chain } = success.result else {
            panic!("expected lineage response: {response}");
        };
        assert_eq!(chain.len(), 2, "grandchild chain has two edges");
        assert_eq!(chain[0].run_id, "fork:t2");
        assert_eq!(chain[0].child.branch.as_deref(), Some("fork/second"));
        assert_eq!(chain[1].run_id, "fork:t1");
        assert_eq!(chain[1].parent.pane_id, "w1:p1");

        // Worktree basename also resolves.
        let response = app.handle_api_request(lineage_request("first"));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::Lineage { chain } = success.result else {
            panic!("expected lineage response: {response}");
        };
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].run_id, "fork:t1");

        // Unknown target: loud, typed miss.
        let response = app.handle_api_request(lineage_request("nope"));
        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "lineage_not_found");

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}
