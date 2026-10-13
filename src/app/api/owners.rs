//! `owner.bind`, `owner.unbind`, `owner.list` and `owner.show` (ADR-0035 §1).
//!
//! Binding and unbinding are operator verbs: an owner holds merge and
//! teardown authority over its delegates (ADR-0027 §5), so no agent may
//! appoint itself (the ADR-0014 boundary). The reads are open to every
//! caller, MCP included.

use super::responses::{encode_error, encode_success};
use super::spawn::SpawnCaller;
use crate::api::schema::{
    EventData, EventEnvelope, EventKind, OwnerBindParams, OwnerBinding, OwnerRepoParams,
    ResponseResult,
};
use crate::app::App;

impl App {
    pub(super) fn handle_owner_bind(&mut self, id: String, params: OwnerBindParams) -> String {
        if let Some(refusal) = self.refuse_non_operator(&id, "owner.bind") {
            return refusal;
        }
        if let Err(problem) = validate_bind(&params) {
            return encode_error(id, "invalid_params", problem);
        }
        let params = OwnerBindParams {
            repo: crate::app::owners::repo_key(&params.repo),
            ..params
        };
        let node = crate::app::short_host_name();
        let previous = self.owners.get(&params.repo).cloned();
        let binding = OwnerBinding {
            agent_id: previous.as_ref().map_or_else(
                || crate::terminal::AgentId::alloc(&node).to_string(),
                |previous| previous.agent_id.clone(),
            ),
            since_ms: previous
                .as_ref()
                .map_or_else(crate::app::notifications::now_ms, |previous| {
                    previous.since_ms
                }),
            repo: params.repo,
            node,
            checkout: params.checkout,
            harness: params.harness,
            model: params.model,
            profiles: params.profiles,
        };
        self.record_owner_event(EventEnvelope {
            event: EventKind::OwnerBound,
            data: EventData::OwnerBound {
                binding: binding.clone(),
                reissued: false,
            },
        });
        encode_success(
            id,
            ResponseResult::OwnerBound {
                binding,
                rebound: previous.is_some(),
            },
        )
    }

    pub(super) fn handle_owner_unbind(&mut self, id: String, params: OwnerRepoParams) -> String {
        if let Some(refusal) = self.refuse_non_operator(&id, "owner.unbind") {
            return refusal;
        }
        let Some(binding) = self.owners.get(&params.repo).cloned() else {
            return not_bound(id, &params.repo);
        };
        self.record_owner_event(EventEnvelope {
            event: EventKind::OwnerUnbound,
            data: EventData::OwnerUnbound {
                repo: binding.repo.clone(),
            },
        });
        encode_success(id, ResponseResult::OwnerUnbound { binding })
    }

    pub(super) fn handle_owner_list(&mut self, id: String) -> String {
        let bindings = self.owners.all().cloned().collect();
        encode_success(id, ResponseResult::OwnerList { bindings })
    }

    pub(super) fn handle_owner_show(&mut self, id: String, params: OwnerRepoParams) -> String {
        match self.owners.get(&params.repo) {
            Some(binding) => encode_success(
                id,
                ResponseResult::OwnerShow {
                    binding: binding.clone(),
                },
            ),
            None => not_bound(id, &params.repo),
        }
    }

    /// The `operator_only` refusal for an operator verb, or `None` when the
    /// caller is the operator. Refused: a caller whose pid the server could
    /// not read (fail closed, as `peers.enroll_reset` does), a caller whose
    /// process ancestry reaches an agent's pane, and a request relayed from a
    /// mesh peer, whose caller this node cannot attest. This is cooperative
    /// only: a detached (`setsid`) child of an agent attests as the operator
    /// until uid attestation lands (ADR-0035 §6.3 points 1 and 6, O19a #1208).
    fn refuse_non_operator(&mut self, id: &str, verb: &str) -> Option<String> {
        let agent = matches!(self.spawn_caller(), SpawnCaller::Agent { .. });
        (self.caller_unattested_or_relayed() || agent).then(|| {
            encode_error(
                id.to_string(),
                "operator_only",
                format!(
                    "{verb} is an operator verb (ADR-0035 §1): an agent, relayed or unattested caller cannot change an owner binding; run it from a shell outside any agent pane"
                ),
            )
        })
    }
}

impl App {
    /// A caller this node cannot attest: its pid could not be read, or the
    /// request was relayed from a mesh peer. Neither has an agent pane in its
    /// ancestry, so a check that only asks "is this an agent?" would take it
    /// for the operator.
    pub(super) fn caller_unattested_or_relayed(&mut self) -> bool {
        self.current_api_peer_pid.is_none()
            || self
                .inbound
                .edge(
                    self.current_api_peer_pid,
                    crate::platform::process_start_time,
                )
                .is_some()
    }
}

fn not_bound(id: String, repo: &str) -> String {
    encode_error(
        id,
        "not_found",
        format!("no owner is bound to {repo:?} on this node; `owner.list` shows the bindings here"),
    )
}

/// The checks `owner.bind` makes before recording anything: a GitHub slug
/// (ADR-0035 §1 keys a binding by it, never by a local path) and an absolute
/// path to an existing git checkout on this node.
fn validate_bind(params: &OwnerBindParams) -> Result<(), String> {
    if !is_repo_slug(&params.repo) {
        return Err(format!(
            "repo {:?} is not a GitHub slug of the form owner/name",
            params.repo
        ));
    }
    let checkout = std::path::Path::new(&params.checkout);
    if !checkout.is_absolute() || !checkout.join(".git").exists() {
        return Err(format!(
            "checkout {:?} must be an absolute path to a git checkout on this node",
            params.checkout
        ));
    }
    let blank = |value: &str| value.trim().is_empty();
    if params.model.as_deref().is_some_and(blank) || params.profiles.iter().any(|p| blank(p)) {
        return Err("model and profile names must not be empty".into());
    }
    Ok(())
}

/// `owner/name` with GitHub's character sets: an account name of ASCII
/// alphanumerics and `-`, a repo name that also allows `.` and `_`.
pub(crate) fn is_repo_slug(repo: &str) -> bool {
    let Some((account, name)) = repo.split_once('/') else {
        return false;
    };
    !account.is_empty()
        && account
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        && !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_'))
}
