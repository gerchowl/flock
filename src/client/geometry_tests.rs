use super::*;

#[test]
fn lost_terminal_poll_detaches_without_sending_a_resize() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let mut samples = 0;
    resize_poll_loop_with(
        tx,
        132,
        40,
        &quit,
        || {
            samples += 1;
            assert!(samples <= 2, "geometry loss must terminate the poller");
            if samples == 1 {
                Some((132, 40, 0, 0))
            } else {
                // Bound even the old fallback implementation: it must fail, not hang.
                quit.store(true, Ordering::Release);
                None
            }
        },
        || {},
    );
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn missing_terminal_at_poll_start_detaches_without_waiting() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    resize_poll_loop_with(
        tx,
        132,
        40,
        &quit,
        || {
            quit.store(true, Ordering::Release);
            None
        },
        || panic!("unavailable terminal must detach before polling"),
    );
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}

#[test]
fn real_geometry_change_is_forwarded_before_later_terminal_loss() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let quit = Arc::new(AtomicBool::new(false));
    let mut samples = [Some((132, 40, 8, 16)), Some((160, 50, 9, 18)), None].into_iter();
    resize_poll_loop_with(
        tx,
        132,
        40,
        &quit,
        || samples.next().expect("poller must exit on loss"),
        || {},
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(ClientLoopEvent::Resize(160, 50, 9, 18))
    ));
    assert!(matches!(rx.try_recv(), Ok(ClientLoopEvent::Detach)));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
}
