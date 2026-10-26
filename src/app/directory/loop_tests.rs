//! Exercise the real owner loops without API dispatch or terminal processes.
use crate::{
    api::{schema::EventKind, EventHub},
    app::App,
    events::AppEvent,
};
use std::time::{Duration, Instant};

/// Retained directory sequences, excluding unrelated lifecycle events.
pub(crate) fn changes(hub: &EventHub, after: u64) -> Vec<u64> {
    hub.ring_snapshot_after(after)
        .into_iter()
        .filter_map(|(seq, event)| (event.event == EventKind::DirectoryChanged).then_some(seq))
        .collect()
}

/// Seed one named pane, then queue its real internal death event without API dispatch.
pub(crate) fn prepare(app: &mut App) -> (EventHub, u64) {
    app.state.config.checks.enable = false;
    app.next_auto_update_check = None;
    app.state.local_sound_playback = false;
    app.local_terminal_notifications = false;
    app.state.workspaces = vec![crate::workspace::Workspace::test_new("loop-fixture")];
    app.state.ensure_test_terminals();
    let pane = app.state.workspaces[0].tabs[0].root_pane;
    let terminal = app.state.workspaces[0]
        .pane_state(pane)
        .unwrap()
        .attached_terminal_id
        .clone();
    app.state
        .terminals
        .get_mut(&terminal)
        .unwrap()
        .set_agent_name("loop-agent".into())
        .unwrap();
    // Seed before running: only the loop may publish the subsequent removal.
    app.sync_directory_events();
    assert_eq!(changes(&app.event_hub, 0).len(), 1);
    let after = app.event_hub.current_sequence();
    app.event_tx
        .try_send(AppEvent::PaneDied { pane_id: pane })
        .unwrap();
    (app.event_hub.clone(), after)
}

/// Wait on the hub, returning empty at the bounded producer-silence ceiling.
pub(crate) fn wait(hub: EventHub, after: u64) -> Vec<u64> {
    // A failure ceiling, not a latency expectation. Every wait uses the hub condvar.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Read the head before scanning so a concurrent push cannot strand the wait.
        let observed = hub.current_sequence();
        let found = changes(&hub, after);
        if !found.is_empty() {
            return found;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !hub.wait_after(observed, remaining) {
            return Vec::new();
        }
    }
}

#[tokio::test]
async fn main_loop_publishes_directory_removal_without_an_api_request() {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        EventHub::default(),
    );
    let (hub, after) = prepare(&mut app);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(1);
    app.input_rx = Some(input_rx);
    // Fixed geometry avoids querying the ambient terminal; nextest owns stdout.
    let mut terminal = ratatui::Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 60, 20)),
        },
    )
    .unwrap();
    let observer_hub = hub.clone();
    let observer = tokio::task::spawn_blocking(move || wait(observer_hub, after));
    let observed = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::select! {
            result = app.run(&mut terminal) => panic!("main loop exited before directory observation: {result:?}"),
            result = observer => result.expect("directory observer panicked"),
        }
    }).await.expect("main loop or directory observer stalled");
    app.finish_session().unwrap();
    assert!(
        app.state.workspaces.is_empty(),
        "the loop must process pane death"
    );
    assert_eq!(observed.len(), 1, "main loop did not publish the removal");
    assert_eq!(changes(&hub, after).len(), 1);
}
