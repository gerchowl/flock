use super::*;
use crate::structured::{
    registry::Registry,
    workers::{tests::Case, Workers},
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
fn model() -> Model {
    let agent = serde_json::from_value(json!({"runtime":"structured","agent_id":"fixture-agent","harness":"claude","name":null,"lifecycle":"offline","status":"unknown","state":crate::api::agent_state::AgentState::unknown(),"session_id":"fixture-session","cwd":".","model":"requested","lease_state":"absent"})).unwrap();
    Model::new(
        Target {
            agent_id: "fixture-agent".into(),
            generation: 1,
        },
        agent,
        Default::default(),
        vec![crate::structured::queue::tests::item("owned")],
    )
}
fn record(seq: u64, raw: &str, known: bool) -> Arc<StreamRecord> {
    Arc::new(serde_json::from_value(json!({"seq":seq,"raw":raw,"known":known,"received_at_ms":1,"origin":"claude","result_subtype":null})).unwrap())
}
fn contents(m: &Model) -> impl Iterator<Item = &Content> {
    m.snapshot.transcript.iter().map(|c| &c.content)
}
fn delta(seq: u64) -> live::Item {
    let r = record(
        seq,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"stale"}}}"#,
        true,
    );
    live::Item::Delta {
        event: Arc::new(crate::structured::claude::SessionEvent::parse(
            &r.raw, "2.1.296",
        )),
        record: r,
    }
}
#[test]
fn gap_floor_rejects_burst_tail_without_spending_a_cursor() {
    let mut m = model();
    m.live(live::Item::Gap { through: 3 });
    for _ in 0..16 {
        m.live(delta(1));
    }
    assert!(m.snapshot.transcript.is_empty());
    assert_eq!(m.snapshot.applied_cursor, 0);
    assert!(matches!(m.snapshot.replay, Replay::Lagged { through: 3 }));
    m.live(delta(3));
    assert!(m.snapshot.transcript.iter().any(|c| c.streaming));
}
#[test]
fn provenance_usage_tool_correlation_and_owned_facts() {
    let mut m = model();
    m.snapshot.scroll_anchor = Some(77);
    let raw = r#"{ "type":"assistant", "future":1.00, "message":{"content":[{"type":"text","text":"known-looking"}]} }"#;
    let mut old = (*record(1, raw, false)).clone();
    old.shape = Some(crate::structured::claude::ShapeKey(
        "recorded-version".into(),
    ));
    m.history(history::Item::Record(Arc::new(old))).unwrap();
    assert!(
        matches!(&m.snapshot.transcript[0].content, Content::RawJson { known: Some(false), shape: Some(s), raw: r } if s.0 == "recorded-version" && r == raw)
    );
    assert_eq!(m.snapshot.transcript.len(), 1, "unknown never reclassified");
    m.history(history::Item::Record(record(2, r#"{"type":"assistant","usage":{"input_tokens":7},"message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#, true))).unwrap();
    m.history(history::Item::Record(record(3, r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#, true))).unwrap();
    assert!(contents(&m).any(|c| matches!(c, Content::ToolUse(t) if t.id == "t")));
    assert!(contents(&m)
        .any(|c| matches!(c, Content::ToolResult { tool_use_id, .. } if tool_use_id == "t")));
    let usage = m.snapshot.usage.as_ref().unwrap();
    assert_eq!(
        (usage.observed_seq, usage.input_tokens, usage.output_tokens),
        (2, Some(7), None)
    );
    assert_eq!(m.snapshot.scroll_anchor, Some(77));
    assert!(m.snapshot.new_output && m.snapshot.queue[0].item_id == "owned");
    assert_eq!(m.snapshot.agent.model.as_deref(), Some("requested"));
    assert_eq!(
        m.snapshot.agent.status,
        crate::api::schema::AgentStatus::Unknown
    );
    m.history(history::Item::Record(record(3, "{}", true)))
        .unwrap();
    assert_eq!(m.snapshot.applied_cursor, 3);
    assert!(m
        .history(history::Item::Record(record(5, "{}", true)))
        .is_err());
    m.history(history::Item::Gap {
        from: 4,
        to: 5,
        reason: history::GapReason::RetainedPrefix,
    })
    .unwrap();
    assert!(matches!(
        m.snapshot.transcript.last().unwrap().content,
        Content::Gap { from: 4, to: 5, .. }
    ));
}
#[tokio::test]
async fn real_fake_partials_reconcile_and_burst_resync() {
    for mode in [
        "s10b-live-exit",
        "s10b-live-burst-exit",
        "s10b-live-interleaved-exit",
    ] {
        let notify = Arc::new(Notify::new());
        let (mut registry, mut workers) = (Registry::default(), Workers::new(notify.clone()));
        let (mut case, mut launch, factory) = Case::prepare(&mut registry, 920, "unused", vec![]);
        let mut identity = registry.remove(&case.id, case.generation).unwrap();
        identity.model = Some(mode.into());
        launch.generation = registry.register(identity).unwrap();
        let executable = case.fixture.root.path().join("claude");
        let interpreter = std::fs::read_to_string(&executable)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        case.fixture.file(
            "claude",
            format!(
                "{interpreter}\n{}",
                include_str!("../../../../tests/fixtures/structured-public/claude.py")
            )
            .as_bytes(),
            0o700,
        );
        case.generation = workers.start(&mut registry, launch, factory).unwrap();
        let mut live = workers.live(&registry, &case.id).unwrap();
        let clock = Instant::now();
        crate::structured::workers::tests::wait(&notify, || {
            workers.poll(&mut registry, clock, 1000);
            workers.report(&case.id, case.generation).is_some()
        })
        .await;
        workers.shutdown(&mut registry).unwrap();
        let mut m = model();
        loop {
            let item = tokio::time::timeout(Duration::from_secs(10), live.recv())
                .await
                .unwrap();
            if matches!(item, live::Item::End(_)) {
                break;
            }
            m.live(item);
        }
        if mode.contains("burst") {
            assert!(
                m.snapshot.transcript.is_empty(),
                "seq-1 burst tail predates Gap through 3"
            );
        } else if mode.contains("interleaved") {
            assert!(contents(&m).any(|c| matches!(c, Content::Markdown(s) if s == "partial")));
            assert!(contents(&m).any(|c| matches!(c, Content::Markdown(s) if s == "later")));
        } else {
            assert!(contents(&m).any(|c| matches!(c, Content::Markdown(s) if s == "partial")));
            assert!(contents(&m).any(|c| matches!(c, Content::Thinking(s) if s == "thinking")));
            assert!(m.snapshot.transcript.iter().any(|c| matches!(
                c.content,
                Content::Thinking(_)
            ) && m
                .snapshot
                .collapsed
                .contains(&c.id)));
        }
        let mut cut = workers
            .history(&registry, &case.id)
            .unwrap()
            .subscribe(Some(m.snapshot.applied_cursor))
            .unwrap();
        loop {
            let item = cut.page().unwrap();
            let done = matches!(item, history::Item::Synced(_));
            m.history(item).unwrap();
            if done {
                break;
            }
        }
        assert_eq!(m.snapshot.applied_cursor, 3);
        assert_eq!(
            contents(&m)
                .filter(|c| matches!(c, Content::Markdown(s) if s == "partial"))
                .count(),
            1
        );
        if mode.contains("interleaved") {
            assert!(
                m.snapshot
                    .transcript
                    .iter()
                    .any(|c| c.streaming
                        && matches!(&c.content, Content::Markdown(s) if s == "later"))
            );
        } else {
            assert!(m.snapshot.transcript.iter().all(|c| !c.streaming));
        }
        assert!(contents(&m).any(|c| matches!(c, Content::ToolResult { tool_use_id, .. } if tool_use_id == "fixture-tool")));
        assert!(m.snapshot.reported_model.is_some());
        assert_eq!(m.snapshot.reported_permissions.as_deref(), Some("default"));
    }
}
#[test]
fn thinking_fold_does_not_hide_sibling_text() {
    let mut m = model();
    m.history(history::Item::Record(record(1, r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"},{"type":"thinking","thinking":"thinking"}]}}"#, true))).unwrap();
    let thinking = m
        .snapshot
        .transcript
        .iter()
        .find(|c| matches!(c.content, Content::Thinking(_)))
        .unwrap();
    let text = m
        .snapshot
        .transcript
        .iter()
        .find(|c| matches!(c.content, Content::Markdown(_)))
        .unwrap();
    assert!(m.snapshot.collapsed.contains(&thinking.id));
    assert!(
        !m.snapshot.collapsed.contains(&text.id),
        "folding thinking must leave sibling text visible"
    );
}
fn message_delta(seq: u64, id: &str, text: &str) -> live::Item {
    let r = record(seq, &json!({"type":"stream_event","api_message_id":id,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}}).to_string(), true);
    live::Item::Delta {
        event: Arc::new(crate::structured::claude::SessionEvent::parse(
            &r.raw, "2.1.296",
        )),
        record: r,
    }
}
#[test]
fn interleaved_messages_do_not_share_block_zero() {
    let mut m = model();
    m.live(message_delta(1, "first", "partial"));
    m.snapshot.scroll_anchor = Some(1);
    m.live(message_delta(1, "first", "!"));
    assert!(m.snapshot.new_output);
    m.live(message_delta(2, "second", "later"));
    assert_eq!(
        contents(&m)
            .filter(|c| matches!(c,Content::Markdown(s) if s=="partial!"))
            .count(),
        1
    );
    assert_eq!(
        contents(&m)
            .filter(|c| matches!(c,Content::Markdown(s) if s=="later"))
            .count(),
        1
    );
}
#[test]
fn history_reconciles_only_its_message_and_preserves_later_partial() {
    let mut m = model();
    m.live(message_delta(0, "first", "partial"));
    m.live(message_delta(1, "second", "later"));
    m.history(history::Item::Record(record(1,r#"{"type":"assistant","message":{"id":"first","content":[{"type":"text","text":"partial"}]}}"#,true))).unwrap();
    assert!(m
        .snapshot
        .transcript
        .iter()
        .any(|c| c.streaming && matches!(&c.content,Content::Markdown(s) if s=="later")));
    assert_eq!(
        contents(&m)
            .filter(|c| matches!(c,Content::Markdown(s) if s=="partial"))
            .count(),
        1
    );
    assert_eq!(
        m.snapshot
            .transcript
            .iter()
            .map(|c| c.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        m.snapshot.transcript.len()
    );
}
#[test]
fn long_stream_retains_only_a_bounded_history_window_and_current_partial() {
    let mut m = model();
    for seq in 1..=64 {
        m.history(history::Item::Record(record(
            seq,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#,
            true,
        )))
        .unwrap();
    }
    assert_eq!(m.snapshot.applied_cursor, 64);
    assert_eq!(
        m.snapshot
            .transcript
            .iter()
            .filter(|c| matches!(c.content, Content::Markdown(_)))
            .count(),
        history::LIVE_RECORDS
    );
    assert!(m.snapshot.transcript.iter().all(|c| c.seq >= 49));
    for _ in 0..1000 {
        m.live(message_delta(64, "current", "x"));
    }
    assert_eq!(
        m.snapshot.transcript.iter().filter(|c| c.streaming).count(),
        1,
        "one current partial, no raw card per delta"
    );
    assert!(contents(&m).any(|c| matches!(c,Content::Markdown(s) if s.len()==1000)));
}
#[test]
fn live_capacity_and_text_overflow_require_replay_without_inventing_a_cursor() {
    for text_overflow in [false, true] {
        let mut m = model();
        if text_overflow {
            m.live(message_delta(
                0,
                "one",
                &"x".repeat(crate::structured::transport::MAX_LINE_BYTES),
            ));
            m.live(message_delta(0, "one", "x"));
        } else {
            for n in 0..=history::LIVE_RECORDS {
                m.live(message_delta(0, &format!("message-{n}"), "partial"));
            }
        }
        assert!(m.snapshot.transcript.is_empty());
        assert!(matches!(m.snapshot.replay, Replay::Lagged { through: 0 }));
        assert_eq!(m.snapshot.applied_cursor, 0);
        m.live(message_delta(0, "ignored", "tail"));
        assert!(m.snapshot.transcript.is_empty());
        m.history(history::Item::Record(record(1, "{}", true)))
            .unwrap();
        m.live(message_delta(1, "new", "accepted"));
        assert!(contents(&m).any(|c| matches!(c,Content::Markdown(s) if s=="accepted")));
    }
}
#[test]
fn malformed_blocks_and_observed_tools_keep_exact_source_without_permission_authority() {
    let mut m = model();
    let raw = r#"{ "type":"assistant", "message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{"x":1.00},"future":true},{"type":"text","text":12}]} }"#;
    m.history(history::Item::Record(record(1, raw, true)))
        .unwrap();
    assert!(contents(&m).any(|c| matches!(c,Content::ToolUse(t) if t.id=="t" && t.raw.as_ref()==raw && t.name=="Bash" && t.input.contains_key("x"))));
    assert!(!contents(&m).any(|c| matches!(c, Content::ToolRequest(_))));
    assert!(
        m.snapshot
            .collapsed
            .contains(&CardId { seq: 1, ordinal: 0 }),
        "malformed block is inspectable as raw JSON"
    );
    assert!(
        matches!(&m.snapshot.transcript[0].content,Content::RawJson {raw:r,known:Some(true),..} if r==raw)
    );
    let ids: Vec<_> = m.snapshot.transcript.iter().map(|c| c.id).collect();
    let mut replay = model();
    replay
        .history(history::Item::Record(record(1, raw, true)))
        .unwrap();
    assert_eq!(
        ids,
        replay
            .snapshot
            .transcript
            .iter()
            .map(|c| c.id)
            .collect::<Vec<_>>()
    );
}
