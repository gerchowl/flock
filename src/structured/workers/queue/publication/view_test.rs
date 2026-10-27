//! Deterministic access to the real publication lock and overflow path.
use super::*;
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
    pub fn locked(&self, f: impl FnOnce()) {
        let held = self.source.1.lock();
        f();
        drop(held);
        self.source.2.notify_waiters();
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
