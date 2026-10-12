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
//! addresses over the mesh); and the binding records no policy reference until
//! adoption exists (O0, #1195).

use std::collections::BTreeMap;

use crate::api::schema::{EventData, EventEnvelope, EventKind, OwnerBinding};

use super::App;

/// Owner bindings by repo slug.
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
                self.bindings.insert(binding.repo.clone(), binding.clone());
            }
            EventData::OwnerUnbound { repo } => {
                self.bindings.remove(repo);
            }
            _ => {}
        }
    }

    pub(crate) fn get(&self, repo: &str) -> Option<&OwnerBinding> {
        self.bindings.get(repo)
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
    /// The one write path for owner bindings: fold the event, then append
    /// it, so the live bindings are always the fold of the log.
    pub(crate) fn record_owner_event(&mut self, event: EventEnvelope) {
        self.owners.apply(&event.data);
        self.emit_event(event);
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
