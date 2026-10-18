use super::{discovery, test_support::HungGit};
use std::path::Path;
use std::time::Duration;

fn bounded_probe(probe: fn(&Path) -> Result<Option<String>, crate::health::GitRefreshErrorKind>) {
    let fixture = HungGit::new();
    let cwd = fixture.cwd().to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(probe(&cwd));
    });
    let controller = fixture.started();
    // Failure ceiling, not synchronization: the producer has attested that it
    // is alive and silent. Closing its socket releases the original bug too.
    let outcome = rx.recv_timeout(Duration::from_secs(30));
    drop(controller);
    if outcome.is_err() {
        let _ = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("released child did not finish");
    }
    worker.join().unwrap();
    assert_eq!(
        outcome.expect("git command exceeded its ten-second bound"),
        Err(crate::health::GitRefreshErrorKind::Timeout)
    );
}

#[test]
fn git_refresh_symbolic_ref_hung_child_is_bounded() {
    bounded_probe(|cwd| {
        discovery::git_symbolic_head_full(cwd, std::time::Instant::now() + Duration::from_secs(60))
    });
}

#[test]
fn git_refresh_rev_parse_hung_child_is_bounded() {
    bounded_probe(|cwd| {
        discovery::git_rev_parse_verify(
            cwd,
            "HEAD",
            std::time::Instant::now() + Duration::from_secs(60),
        )
    });
}
