//! Owner bindings (ADR-0035 §1): one resident owner per repo, keyed by the
//! repo's GitHub slug.
//!
//! A binding is a fold over the durable `OwnerBound` / `OwnerUnbound` events,
//! held in memory and folded once per server start in the same log read as
//! the delegation edges and the goals (`App::reissue_open_records`). Every
//! binding is re-issued as an `OwnerBound { reissued: true }` copy on start
//! and after every log rotation, the mechanism ADR-0031 §4 gives goals, so an
//! owner that stays quiet for weeks cannot rotate out.
//!
//! Known limits: the fold is per node, so "one owner per repo fleet-wide" is
//! not enforced across nodes yet (#1234, after O7 #1212 resolves owner
//! addresses over the mesh); and the binding records no policy reference yet.
//! The join point is `App::owner_policies` (O0, #1195), whose adoptions are
//! keyed by the checkout's git common dir rather than the slug: a binding
//! reads its adopted version through `binding.checkout`.

use std::collections::BTreeMap;

use crate::api::schema::{EventData, EventEnvelope, EventKind, OwnerBinding};

use super::App;

/// The key a repo slug folds and looks up under. GitHub resolves
/// `owner/name` case-insensitively, so `Example/Widgets` and
/// `example/widgets` are one repo and one binding. `owner.bind` records the
/// slug in this form too.
pub(crate) fn repo_key(repo: &str) -> String {
    repo.to_ascii_lowercase()
}

/// Owner bindings by repo slug, keyed by [`repo_key`].
#[derive(Debug, Default)]
pub(crate) struct OwnerIndex {
    bindings: BTreeMap<String, OwnerBinding>,
}

impl OwnerIndex {
    /// Apply one event. Both arms set state, so a duplicated line or a
    /// re-issue next to its original folds to the same bindings.
    pub(crate) fn apply(&mut self, data: &EventData) {
        match data {
            EventData::OwnerBound { binding, .. } => {
                self.bindings
                    .insert(repo_key(&binding.repo), binding.clone());
            }
            EventData::OwnerUnbound { repo } => {
                self.bindings.remove(&repo_key(repo));
            }
            _ => {}
        }
    }

    pub(crate) fn get(&self, repo: &str) -> Option<&OwnerBinding> {
        self.bindings.get(&repo_key(repo))
    }

    /// Every binding, ordered by repo slug.
    pub(crate) fn all(&self) -> impl Iterator<Item = &OwnerBinding> {
        self.bindings.values()
    }

    fn reissue_events(&self) -> Vec<EventEnvelope> {
        self.bindings
            .values()
            .map(|binding| EventEnvelope {
                event: EventKind::OwnerBound,
                data: EventData::OwnerBound {
                    binding: binding.clone(),
                    reissued: true,
                },
            })
            .collect()
    }
}

impl App {
    /// Persist and sync an owner mutation before folding it into live state.
    /// Reuse the required-audit writer so persistence failure refuses the effect.
    pub(crate) fn record_owner_event(
        &mut self,
        event: EventEnvelope,
    ) -> Result<(), crate::api::RequiredAuditError> {
        self.event_hub
            .try_push_durable(event.clone())
            .inspect_err(|error| {
                crate::logging::event_log_write_failed(&error.to_string());
            })?;
        self.owners.apply(&event.data);
        self.owner_policies.apply(&event.data);
        Ok(())
    }

    /// Server start: fold the log read the delegation pass already made and
    /// re-issue every binding into the newest file.
    pub(super) fn reissue_owner_bindings(&mut self, restored: &[(u64, u64, EventEnvelope)]) {
        let mut index = OwnerIndex::default();
        for (_, _, envelope) in restored {
            index.apply(&envelope.data);
        }
        self.owners = index;
        self.emit_owner_reissues();
    }

    pub(super) fn emit_owner_reissues(&mut self) {
        for event in self.owners.reissue_events() {
            self.emit_event(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        Method, OwnerBindParams, OwnerHarness, OwnerRepoParams, Request, ResponseResult,
        SuccessResponse,
    };
    use crate::api::EventHub;
    use crate::app::delegation::handler_tests::{app_on, caller_is_an_agent, shutdown, temp_log};

    const REPO: &str = "example/widgets";

    /// A git checkout next to the test's log, so it is cleaned up with it.
    fn checkout(log: &std::path::Path) -> String {
        let checkout = log.parent().expect("log dir").join("checkout");
        std::fs::create_dir_all(checkout.join(".git")).expect("checkout");
        checkout.display().to_string()
    }

    fn bind_params(checkout: &str) -> OwnerBindParams {
        OwnerBindParams {
            repo: REPO.into(),
            checkout: checkout.into(),
            harness: OwnerHarness::Claude,
            model: Some("opus".into()),
            profiles: vec!["main".into(), "spare".into()],
        }
    }

    fn call(app: &mut App, method: Method) -> serde_json::Value {
        let response = app.handle_api_request(Request {
            id: "req".into(),
            method,
        });
        serde_json::from_str(&response).expect(&response)
    }

    fn bind(app: &mut App, params: OwnerBindParams) -> (OwnerBinding, bool) {
        let response = call(app, Method::OwnerBind(params));
        let success: SuccessResponse =
            serde_json::from_value(response.clone()).unwrap_or_else(|_| panic!("{response}"));
        let ResponseResult::OwnerBound { binding, rebound } = success.result else {
            panic!("expected owner_bound: {response}");
        };
        (binding, rebound)
    }

    fn show(app: &mut App) -> serde_json::Value {
        call(
            app,
            Method::OwnerShow(OwnerRepoParams { repo: REPO.into() }),
        )
    }

    fn owner_events(hub: &EventHub) -> Vec<EventData> {
        hub.persisted_events_after(0)
            .into_iter()
            .map(|(_, _, envelope)| envelope.data)
            .filter(|data| {
                matches!(
                    data,
                    EventData::OwnerBound { .. } | EventData::OwnerUnbound { .. }
                )
            })
            .collect()
    }

    fn sample(repo: &str) -> OwnerBinding {
        OwnerBinding {
            repo: repo.into(),
            node: "node-a".into(),
            agent_id: format!("agent_{}", repo.replace('/', "_")),
            checkout: "/srv/checkout".into(),
            harness: OwnerHarness::Codex,
            model: None,
            profiles: vec![],
            since_ms: 1,
        }
    }

    fn bound(binding: OwnerBinding) -> EventData {
        EventData::OwnerBound {
            binding,
            reissued: false,
        }
    }

    /// Start at each public verb and rebuild state from reopened journal files.
    #[tokio::test]
    async fn audit_failures_reconstruct_only_committed_mutations() {
        use crate::api::{error_code::ErrorCode, AuditFault};
        use crate::owner_policy::test_fixture::{path, Fixture};
        let fixture = Fixture::new(
            "owner-audit-replay",
            r#"[owner.standup]
time = "09:00"
"#,
        );
        let version = fixture.upstream_blob();
        for fault in [
            AuditFault::PartialWrite,
            AuditFault::FileSync,
            AuditFault::DirectorySync,
            AuditFault::Rollback,
        ] {
            for rotate in [false, true] {
                for verb in ["bind", "rebind", "unbind", "policy", "delegate"] {
                    let log = temp_log("owner-audit-replay");
                    let hub = EventHub::with_persistence(log.clone());
                    let mut app = app_on(hub.clone());
                    let agent = caller_is_an_agent(&mut app);
                    app.current_api_peer_pid = Some(std::process::id());
                    app.current_api_peer_birth =
                        Some(crate::platform::process_start_time(std::process::id()));
                    let checkout = checkout(&log);
                    let before = (verb != "bind").then(|| bind(&mut app, bind_params(&checkout)).0);
                    let policy_request =
                        Method::OwnerPolicyAdopt(crate::api::schema::OwnerPolicyParams {
                            repo: path(&fixture.clone).into(),
                            policy_version: Some(version.clone()),
                        });
                    let delegate_request: Method = serde_json::from_value(serde_json::json!({
                        "method":"delegate.rebind", "params":{"name":"fixture", "agent_id":agent, "previous_agent_id":null}
                    })).unwrap();
                    let mut params = bind_params(&checkout);
                    params.harness = OwnerHarness::Codex;
                    let request = match verb {
                        "bind" | "rebind" => Method::OwnerBind(params),
                        "unbind" => Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() }),
                        "policy" => policy_request.clone(),
                        "delegate" => delegate_request.clone(),
                        _ => unreachable!(),
                    };
                    let before_bytes = std::fs::read(&log).unwrap();
                    let before_count = hub.persisted_events_after(0).len();
                    let before_cursor = hub.current_sequence();
                    if rotate {
                        hub.rotate_on_next_push();
                    }
                    hub.fail_audit_for_test(fault);
                    let uncertain = fault == AuditFault::Rollback;
                    let expected = if uncertain {
                        "audit_indeterminate"
                    } else if verb == "delegate" {
                        "delegate_audit_unavailable"
                    } else {
                        "audit_unavailable"
                    };
                    let response = call(&mut app, request.clone());
                    assert_eq!(
                        response["error"]["code"], expected,
                        "{verb} {fault:?} rotate={rotate}: {response}"
                    );
                    assert!(response.get("result").is_none());
                    assert_eq!(app.owners.get(REPO), before.as_ref());
                    let shown = call(
                        &mut app,
                        Method::OwnerPolicyShow(crate::api::schema::OwnerPolicyParams {
                            repo: path(&fixture.clone).into(),
                            policy_version: None,
                        }),
                    );
                    assert!(shown["result"]["adopted"].is_null(), "{shown}");
                    if uncertain {
                        assert!(response["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains("may or may not take effect after restart"));
                        assert_eq!(
                            ErrorCode::AuditIndeterminate.retry().as_str(),
                            "after_inspecting"
                        );
                        assert_eq!(
                            ErrorCode::AuditIndeterminate.exit(),
                            crate::cli::exit::REQUEST_OUTCOME_UNKNOWN
                        );
                        for later in [
                            request,
                            Method::OwnerBind(bind_params(&checkout)),
                            policy_request,
                            delegate_request,
                        ] {
                            let response = call(&mut app, later);
                            assert_eq!(
                                response["error"]["code"], "audit_indeterminate",
                                "{response}"
                            );
                        }
                    } else {
                        assert_eq!(call(&mut app, request)["error"]["code"], expected);
                        assert_eq!(
                            std::fs::read(&log).unwrap(),
                            if rotate { vec![] } else { before_bytes.clone() }
                        );
                        if rotate {
                            assert_eq!(
                                std::fs::read(log.with_file_name("event-log.1.jsonl")).unwrap(),
                                before_bytes
                            );
                        }
                    }
                    assert_eq!(hub.rotations(), u64::from(rotate));
                    assert_eq!(app.owners.get(REPO), before.as_ref());
                    assert!(!hub
                        .events_after(before_cursor)
                        .unwrap_or_default()
                        .iter()
                        .any(|(_, e)| matches!(
                            e.event,
                            EventKind::OwnerBound
                                | EventKind::OwnerUnbound
                                | EventKind::OwnerPolicyAdopted
                                | EventKind::DelegateRebindAuthorized
                        )));

                    shutdown(app);
                    drop(hub);
                    let hub = EventHub::with_persistence(log.clone());
                    let history = hub.persisted_events_after(0);
                    assert_eq!(
                        history.len(),
                        before_count + usize::from(uncertain),
                        "{verb} {fault:?} rotate={rotate}"
                    );
                    assert_eq!(
                        history
                            .iter()
                            .filter(|(_, _, e)| e.event == EventKind::DelegateRebindAuthorized)
                            .count(),
                        usize::from(uncertain && verb == "delegate")
                    );
                    let mut app = app_on(hub.clone());
                    app.reissue_open_records();
                    let owner = app.owners.get(REPO);
                    match (uncertain, verb) {
                        (true, "bind" | "rebind") => {
                            assert_eq!(owner.unwrap().harness, OwnerHarness::Codex)
                        }
                        (true, "unbind") => assert!(owner.is_none()),
                        _ => assert_eq!(owner, before.as_ref()),
                    }
                    let shown = call(
                        &mut app,
                        Method::OwnerPolicyShow(crate::api::schema::OwnerPolicyParams {
                            repo: path(&fixture.clone).into(),
                            policy_version: None,
                        }),
                    );
                    if uncertain && verb == "policy" {
                        assert_eq!(shown["result"]["adopted"]["policy_version"], version);
                        assert_eq!(shown["result"]["policy"]["standup"]["time"], "09:00");
                    } else {
                        assert!(shown["result"]["adopted"].is_null(), "{shown}");
                    }
                    app.current_api_peer_pid = Some(std::process::id());
                    assert!(call(&mut app, Method::OwnerBind(bind_params(&checkout)))
                        .get("result")
                        .is_some());
                    shutdown(app);
                    drop(hub);
                    std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn refused_unbind_never_reappears_on_restart_after_directory_repair() {
        let log = temp_log("owner-refusal-restart");
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.current_api_peer_pid = Some(std::process::id());
        let (before, _) = bind(&mut app, bind_params(&checkout(&log)));
        let relocated = log.parent().unwrap().with_extension("relocated");
        std::fs::rename(log.parent().unwrap(), &relocated).unwrap();
        let refused = call(
            &mut app,
            Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() }),
        );
        assert_eq!(refused["error"]["code"], "audit_unavailable");
        shutdown(app);
        std::fs::rename(&relocated, log.parent().unwrap()).unwrap();
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(
            app.owners.get(REPO),
            Some(&before),
            "a refused unbind must not replay"
        );
        shutdown(app);
        std::fs::remove_dir_all(log.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn bind_refuses_failed_audit_before_changing_the_binding() {
        for (sync_failure, prebound) in [(false, false), (true, false), (false, true), (true, true)]
        {
            let log = temp_log("owner-bind-audit");
            let hub = EventHub::with_persistence(log.clone());
            let mut app = app_on(hub.clone());
            app.current_api_peer_pid = Some(std::process::id());
            let checkout = checkout(&log);
            let before = prebound.then(|| bind(&mut app, bind_params(&checkout)).0);
            let relocated = log.parent().unwrap().with_extension("relocated");
            let checkout = if sync_failure {
                std::fs::rename(log.parent().unwrap(), &relocated).unwrap();
                relocated.join("checkout").display().to_string()
            } else {
                hub.fail_appends_for_test();
                checkout
            };
            let mut params = bind_params(&checkout);
            params.harness = OwnerHarness::Codex;
            for _ in 0..2 {
                let response = call(&mut app, Method::OwnerBind(params.clone()));
                assert_eq!(response["error"]["code"], "audit_unavailable", "{response}");
                assert!(response.get("result").is_none());
                assert_eq!(app.owners.get(REPO), before.as_ref());
            }
            assert_eq!(
                hub.events_after(0)
                    .unwrap_or_default()
                    .iter()
                    .filter(|(_, e)| matches!(
                        e.event,
                        EventKind::OwnerBound | EventKind::OwnerUnbound
                    ))
                    .count(),
                usize::from(prebound)
            );
            shutdown(app);
            std::fs::remove_dir_all(if sync_failure {
                relocated
            } else {
                log.parent().unwrap().into()
            })
            .unwrap();
        }
    }

    #[tokio::test]
    async fn unbind_refuses_failed_audit_before_removing_the_binding() {
        for sync_failure in [false, true] {
            let log = temp_log("owner-unbind-audit");
            let hub = EventHub::with_persistence(log.clone());
            let mut app = app_on(hub.clone());
            app.current_api_peer_pid = Some(std::process::id());
            let (before, _) = bind(&mut app, bind_params(&checkout(&log)));
            let relocated = log.parent().unwrap().with_extension("relocated");
            if sync_failure {
                std::fs::rename(log.parent().unwrap(), &relocated).unwrap();
            } else {
                hub.fail_appends_for_test();
            }
            for _ in 0..2 {
                let response = call(
                    &mut app,
                    Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() }),
                );
                assert_eq!(response["error"]["code"], "audit_unavailable", "{response}");
                assert!(response.get("result").is_none());
                assert_eq!(app.owners.get(REPO), Some(&before));
            }
            assert_eq!(
                hub.events_after(0)
                    .unwrap_or_default()
                    .iter()
                    .filter(|(_, e)| matches!(
                        e.event,
                        EventKind::OwnerBound | EventKind::OwnerUnbound
                    ))
                    .count(),
                1
            );
            shutdown(app);
            std::fs::remove_dir_all(if sync_failure {
                relocated
            } else {
                log.parent().unwrap().into()
            })
            .unwrap();
        }
    }

    #[test]
    fn the_fold_keys_bindings_by_repo_and_an_unbind_removes_one() {
        let mut index = OwnerIndex::default();
        let mut rebound = sample("a/one");
        rebound.checkout = "/srv/moved".into();
        let events = [
            bound(sample("a/one")),
            bound(sample("a/two")),
            bound(rebound.clone()),
            EventData::OwnerUnbound {
                repo: "a/two".into(),
            },
            EventData::OwnerUnbound {
                repo: "a/never".into(),
            },
        ];
        for data in &events {
            index.apply(data);
        }
        assert_eq!(index.all().collect::<Vec<_>>(), [&rebound]);

        // A duplicated line, and the re-issue next to its original, fold to
        // the same bindings.
        for data in events.iter().chain(&events) {
            index.apply(data);
        }
        for envelope in index.reissue_events() {
            index.apply(&envelope.data);
        }
        assert_eq!(index.all().collect::<Vec<_>>(), [&rebound]);
    }

    /// ADR-0035 §1 acceptance: bind, restart the server, force rotations
    /// past `MAX_ROTATED`, and the binding survives with its `AgentId`.
    #[tokio::test]
    async fn a_binding_survives_a_restart_and_rotating_past_every_kept_file() {
        let log = temp_log("owner-survives");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.current_api_peer_pid = Some(std::process::id());
        let (binding, rebound) = bind(&mut app, bind_params(&checkout));
        assert!(!rebound);
        assert!(crate::terminal::AgentId::is_well_formed(&binding.agent_id));
        assert_eq!(binding.node, crate::app::short_host_name());
        assert_eq!(binding.profiles, ["main", "spare"]);
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        assert_eq!(
            app.owners.get(REPO),
            None,
            "folded at start, not in App::new"
        );
        app.reissue_open_records();
        assert_eq!(app.owners.get(REPO), Some(&binding));

        // One more rotation than the log keeps files (MAX_ROTATED = 4), so
        // every line written before the first one is gone.
        let hub = app.event_hub.clone();
        for round in 0..6 {
            hub.rotate_on_next_push();
            app.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id: format!("filler-{round}"),
                },
            });
            app.reissue_records_after_rotation();
        }
        assert_eq!(hub.rotations(), 6);
        assert!(
            !owner_events(&hub).iter().any(|data| matches!(
                data,
                EventData::OwnerBound {
                    reissued: false,
                    ..
                }
            )),
            "the original bind has rotated out"
        );
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(app.owners.get(REPO), Some(&binding));
        assert_eq!(
            show(&mut app)["result"]["binding"]["agent_id"],
            *binding.agent_id
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// The owner keeps one `AgentId` for life (ADR-0035 §1): binding a bound
    /// repo again replaces the other fields and keeps the id and `since_ms`.
    /// An unbind ends the binding durably.
    #[tokio::test]
    async fn a_rebind_keeps_the_agent_id_and_an_unbind_ends_the_binding() {
        let log = temp_log("owner-rebind");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.current_api_peer_pid = Some(std::process::id());
        let (first, _) = bind(&mut app, bind_params(&checkout));
        let mut params = bind_params(&checkout);
        params.harness = OwnerHarness::Opencode;
        params.profiles = vec![];
        let (second, rebound) = bind(&mut app, params);
        assert!(rebound);
        assert_eq!(
            (second.agent_id.as_str(), second.since_ms, second.harness),
            (
                first.agent_id.as_str(),
                first.since_ms,
                OwnerHarness::Opencode
            )
        );
        let listed = call(&mut app, Method::OwnerList(Default::default()));
        assert_eq!(
            listed["result"]["bindings"].as_array().map(Vec::len),
            Some(1)
        );

        let unbound = call(
            &mut app,
            Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() }),
        );
        assert_eq!(unbound["result"]["binding"]["agent_id"], *first.agent_id);
        assert_eq!(show(&mut app)["error"]["code"], "not_found");
        assert_eq!(
            owner_events(&hub).last(),
            Some(&EventData::OwnerUnbound { repo: REPO.into() })
        );
        shutdown(app);

        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(app.owners.get(REPO), None);
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// ADR-0035 §1 acceptance: binding is an operator verb, refused for an
    /// attested agent caller (the ADR-0014 boundary). Nothing is recorded.
    #[tokio::test]
    async fn an_attested_agent_cannot_bind_or_unbind_and_can_read() {
        let log = temp_log("owner-agent");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.current_api_peer_pid = Some(std::process::id());
        let (binding, _) = bind(&mut app, bind_params(&checkout));
        caller_is_an_agent(&mut app);

        let mut params = bind_params(&checkout);
        params.repo = "example/other".into();
        let refused = call(&mut app, Method::OwnerBind(params));
        assert_eq!(refused["error"]["code"], "operator_only", "{refused}");
        let refused = call(
            &mut app,
            Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() }),
        );
        assert_eq!(refused["error"]["code"], "operator_only", "{refused}");
        assert_eq!(owner_events(&hub).len(), 1, "only the operator's bind");
        assert_eq!(
            show(&mut app)["result"]["binding"]["agent_id"],
            *binding.agent_id
        );
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// The operator check fails closed: a caller whose pid the server could
    /// not read, and a request relayed from an enrolled mesh peer (an inbound
    /// relay edge on the caller's pid), are refused like an agent, and nothing is
    /// recorded. Neither has an agent pane in its ancestry, so without the
    /// check each would pass as the operator.
    #[tokio::test]
    async fn an_unattested_or_relayed_caller_cannot_bind_or_unbind() {
        let log = temp_log("owner-unattested");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        let pid = std::process::id();
        app.current_api_peer_pid = Some(pid);
        bind(&mut app, bind_params(&checkout));
        let refusals = |app: &mut App| {
            let unbind = Method::OwnerUnbind(OwnerRepoParams { repo: REPO.into() });
            [Method::OwnerBind(bind_params(&checkout)), unbind]
                .map(|method| call(app, method)["error"]["code"].clone())
        };

        app.current_api_peer_pid = None;
        assert_eq!(refusals(&mut app), ["operator_only", "operator_only"]);

        app.current_api_peer_pid = Some(pid);
        let started = crate::platform::process_start_time(pid).expect("own start time");
        app.inbound
            .attach(pid, started, crate::platform::process_start_time)
            .expect("relay edge");
        // An enrolled relay: an unenrolled one is refused earlier, with
        // `mesh_not_enrolled`, before any method runs.
        let edge = app
            .inbound
            .edge_mut(Some(pid), crate::platform::process_start_time)
            .expect("relay edge");
        edge.enrollment.state = "pinned".into();
        edge.enrollment.node_id = Some("node-relay.example".into());
        assert_eq!(refusals(&mut app), ["operator_only", "operator_only"]);

        assert_eq!(owner_events(&hub).len(), 1, "only the operator's bind");
        assert!(app.owners.get(REPO).is_some());
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    /// GitHub slugs are case-insensitive, so two spellings are one binding:
    /// a rebind in another case keeps the `AgentId`, both spellings read it,
    /// and an unbind in either case ends it, in memory and after a restart.
    #[tokio::test]
    async fn a_slug_in_another_case_is_the_same_binding() {
        let log = temp_log("owner-case");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.current_api_peer_pid = Some(std::process::id());
        let mut params = bind_params(&checkout);
        params.repo = "Example/Widgets".into();
        let (first, rebound) = bind(&mut app, params);
        assert!(!rebound);
        assert_eq!(first.repo, REPO, "recorded in lowercase");
        let (second, rebound) = bind(&mut app, bind_params(&checkout));
        assert!(rebound);
        assert_eq!(second.agent_id, first.agent_id);
        let shown = call(
            &mut app,
            Method::OwnerShow(OwnerRepoParams {
                repo: "EXAMPLE/widgets".into(),
            }),
        );
        assert_eq!(shown["result"]["binding"]["agent_id"], *first.agent_id);
        let listed = call(&mut app, Method::OwnerList(Default::default()));
        assert_eq!(
            listed["result"]["bindings"].as_array().map(Vec::len),
            Some(1)
        );

        // A fold over differently-cased lines lands on one key as well.
        let mut index = OwnerIndex::default();
        let mut upper = sample("Example/Widgets");
        upper.agent_id = first.agent_id.clone();
        index.apply(&bound(upper));
        index.apply(&bound(sample(REPO)));
        assert_eq!(index.all().count(), 1);
        index.apply(&EventData::OwnerUnbound {
            repo: "EXAMPLE/WIDGETS".into(),
        });
        assert_eq!(index.all().count(), 0);

        call(
            &mut app,
            Method::OwnerUnbind(OwnerRepoParams {
                repo: "example/WIDGETS".into(),
            }),
        );
        assert_eq!(app.owners.get("Example/Widgets"), None);
        shutdown(app);
        let mut app = app_on(EventHub::with_persistence(log.clone()));
        app.reissue_open_records();
        assert_eq!(app.owners.all().count(), 0);
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }

    #[tokio::test]
    async fn bind_refuses_a_path_for_a_slug_and_a_relative_or_missing_checkout() {
        let log = temp_log("owner-invalid");
        let checkout = checkout(&log);
        let hub = EventHub::with_persistence(log.clone());
        let mut app = app_on(hub.clone());
        app.current_api_peer_pid = Some(std::process::id());
        let cases: [(&str, String, Option<&str>); 6] = [
            ("/srv/widgets", checkout.clone(), None),
            ("example/../x", checkout.clone(), None),
            ("example", checkout.clone(), None),
            (REPO, "relative/checkout".into(), None),
            (REPO, format!("{checkout}/missing"), None),
            (REPO, checkout.clone(), Some(" ")),
        ];
        for (repo, checkout, model) in cases {
            let mut params = bind_params(&checkout);
            params.repo = repo.into();
            params.model = model.map(str::to_string);
            let refused = call(&mut app, Method::OwnerBind(params));
            assert_eq!(
                refused["error"]["code"], "invalid_params",
                "{repo} {checkout}"
            );
        }
        assert!(owner_events(&hub).is_empty());
        shutdown(app);
        let _ = std::fs::remove_dir_all(log.parent().expect("log dir"));
    }
}
