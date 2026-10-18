//! UID boundary, ADR-0036 §1a; ancestry supplies agent identity separately.
use crate::platform::peer;
use std::sync::{Arc, Once, OnceLock};

type LookupResult = Result<Option<u32>, ()>;

/// Lazy, server-owned resolution shared by its app and API listener.
/// First use waits at most 100 ms for NSS.
/// after that wait, pending calls return immediately without new workers.
#[derive(Clone)]
pub(crate) struct DedicatedUid(Arc<AccountLookup>);
struct AccountLookup {
    collision_reported: Once,
    started: Once,
    result: Arc<OnceLock<LookupResult>>,
    resolve: Arc<dyn Fn() -> std::io::Result<Option<u32>> + Send + Sync>,
}
impl DedicatedUid {
    /// Construct an unstarted lookup, without calling directory services.
    pub(crate) fn lazy(
        resolve: impl Fn() -> std::io::Result<Option<u32>> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(AccountLookup {
            collision_reported: Once::new(),
            started: Once::new(),
            result: Arc::new(OnceLock::new()),
            resolve: Arc::new(resolve),
        }))
    }
    fn get(&self) -> LookupResult {
        if let Some(result) = self.0.result.get() {
            return *result;
        }
        self.0.started.call_once(|| {
            let result = self.0.result.clone();
            let resolve = self.0.resolve.clone();
            let (done, completed) = std::sync::mpsc::channel();
            // NSS may never return. Detach one worker, never join or respawn it.
            if let Err(error) =
                std::thread::Builder::new()
                    .name("peer-uid".into())
                    .spawn(move || {
                        let value = resolve().map_err(|error| {
                            tracing::warn!("structured account lookup failed: {error}");
                        });
                        let _ = result.set(value);
                        let _ = done.send(());
                    })
            {
                tracing::warn!("structured account lookup worker failed: {error}");
                let _ = self.0.result.set(Err(()));
            }
            // Like the health probe, leave hung NSS off the caller thread.
            let _ = completed.recv_timeout(std::time::Duration::from_millis(100));
        });
        self.0.result.get().copied().unwrap_or(Err(()))
    }
}
impl From<LookupResult> for DedicatedUid {
    fn from(value: LookupResult) -> Self {
        let lookup = Self::lazy(peer::dedicated_uid);
        let _ = lookup.0.result.set(value);
        lookup
    }
}

/// Server-owned account configuration, independent of request claims.
#[derive(Clone)]
pub(crate) struct UidPolicy {
    /// Effective UID of the server at construction.
    pub operator: u32,
    /// Cached account result, or Unknown while configured resolution is pending.
    /// Explicit `security.agent_account = "none"` installs `Ok(None)` at startup,
    /// bypassing NSS entirely. `run_as = "operator"` never disables recognition.
    pub dedicated: DedicatedUid,
}
/// Local socket UID classification. Unknown is never operator authority.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Principal {
    /// Operator UID, subject to independent agent ancestry checks.
    Operator,
    /// Dedicated agent UID, including callers without known ancestry.
    Agent,
    /// Missing, unrecognized, or unreadable credentials.
    Unknown,
}
impl Default for UidPolicy {
    fn default() -> Self {
        Self {
            operator: peer::effective_uid(),
            dedicated: DedicatedUid::lazy(peer::dedicated_uid),
        }
    }
}
impl UidPolicy {
    /// This server's confirmed collision, without starting or waiting for NSS.
    /// The immutable cached result retains the diagnostic until server restart.
    pub(crate) fn account_diagnostic(&self) -> Option<crate::api::schema::AgentAccountDiagnostic> {
        self.report_cached_collision();
        (self.dedicated.0.result.get() == Some(&Ok(Some(self.operator)))).then_some(
            crate::api::schema::AgentAccountDiagnostic::AgentAccountUidCollision {
                uid: self.operator,
            },
        )
    }

    /// Publish a completed collision without starting or waiting for NSS.
    fn report_cached_collision(&self) {
        if self.dedicated.0.result.get() == Some(&Ok(Some(self.operator))) {
            self.dedicated.0.collision_reported.call_once(|| {
                tracing::warn!(code = "agent_account_uid_collision", uid = self.operator,
                    "flk-agent UID equals server UID: ADR-0036 section 1a misconfiguration; operator authority refused");
            });
        }
    }

    /// Constrain the caller role by UID; agent identity still requires ancestry.
    /// Same-UID authority does not depend on directory-service availability.
    /// A confirmed dedicated-account collision still fails closed (#1944).
    pub fn classify(&self, uid: Option<u32>) -> Principal {
        let Some(uid) = uid else {
            return Principal::Unknown;
        };
        let dedicated = self.dedicated.get();
        self.report_cached_collision();
        if dedicated == Ok(Some(uid)) {
            Principal::Agent
        } else if uid == self.operator {
            Principal::Operator
        } else {
            Principal::Unknown
        }
    }
}

impl UidPolicy {
    /// ADR-0036 §1a: unattested callers have no target or agent-directory access.
    /// Only the untargeted health/capability ping remains available. Retry safety
    /// grants no authority; future methods fail closed until admission is decided.
    pub fn refusal(
        &self,
        uid: Option<u32>,
        attributed: bool,
        method: &crate::api::schema::Method,
    ) -> Option<crate::api::error_code::ErrorCode> {
        use crate::api::error_code::ErrorCode;
        use crate::api::schema::Method::*;
        match self.classify(uid) {
            Principal::Operator => return None,
            Principal::Agent if attributed => return None,
            _ => (),
        }
        if matches!(method, Ping(_)) {
            return None;
        }

        Some(
            if matches!(
                method,
                ServerStop(_)
                    | ServerReloadConfig(_)
                    | ServerLiveHandoff(_)
                    | ClientTokenAdd(_)
                    | ClientTokenRemove(_)
                    | ClientTokenList(_)
                    | ClientOpen(_)
            ) {
                ErrorCode::OperatorOnly
            } else {
                ErrorCode::NotAuthorized
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct ReleaseLookup(Option<mpsc::Sender<()>>);
    impl Drop for ReleaseLookup {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }

    #[tokio::test]
    async fn app_and_listener_keep_the_same_policy_instance() {
        use crate::config::model::AgentAccount;
        for account in [AgentAccount::FlkAgent, AgentAccount::None] {
            let mut config = crate::config::Config::default();
            config.security.agent_account = account;
            let runtime_config = crate::app::AppRuntimeConfig::for_config(&config);
            let listener_policy = runtime_config.uid_policy.clone();
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let app = crate::app::App::new_with_runtime_config(
                &config,
                true,
                None,
                rx,
                crate::api::EventHub::default(),
                runtime_config,
            );
            assert!(
                Arc::ptr_eq(
                    &app.runtime_config.uid_policy.dedicated.0,
                    &listener_policy.dedicated.0
                ),
                "app and listener must share their configured cache for {account:?}"
            );
            assert_eq!(
                app.runtime_config.uid_policy.operator,
                listener_policy.operator
            );
            assert!(!listener_policy.dedicated.0.started.is_completed());
            if account == AgentAccount::None {
                assert_eq!(listener_policy.dedicated.get(), Ok(None));
            }
        }
    }

    #[test]
    fn policy_construction_leaves_its_owned_cache_unstarted() {
        let (done, result) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let lookup = DedicatedUid::lazy(|| panic!("construction must not enter NSS"));
            let policy = UidPolicy {
                operator: 7,
                dedicated: lookup.clone(),
            };
            let proof = (
                Arc::ptr_eq(&policy.dedicated.0, &lookup.0),
                lookup.0.started.is_completed(),
                lookup.0.result.get().is_none(),
            );
            drop((policy, lookup));
            let _ = done.send(proof);
        });
        let proof = result
            .recv_timeout(Duration::from_secs(2))
            .expect("construction must not enter NSS");
        driver.join().unwrap();
        assert_eq!(proof, (true, false, true));
    }

    #[test]
    fn blocked_account_lookup_keeps_same_uid_authority_without_blocking_startup() {
        exercise_blocked_lookup(crate::config::model::AgentAccount::FlkAgent);
    }

    #[test]
    fn explicit_no_account_allows_operator_stop_without_entering_hung_lookup() {
        exercise_blocked_lookup(crate::config::model::AgentAccount::None);
    }

    fn exercise_blocked_lookup(account: crate::config::model::AgentAccount) {
        use crate::api::schema::*;
        let recognition = account == crate::config::model::AgentAccount::FlkAgent;
        let (release, held) = mpsc::channel();
        let release = ReleaseLookup(Some(release));
        let held = std::sync::Mutex::new(held);
        let (entered, lookup_entered) = mpsc::channel();
        let (started, startup) = mpsc::channel();
        let (go, proceed) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let lookup = DedicatedUid::lazy(move || {
                entered.send(()).unwrap();
                held.lock().unwrap().recv().unwrap();
                Ok(Some(42))
            });

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _guard = runtime.enter();
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut config = crate::config::Config::default();
            config.security.agent_account = account;
            config.structured.run_as = crate::structured::launch::RunAs::Operator;
            let mut app = crate::app::App::new_with_runtime_config(
                &config,
                true,
                None,
                rx,
                crate::api::EventHub::default(),
                crate::app::AppRuntimeConfig::for_config_with_uid_policy(
                    &config,
                    UidPolicy {
                        operator: 7,
                        dedicated: lookup,
                    },
                ),
            );
            started
                .send(app.runtime_config.uid_policy.dedicated.clone())
                .unwrap();
            proceed
                .recv_timeout(Duration::from_secs(2))
                .expect("test starts classification");
            app.current_api_peer_pid = Some(std::process::id());
            app.current_api_peer_birth =
                Some(crate::platform::process_start_time(std::process::id()));
            app.current_api_peer_uid = Some(Some(7));
            let roles: Vec<_> = [Some(7), Some(42), Some(0), Some(99), None]
                .into_iter()
                .map(|uid| app.runtime_config.uid_policy.classify(uid))
                .collect();
            let stop: serde_json::Value = serde_json::from_str(&app.handle_api_request(Request {
                id: "stop".into(),
                method: Method::ServerStop(EmptyParams {}),
            }))
            .unwrap();
            let read: serde_json::Value = serde_json::from_str(&app.handle_api_request(Request {
                id: "read".into(),
                method: Method::WorkspaceList(EmptyParams {}),
            }))
            .unwrap();
            let quit = app.state.should_quit;
            drop(app);
            drop(_guard);
            drop(runtime);
            finished.send((roles, stop, read, quit)).unwrap();
        });
        let cached = startup
            .recv_timeout(Duration::from_secs(2))
            .expect("startup must not enter NSS");
        assert!(
            lookup_entered.try_recv().is_err(),
            "startup must leave lookup lazy"
        );
        go.send(()).unwrap();
        if recognition {
            lookup_entered
                .recv_timeout(Duration::from_secs(2))
                .expect("configured classification starts lookup");
        }
        // The lookup remains alive and silent until cleanup: no sleep or retry.
        let (roles, stop, read, quit) = result
            .recv_timeout(Duration::from_secs(2))
            .expect("classification must finish while NSS stays blocked");
        driver.join().unwrap();
        assert_eq!(roles[0], Principal::Operator);
        assert!(roles[1..].iter().all(|role| *role == Principal::Unknown));
        assert!(stop.get("result").is_some(), "{stop}");
        assert!(read.get("result").is_some(), "{read}");
        assert!(quit);
        assert!(
            lookup_entered.try_recv().is_err(),
            "one worker despite repeated calls"
        );
        if !recognition {
            assert!(
                !cached.0.started.is_completed(),
                "disabled policy never starts NSS"
            );
            assert_eq!(cached.get(), Ok(None));
            return;
        }
        let (published, ready) = mpsc::channel();
        let waiter_cache = cached.clone();
        let waiter = std::thread::spawn(move || {
            let _ = published.send(*waiter_cache.0.result.wait());
        });
        drop(release);
        assert_eq!(
            ready
                .recv_timeout(Duration::from_secs(2))
                .expect("late result cached"),
            Ok(Some(42))
        );
        waiter.join().unwrap();
        assert_eq!(cached.get(), Ok(Some(42)));
    }

    #[test]
    fn lazy_lookup_caches_absence_and_errors_without_repeating_work() {
        for value in [Ok(None), Err(())] {
            let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let worker_count = count.clone();
            let lookup = DedicatedUid::lazy(move || {
                worker_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                value.map_err(|()| std::io::Error::other("fixture directory failure"))
            });
            let (done, result) = mpsc::channel();
            let cache = lookup.clone();
            let waiter = std::thread::spawn(move || {
                let _ = cache.get();
                let _ = done.send(*cache.0.result.wait());
            });
            assert_eq!(
                result
                    .recv_timeout(Duration::from_secs(2))
                    .expect("lookup publishes"),
                value
            );
            waiter.join().unwrap();
            for _ in 0..3 {
                assert_eq!(lookup.get(), value);
            }
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn anonymous_policy_separates_reads_sends_and_other_mutations() {
        use crate::api::{error_code::ErrorCode, schema::*};
        let policy = UidPolicy {
            operator: 7,
            dedicated: Ok(Some(42)).into(),
        };
        let read = Method::WorkspaceList(EmptyParams {});
        let stop = Method::ServerStop(EmptyParams {});
        for uid in [Some(42), Some(99), None] {
            assert_eq!(
                policy.refusal(uid, false, &read),
                Some(ErrorCode::NotAuthorized)
            );
            assert_eq!(
                policy.refusal(uid, false, &Method::Ping(PingParams::default())),
                None
            );
            assert_eq!(
                policy.refusal(uid, false, &stop),
                Some(ErrorCode::OperatorOnly)
            );
            for method in [
                "pane.send_text",
                "pane.write_text",
                "pane.send_keys",
                "pane.write_keys",
                "pane.send_input",
                "pane.write_input",
                "pane.submit",
            ] {
                let params = if method.ends_with("_keys") {
                    serde_json::json!({"pane_id": "fixture-pane", "keys": ["Enter"]})
                } else {
                    serde_json::json!({"pane_id": "fixture-pane", "text": "fixture"})
                };
                let request: Request = serde_json::from_value(serde_json::json!({
                    "id": "uid", "method": method,
                    "params": params
                }))
                .unwrap();
                assert_eq!(
                    policy.refusal(uid, false, &request.method),
                    Some(ErrorCode::NotAuthorized),
                    "{method}"
                );
            }
        }
        assert_eq!(policy.refusal(Some(7), false, &stop), None);
        assert_eq!(policy.refusal(Some(42), true, &stop), None);
        assert_eq!(
            policy.refusal(None, true, &stop),
            Some(ErrorCode::OperatorOnly)
        );
    }

    #[test]
    fn same_uid_survives_failed_and_pending_account_lookup() {
        use crate::api::schema::{EmptyParams, Method};
        let pending = DedicatedUid::lazy(|| panic!("lookup already started"));
        pending.0.started.call_once(|| ());
        for lookup in [DedicatedUid::from(Err(())), pending] {
            let policy = UidPolicy {
                operator: 7,
                dedicated: lookup,
            };
            assert_eq!(policy.classify(Some(7)), Principal::Operator);
            assert_eq!(
                policy.refusal(Some(7), false, &Method::AgentList(EmptyParams {})),
                None
            );
        }
    }

    #[test]
    fn uid_boundary_never_promotes_anonymous_or_unknown_credentials() {
        let policy = UidPolicy {
            operator: 7,
            dedicated: Ok(Some(42)).into(),
        };
        for (uid, expected) in [
            (Some(7), Principal::Operator),
            (Some(42), Principal::Agent),
            (Some(99), Principal::Unknown),
            (Some(0), Principal::Unknown),
            (None, Principal::Unknown),
        ] {
            assert_eq!(policy.classify(uid), expected);
        }
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Err(()).into()
            }
            .classify(Some(7)),
            Principal::Operator
        );
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Ok(None).into()
            }
            .classify(Some(7)),
            Principal::Operator
        );
        assert_eq!(
            UidPolicy {
                operator: 7,
                dedicated: Ok(Some(7)).into()
            }
            .classify(Some(7)),
            Principal::Agent
        );
    }
}
