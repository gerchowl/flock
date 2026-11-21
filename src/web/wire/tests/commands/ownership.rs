use super::*;

#[test]
fn stalled_rollback_replay_refuses_audited_commands_and_retries_after_worker_stops() {
    runtime(async {
        let mut case = Case::new(183172).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        case.server
            .core
            .journal
            .claim_test(crate::web::wire::commands::Command {
                id: "retained-stall".into(),
                verb: "agent.send".into(),
                args: json!({}),
                resume_only: false,
                read_only: false,
            })
            .await;
        case.server.core.freeze_commands().await.unwrap();
        let mut observed = crate::web::wire::commands::Ownership::test_watch_replay(
            case.server._root.path().to_owned(),
        );
        let (started, release) = crate::web::wire::commands::Ownership::test_stall_replay(
            case.server._root.path().to_owned(),
        );
        let recovery = case.server.core.reopen_commands();
        within(started).await.unwrap();
        within(observed.recv())
            .await
            .expect("replay watchdog did not arm");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        let outcome = tokio::time::timeout(Duration::from_secs(1), recovery).await;
        tokio::time::resume();
        let result = match outcome {
            Ok(result) => result.unwrap(),
            Err(_) => {
                release.send(()).unwrap();
                panic!("rollback did not return its replay stall error");
            }
        };
        assert_eq!(result, Err(crate::api::error_code::ErrorCode::SessionBusy));
        refusal(
            case.command(&mut socket, "agent.decide", args.clone())
                .await,
            "session_busy",
            crate::api::error_code::ErrorCode::SessionBusy.exit(),
        );
        assert_eq!(case.handles().len(), 2);
        let events = refusal_events(&case);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reason"], "session_busy");
        let (mut steps, next) = crate::web::wire::commands::Ownership::test_step_replay(
            case.server._root.path().to_owned(),
        );
        release.send(()).unwrap();
        assert!(
            within(steps.recv()).await.is_some(),
            "late replay must retire before a new attempt opens"
        );
        assert!(case.server.core.shared.commands.full("agent.decide"));
        next.send(()).unwrap();
        let wake = case.server.wake.clone();
        within(async {
            loop {
                let changed = wake.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !case.server.core.shared.commands.full("agent.decide") {
                    break;
                }
                changed.await;
            }
        })
        .await;
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["ok"],
            true
        );
        case.until(|r| r.handles().len() == 1).await;
        case.stop().await;
    });
}

#[test]
fn rollback_reopens_wire_admission_before_effect_and_persists_outcome() {
    runtime(async {
        let mut case = Case::new(183170).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        case.server.core.freeze_commands().await.unwrap();
        refusal(
            case.command(&mut socket, "agent.decide", args.clone())
                .await,
            "session_busy",
            crate::api::error_code::ErrorCode::SessionBusy.exit(),
        );
        assert_eq!(case.handles().len(), 2);
        within(case.server.core.reopen_commands())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["ok"],
            true
        );
        case.until(|r| r.handles().len() == 1).await;
        assert!(
            fs::metadata(case.server._root.path().join("wire/commands.jsonl"))
                .unwrap()
                .len()
                > 0,
            "live admission must persist its claim and outcome"
        );
        case.stop().await;
    });
}
#[test]
fn failed_rollback_reacquire_is_typed_audited_and_never_dispatches() {
    runtime(async {
        let mut case = Case::new(183171).await;
        let args = case.prompt().await;
        let mut socket = case.open().await;
        case.server.core.freeze_commands().await.unwrap();
        let holder = Core::new(
            case.server._root.path().to_owned(),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
        .unwrap();
        let result = within(case.server.core.reopen_commands()).await.unwrap();
        assert_eq!(
            result.unwrap_err(),
            crate::api::error_code::ErrorCode::SessionBusy
        );
        refusal(
            case.command(&mut socket, "agent.decide", args.clone())
                .await,
            "session_busy",
            crate::api::error_code::ErrorCode::SessionBusy.exit(),
        );
        assert_eq!(case.handles().len(), 2);
        let events = refusal_events(&case);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reason"], "session_busy");
        assert_eq!(events[0]["token_id"], "one");
        holder.shutdown().await.unwrap();
        let wake = case.server.wake.clone();
        within(async {
            loop {
                let changed = wake.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !case.server.core.shared.commands.full("agent.decide") {
                    break;
                }
                changed.await;
            }
        })
        .await;
        assert_eq!(
            case.command(&mut socket, "agent.decide", args).await["ok"],
            true
        );
        case.until(|r| r.handles().len() == 1).await;
        case.stop().await;
    });
}
