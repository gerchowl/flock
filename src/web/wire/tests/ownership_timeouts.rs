use super::*;

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
        let (started, release) = Ownership::test_stall_replay(data.clone());
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
        tokio::select! {
            entered = within(started) => entered.unwrap(),
            result = &mut caller => panic!("mode {mode} completed before replay: {:?}", result.err()),
        }
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        // Poll after the old deadline while the actual replay worker is held.
        let premature = futures_util::poll!(caller.as_mut()).is_ready();
        tokio::time::resume();
        release.send(()).unwrap();
        assert!(
            !premature,
            "mode {mode} exhausted the replay wall-clock budget"
        );
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
