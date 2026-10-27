//! Deterministic access to the real publication lock and overflow path.
use super::*;
const WITHIN: std::time::Duration = std::time::Duration::from_secs(10);
pub struct Fixture {
    pub source: Source,
    publisher: Option<Publisher>,
    journal: Journal,
    _root: crate::structured::launch::SessionConfigDir,
}
impl Fixture {
    pub fn new() -> Self {
        let root =
            crate::structured::launch::SessionConfigDir::create(&std::env::temp_dir()).unwrap();
        let journal = Journal::open(root.path(), "view-fixture").unwrap();
        let (publisher, source) = Publisher::new("view-fixture", journal.replay(0).unwrap());
        Self {
            source,
            publisher: Some(publisher),
            journal,
            _root: root,
        }
    }
    pub fn capture(&mut self) {
        self.publisher
            .as_mut()
            .unwrap()
            .capture(|| self.journal.subscription().map_err(Into::into), None);
    }
    pub fn append(&mut self, n: usize) {
        let after = self.journal.replay(0).unwrap().through;
        let item = crate::structured::queue::tests::item(&format!("item-{n}"));
        self.journal
            .append(crate::structured::queue::Change::Upsert {
                item: Box::new(item),
                position: n,
            })
            .unwrap();
        self.publisher.as_mut().unwrap().publish(&Update {
            facts: vec![],
            admitted: None,
            offer: Ok(Offer::NoEligibleItems),
            pending: None,
            cut: Some(self.journal.replay(after).unwrap()),
        });
    }
    pub fn competing_subscriber(&self, f: impl FnOnce()) {
        let ended = self.source.1.lock().ended;
        let source = self.source.clone();
        let (ready, entered) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let competing = std::thread::spawn(move || {
            GATE.with(|slot| *slot.borrow_mut() = Some((ready, gate)));
            source.subscribe(None).map(drop)
        });
        entered.recv_timeout(WITHIN).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        release.send(()).unwrap();
        let subscribed = competing.join().unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        assert!(
            subscribed.is_ok() || (!ended && matches!(subscribed, Err(ErrorCode::SessionBusy)))
        );
    }
    pub fn finish(&mut self, refused: bool) {
        self.publisher.as_mut().unwrap().finish(
            || {
                if refused {
                    Err(Failure {
                        code: ErrorCode::InternalError,
                        source: None,
                    })
                } else {
                    self.journal.subscription().map_err(Into::into)
                }
            },
            None,
        );
        drop(self.publisher.take());
    }
}

thread_local! {
    static GATE: std::cell::RefCell<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>> = const { std::cell::RefCell::new(None) };
}
/// Pause a real subscriber after book acquisition, without manufacturing a wake.
pub(super) fn pause_subscription() {
    GATE.with(|slot| {
        if let Some((ready, release)) = slot.borrow_mut().take() {
            ready.send(()).unwrap();
            release.recv_timeout(WITHIN).unwrap();
        }
    });
}
