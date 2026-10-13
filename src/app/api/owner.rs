//! `owner.policy.show` and `owner.policy.adopt` (ADR-0035 §6.2, #1195), and
//! the adopted-policy fold the server start pass and log rotation re-issue.

use std::path::Path;

use super::responses::{encode_error, encode_success};
use crate::api::schema::{EventData, EventEnvelope, EventKind, OwnerPolicyParams, ResponseResult};
use crate::app::App;
use crate::owner_policy::{self, OwnerPolicyIndex};

impl App {
    /// Server start: fold the adoptions from the log read the delegation pass
    /// already made, and write each repo's adoption into the newest file.
    pub(crate) fn reissue_adopted_owner_policies(
        &mut self,
        restored: &[(u64, u64, EventEnvelope)],
    ) {
        let mut index = OwnerPolicyIndex::default();
        for (_, _, envelope) in restored {
            index.apply(&envelope.data);
        }
        self.owner_policies = index;
        self.emit_owner_policy_reissues();
    }

    pub(crate) fn emit_owner_policy_reissues(&mut self) {
        for event in self.owner_policies.reissue_events() {
            self.emit_event(event);
        }
    }

    pub(super) fn handle_owner_policy_show(
        &mut self,
        id: String,
        params: OwnerPolicyParams,
    ) -> String {
        let report = owner_policy::locate(Path::new(&params.repo)).and_then(|located| {
            owner_policy::report(&located, self.owner_policies.get(&located.repo_key()))
        });
        match report {
            Ok(report) => encode_success(id, ResponseResult::OwnerPolicy(Box::new(report))),
            Err(err) => encode_error(id, "owner_policy_unreadable", err),
        }
    }

    /// Adopt the default branch's current version. Refused for any caller
    /// but the local operator (ADR-0014; cooperative until O19a, see
    /// `caller_is_local_operator`), and when `policy_version` is no longer
    /// the current version, so the operator adopts exactly what they read.
    pub(super) fn handle_owner_policy_adopt(
        &mut self,
        id: String,
        params: OwnerPolicyParams,
    ) -> String {
        if !self.caller_is_local_operator() {
            return encode_error(
                id,
                "operator_only",
                "owner policy adoption requires the local operator outside agent panes and relays (ADR-0035 §6.2)",
            );
        }
        let located = match owner_policy::locate(Path::new(&params.repo)) {
            Ok(located) => located,
            Err(err) => return encode_error(id, "owner_policy_unreadable", err),
        };
        if params.policy_version != located.current {
            return encode_error(
                id,
                "owner_policy_changed",
                format!(
                    "the current {} on {} is {}, not {}; read it with `flk owner policy show` and adopt that",
                    owner_policy::POLICY_FILE,
                    located.git_ref,
                    located.current.as_deref().unwrap_or("none"),
                    params.policy_version.as_deref().unwrap_or("none"),
                ),
            );
        }
        let repo = located.repo_key();
        let adopted_now = self
            .owner_policies
            .get(&repo)
            .is_none_or(|adopted| adopted.policy_version != located.current);
        if let Err(err) = owner_policy::behaviour(&located.common_dir, located.current.as_deref()) {
            return encode_error(id, "owner_policy_unreadable", err);
        }
        if adopted_now {
            let data = EventData::OwnerPolicyAdopted {
                repo: repo.clone(),
                policy_version: located.current.clone(),
                reissued: false,
            };
            self.owner_policies.apply(&data);
            self.emit_event(EventEnvelope {
                event: EventKind::OwnerPolicyAdopted,
                data,
            });
        }
        match owner_policy::report(&located, self.owner_policies.get(&repo)) {
            Ok(mut report) => {
                report.adopted_now = Some(adopted_now);
                encode_success(id, ResponseResult::OwnerPolicy(Box::new(report)))
            }
            Err(err) => encode_error(id, "owner_policy_unreadable", err),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{Method, OwnerPolicyParams, Request};
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};
    use crate::app::App;
    use crate::owner_policy::test_fixture::{path, Fixture};

    const V1: &str = "[owner.standup]\ntime = \"09:00\"\n";
    const V2: &str = "[owner.standup]\ntime = \"10:30\"\n";

    fn call(
        app: &mut App,
        adopt: Option<Option<&str>>,
        repo: &std::path::Path,
    ) -> serde_json::Value {
        let params = OwnerPolicyParams {
            repo: path(repo).into(),
            policy_version: adopt.flatten().map(str::to_string),
        };
        let method = match adopt {
            Some(_) => Method::OwnerPolicyAdopt(params),
            None => Method::OwnerPolicyShow(params),
        };
        let response = app.handle_api_request(Request {
            id: "t".into(),
            method,
        });
        serde_json::from_str(&response).expect("json response")
    }

    fn operator(app: &mut App) {
        // The test process runs in no pane, so it attests as the operator.
        app.current_api_peer_pid = Some(std::process::id());
    }

    /// ADR-0035 §6.2: a merged change is reported as pending and does not
    /// apply until the operator adopts it. The adoption survives a restart.
    #[tokio::test]
    async fn a_merged_change_is_reported_and_not_applied_until_adopted() {
        let log = temp_log("owner-policy-adopt");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        operator(&mut app);
        let fixture = Fixture::new("owner-policy-adopt", V1);
        let v1 = fixture.upstream_blob();

        let shown = call(&mut app, None, &fixture.clone);
        assert_eq!(shown["result"]["pending"], true, "{shown}");
        assert_eq!(
            shown["result"]["policy"],
            serde_json::json!({}),
            "nothing applies before adoption"
        );

        let adopted = call(&mut app, Some(Some(&v1)), &fixture.clone);
        assert_eq!(adopted["result"]["adopted_now"], true, "{adopted}");
        assert_eq!(adopted["result"]["pending"], false);
        assert_eq!(adopted["result"]["policy"]["standup"]["time"], "09:00");

        fixture.merge(V2);
        let v2 = fixture.upstream_blob();
        let shown = call(&mut app, None, &fixture.clone);
        assert_eq!(shown["result"]["current"], v2.as_str(), "{shown}");
        assert_eq!(shown["result"]["adopted"]["policy_version"], v1.as_str());
        assert_eq!(shown["result"]["pending"], true);
        assert_eq!(
            shown["result"]["policy"]["standup"]["time"], "09:00",
            "v2 is not applied"
        );

        // Adopting a version other than the current one is refused.
        let stale = call(&mut app, Some(Some(&v1)), &fixture.clone);
        assert_eq!(stale["error"]["code"], "owner_policy_changed", "{stale}");

        let adopted = call(&mut app, Some(Some(&v2)), &fixture.clone);
        assert_eq!(
            adopted["result"]["policy"]["standup"]["time"], "10:30",
            "{adopted}"
        );
        let again = call(&mut app, Some(Some(&v2)), &fixture.clone);
        assert_eq!(again["result"]["adopted_now"], false, "{again}");
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        operator(&mut app);
        let shown = call(&mut app, None, &fixture.clone);
        assert_eq!(
            shown["result"]["adopted"]["policy_version"],
            v2.as_str(),
            "{shown}"
        );
        assert_eq!(shown["result"]["pending"], false);
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0035 §6.2 / ADR-0014: adoption is an operator verb. An attested
    /// agent is refused, and so is a caller whose pid is unknown.
    #[tokio::test]
    async fn adoption_is_refused_for_an_agent_and_an_unattested_caller() {
        let log = temp_log("owner-policy-agent");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        let fixture = Fixture::new("owner-policy-agent", V1);
        let v1 = fixture.upstream_blob();

        caller_is_an_agent(&mut app);
        let refused = call(&mut app, Some(Some(&v1)), &fixture.clone);
        assert_eq!(refused["error"]["code"], "operator_only", "{refused}");

        app.current_api_peer_pid = None;
        let refused = call(&mut app, Some(Some(&v1)), &fixture.clone);
        assert_eq!(refused["error"]["code"], "operator_only", "{refused}");

        let shown = call(&mut app, None, &fixture.clone);
        assert!(
            shown["result"]["adopted"].is_null(),
            "refusals record nothing: {shown}"
        );
        assert_eq!(
            crate::cli::refusal_exit("operator_only"),
            crate::cli::exit::POLICY_REFUSED
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0031 §4: an adoption that writes nothing more outlives rotating
    /// past every kept log file.
    #[tokio::test]
    async fn an_adoption_survives_rotating_past_every_kept_file() {
        let log = temp_log("owner-policy-rotation");
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        operator(&mut app);
        let fixture = Fixture::new("owner-policy-rotation", V1);
        let v1 = fixture.upstream_blob();
        call(&mut app, Some(Some(&v1)), &fixture.clone);
        for round in 0..6 {
            hub.rotate_on_next_push();
            app.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::WorkspaceClosed,
                data: crate::api::schema::EventData::WorkspaceClosed {
                    workspace_id: format!("filler-{round}"),
                },
            });
            app.reissue_records_after_rotation();
        }
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        let shown = call(&mut app, None, &fixture.clone);
        assert_eq!(
            shown["result"]["adopted"]["policy_version"],
            v1.as_str(),
            "{shown}"
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }
}
