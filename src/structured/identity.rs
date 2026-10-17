//! Pure UID boundary, ADR-0036 §1a; ancestry supplies agent identity separately.
use crate::platform::peer;
use std::sync::{Arc, Once, OnceLock};

type LookupResult = Result<Option<u32>, ()>;

/// Lazy, process-shared resolution. First use waits at most 100 ms for NSS.
/// after that wait, pending calls return immediately without new workers.
#[derive(Clone)]
pub(crate) struct DedicatedUid(Arc<AccountLookup>);
struct AccountLookup {
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
pub(crate) struct UidPolicy {
    /// Effective UID of the server at construction.
    pub operator: u32,
    /// Cached account result, or Unknown while lazy resolution is pending.
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
        static DEDICATED: OnceLock<DedicatedUid> = OnceLock::new();
        Self {
            operator: peer::effective_uid(),
            dedicated: DEDICATED
                .get_or_init(|| DedicatedUid::lazy(peer::dedicated_uid))
                .clone(),
        }
    }
}
impl UidPolicy {
    /// Constrain the caller role by UID; agent identity still requires ancestry.
    pub fn classify(&self, uid: Option<u32>) -> Principal {
        let (Some(uid), Ok(dedicated)) = (uid, self.dedicated.get()) else {
            return Principal::Unknown;
        };
        if dedicated == Some(uid) {
            Principal::Agent
        } else if uid == self.operator {
            Principal::Operator
        } else {
            Principal::Unknown
        }
    }
}

impl UidPolicy {
    /// ADR-0036 §1a: anonymous/unknown UIDs may read, but cannot bypass the
    /// boundary through raw terminal sends or other state-changing methods.
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
        // is_retry_safe promises reads/previews only. Inbox consumption and
        // cursor/wait lifecycle are reads too, but intentionally not retried.
        if method.is_retry_safe()
            || matches!(
                method,
                ChannelHello(_)
                    | WaitPark(_)
                    | MsgRead(_)
                    | MsgWaitReply(_)
                    | EventsSubscribe(_)
                    | EventsWait(_)
                    | EventsRead(_)
                    | WaitHold(_, _)
                    | WaitRelease(_)
                    | PaneWaitForOutput(_)
                    | PaneStashShow(_)
            )
        {
            return None;
        }
        Some(
            if matches!(
                method,
                MsgSend(_)
                    | MsgReply(_)
                    | MsgWake(_)
                    | AgentSend(_)
                    | PaneSendText(_)
                    | PaneWriteText(_)
                    | PaneWriteKeys(_)
                    | PaneWriteInput(_)
                    | PaneSendInput(_)
                    | PaneSendKeys(_)
                    | PaneSubmit(_)
                    | PaneStashRestore(_)
            ) {
                ErrorCode::MsgNotAllowed
            } else {
                ErrorCode::OperatorOnly
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

    #[test]
    fn default_policy_constructs_shared_unstarted_cache() {
        let (done, result) = mpsc::channel();
        let driver = std::thread::spawn(move || {
            let first = UidPolicy::default();
            let second = UidPolicy::default();
            let proof = (
                Arc::ptr_eq(&first.dedicated.0, &second.dedicated.0),
                first.dedicated.0.started.is_completed(),
                first.dedicated.0.result.get().is_none(),
            );
            drop((first, second));
            let _ = done.send(proof);
        });
        let proof = result
            .recv_timeout(Duration::from_secs(2))
            .expect("default must not enter NSS");
        driver.join().unwrap();
        assert_eq!(proof, (true, false, true));
    }

    #[test]
    fn blocked_account_lookup_does_not_block_app_startup_or_grant_authority() {
        use crate::api::schema::*;
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
            let mut app = crate::app::App::new_with_runtime_config(
                &crate::config::Config::default(),
                true,
                None,
                rx,
                crate::api::EventHub::default(),
                crate::app::AppRuntimeConfig {
                    uid_policy: UidPolicy {
                        operator: 7,
                        dedicated: lookup,
                    },
                    ..Default::default()
                },
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
        lookup_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("classification starts lookup");
        // The lookup remains alive and silent until cleanup: no sleep or retry.
        let (roles, stop, read, quit) = result
            .recv_timeout(Duration::from_secs(2))
            .expect("classification must finish while NSS stays blocked");
        driver.join().unwrap();
        assert!(roles.iter().all(|role| *role == Principal::Unknown));
        assert_eq!(stop["error"]["code"], "operator_only");
        assert!(read.get("result").is_some(), "{read}");
        assert!(!quit);
        assert!(
            lookup_entered.try_recv().is_err(),
            "one worker despite repeated calls"
        );
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
            assert_eq!(policy.refusal(uid, false, &read), None);
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
                    Some(ErrorCode::MsgNotAllowed),
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
            Principal::Unknown
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
