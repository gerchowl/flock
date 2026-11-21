use super::*;

#[tokio::test]
async fn stalled_replay_returns_typed_busy_without_releasing_its_lease() {
    use crate::web::wire::commands::Ownership;
    for mode in 0..3 {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let data = root.path().to_owned();
        let prepared = if mode == 0 {
            None
        } else {
            let core = Core::new_shared(
                data.clone(),
                Arc::new(tokio::sync::Notify::new()),
                Owner::default(),
                crate::api::EventHub::default(),
                true,
            )
            .await
            .unwrap();
            if mode == 2 {
                core.acquire_commands().await.unwrap();
                core.freeze_commands().await.unwrap();
            }
            Some(core)
        };
        let mut observed = Ownership::test_watch_replay(data.clone());
        let (started, release) = Ownership::test_stall_replay(data.clone());
        let mut caller = Box::pin(async move {
            let core = match prepared {
                Some(core) => core,
                None => Core::new(data.clone(), Arc::new(tokio::sync::Notify::new())).await?,
            };
            let result = match mode {
                1 => core.acquire_commands().await,
                2 => core
                    .reopen_commands()
                    .await
                    .map_err(io::Error::other)?
                    .map_err(io::Error::other),
                _ => Ok(()),
            };
            Ok::<_, io::Error>((core, result))
        });
        tokio::select! {
            entered = within(started) => entered.unwrap(),
            _ = &mut caller => panic!("completed before replay entered"),
        }
        within(observed.recv())
            .await
            .expect("replay watchdog did not arm");
        tokio::time::pause();
        // The documented stall window is ten seconds, independently of total work.
        tokio::time::advance(Duration::from_secs(11)).await;
        let outcome = tokio::time::timeout(Duration::from_secs(1), &mut caller)
            .await
            .map_or(Poll::Pending, Poll::Ready);
        tokio::time::resume();
        let bounded = outcome.is_ready();
        let contender = Core::new_shared(
            root.path().to_owned(),
            Arc::new(tokio::sync::Notify::new()),
            Owner::default(),
            crate::api::EventHub::default(),
            true,
        )
        .await
        .unwrap();
        let error = contender.acquire_commands().await.unwrap_err();
        assert_eq!(
            error
                .get_ref()
                .and_then(|e| e.downcast_ref::<crate::api::error_code::ErrorCode>()),
            Some(&crate::api::error_code::ErrorCode::SessionBusy)
        );
        contender.shutdown().await.unwrap();
        if let Poll::Ready(Ok((core, _))) = &outcome {
            assert!(core.shared.commands.full("agent.decide"));
            core.journal.stop();
        }
        release.send(()).unwrap();
        assert!(
            bounded,
            "mode {mode}: stalled replay exceeded its stall window"
        );
        let error = match outcome {
            Poll::Ready(Err(error)) => error,
            Poll::Ready(Ok((core, result))) => {
                core.shutdown().await.unwrap();
                result.unwrap_err()
            }
            Poll::Pending => unreachable!(),
        };
        assert_eq!(
            error
                .get_ref()
                .and_then(|e| e.downcast_ref::<crate::api::error_code::ErrorCode>()),
            Some(&crate::api::error_code::ErrorCode::SessionBusy)
        );
    }
}

#[tokio::test]
async fn near_cap_startup_importer_and_rollback_allow_replay_past_lease_budget() {
    use crate::web::wire::commands::Ownership;
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let data = root.path().to_owned();
    let seed = Core::new(data.clone(), Arc::new(tokio::sync::Notify::new()))
        .await
        .unwrap();
    seed.journal.seed_capacity_test().await;
    seed.shutdown().await.unwrap();
    for mode in 0..3 {
        let mut observed = Ownership::test_watch_replay(data.clone());
        let (mut steps, release) = Ownership::test_step_replay(data.clone());
        let path = data.clone();
        let mut caller = Box::pin(async move {
            let core = Core::new_shared(
                path,
                Arc::new(tokio::sync::Notify::new()),
                Owner::default(),
                crate::api::EventHub::default(),
                mode != 0,
            )
            .await?;
            if mode == 1 {
                core.acquire_commands().await?;
            } else if mode == 2 {
                core.reopen_commands()
                    .await
                    .map_err(io::Error::other)?
                    .map_err(io::Error::other)?;
            }
            Ok::<_, io::Error>(core)
        });
        let mut previous = 0;
        for _ in 0..3 {
            let bytes = tokio::select! {
                entered = within(steps.recv()) => entered.expect("replay step producer exited"),
                result = &mut caller => panic!("mode {mode} completed before replay: {:?}", result.err()),
            };
            assert!(bytes > previous, "replay must consume actual journal bytes");
            assert!(
                bytes < 60 * 1024,
                "steps must precede the complete large outcome"
            );
            previous = bytes;
            within(async {
                loop {
                    let (consumed, _) = observed.recv().await.expect("watchdog producer exited");
                    if consumed >= bytes {
                        break;
                    }
                }
            })
            .await;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(6)).await;
            let premature = tokio::time::timeout(Duration::from_millis(1), &mut caller)
                .await
                .is_ok();
            tokio::time::resume();
            release.send(()).unwrap();
            assert!(
                !premature,
                "mode {mode} timed out despite advancing replay bytes"
            );
        }
        let result = tokio::time::timeout(Duration::from_secs(30), &mut caller)
            .await
            .expect("replay worker failed to complete after release");
        let core = result.unwrap();
        assert!(!core.shared.commands.full("agent.decide"));
        core.shutdown().await.unwrap();
    }
}

async fn stalled_open_fails_before_ready(deferred: bool) {
    let root = crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
    let data = root.path().to_owned();
    let wake = Arc::new(tokio::sync::Notify::new());
    let (started, release) =
        crate::web::wire::commands::Ownership::test_stall_next_open(data.clone());
    let mut caller = tokio::spawn(async move {
        let core = Core::new_shared(
            data,
            wake,
            Owner::default(),
            crate::api::EventHub::default(),
            deferred,
        )
        .await?;
        if deferred {
            core.acquire_commands().await?;
        }
        Ok::<_, io::Error>(core)
    });
    within(started).await.unwrap();
    let bounded = tokio::time::timeout(Duration::from_secs(2), &mut caller).await;
    // Release the producer even when the old behavior wrongly returns a Core.
    release.send(()).unwrap();
    let result = bounded
        .expect("startup/importer waited indefinitely for journal open")
        .unwrap();
    let error = match result {
        Err(error) => error,
        Ok(core) => {
            core.shutdown().await.unwrap();
            panic!("reported ready before command journal lease and replay completed");
        }
    };
    assert_eq!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<crate::api::error_code::ErrorCode>()),
        Some(&crate::api::error_code::ErrorCode::SessionBusy),
    );
}
#[test]
fn stalled_startup_fails_with_typed_error_before_ready() {
    runtime(stalled_open_fails_before_ready(false));
}
#[test]
fn stalled_importer_commit_fails_with_typed_error_before_ready() {
    runtime(stalled_open_fails_before_ready(true));
}
#[test]
fn importer_preparation_does_not_acquire_exporters_lease() {
    runtime(async {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let exporter = Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new()))
            .await
            .unwrap();
        let importer = Core::new_shared(
            root.path().to_owned(),
            Arc::new(tokio::sync::Notify::new()),
            Owner::default(),
            crate::api::EventHub::default(),
            true,
        )
        .await
        .unwrap();
        assert!(importer.shared.commands.full("agent.decide"));
        exporter.freeze_commands().await.unwrap();
        importer.acquire_commands().await.unwrap();
        assert!(!importer.shared.commands.full("agent.decide"));
        let contender =
            Core::new(root.path().to_owned(), Arc::new(tokio::sync::Notify::new())).await;
        assert!(
            matches!(contender, Err(error) if error.get_ref().and_then(|e| e.downcast_ref::<crate::api::error_code::ErrorCode>()) == Some(&crate::api::error_code::ErrorCode::SessionBusy))
        );
        importer.shutdown().await.unwrap();
        exporter.shutdown().await.unwrap();
    });
}
