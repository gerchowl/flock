//! Process-wide custody writer gate shared by enrollment and mail workers.
use super::store::Store;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

static RECOVERING: AtomicBool = AtomicBool::new(false);
static SUSPENDED: AtomicBool = AtomicBool::new(false);
static READ_PATH: RwLock<Option<std::path::PathBuf>> = RwLock::new(None);
thread_local! { static RECOVERY_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

pub(crate) fn begin_recovery() {
    RECOVERING.store(true, Ordering::Release);
}

pub(crate) fn recovery_work<T>(f: impl FnOnce() -> T) -> T {
    RECOVERY_WORKER.with(|flag| flag.set(true));
    let result = f();
    RECOVERY_WORKER.with(|flag| flag.set(false));
    result
}

fn check_recovery() -> Result<(), String> {
    if RECOVERING.load(Ordering::Acquire) && !RECOVERY_WORKER.with(|flag| flag.get()) {
        return Err("mesh store recovery in progress".into());
    }
    Ok(())
}

#[derive(Default)]
struct Writer {
    store: Option<Store>,
    suspended: bool,
    generation: u64,
    reason: Option<String>,
}

impl Writer {
    fn open(&mut self, path: &Path, minimum: u64) -> Result<(), String> {
        Store::check_generation(path, minimum).map_err(|e| e.to_string())?;
        if self.store.is_none() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis() as i64;
            self.store = Some(Store::open(path, now).map_err(|e| e.to_string())?);
            if let Ok(mut read_path) = READ_PATH.write() {
                *read_path = Some(path.to_owned());
            }
        }
        self.suspended = false;
        SUSPENDED.store(false, Ordering::Release);
        Ok(())
    }

    fn access<T>(
        &mut self,
        path: &Path,
        f: impl FnOnce(&mut Store) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.suspended {
            return Err(self
                .reason
                .clone()
                .unwrap_or_else(|| "mesh store writer suspended for handoff".into()));
        }
        self.open(path, 0)?;
        match self.store.as_mut() {
            Some(store) => f(store),
            None => Err("mesh store unavailable".into()),
        }
    }

    fn suspend(&mut self) -> Result<u64, String> {
        if let Some(store) = self.store.as_mut() {
            self.generation = store.handoff_generation().map_err(|e| e.to_string())?;
        }
        self.store = None;
        self.suspended = true;
        SUSPENDED.store(true, Ordering::Release);
        Ok(self.generation)
    }
}

fn writer() -> &'static Mutex<Writer> {
    static WRITER: OnceLock<Mutex<Writer>> = OnceLock::new();
    WRITER.get_or_init(Default::default)
}

pub(crate) fn with_store<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    check_recovery()?;
    let mut writer = writer().lock().map_err(|_| "mesh store poisoned")?;
    writer.access(&crate::config::state_dir().join("mesh-mail.sqlite"), f)
}

/// Queries never bootstrap a writer, migrate a database, or advance its clock.
pub(crate) fn read<T>(f: impl FnOnce(&Store) -> Result<T, String>) -> Result<Option<T>, String> {
    check_recovery()?;
    let writer = writer().lock().map_err(|_| "mesh store poisoned")?;
    if writer.suspended {
        return Err(writer
            .reason
            .clone()
            .unwrap_or_else(|| "mesh store suspended for handoff".into()));
    }
    writer.store.as_ref().map(f).transpose()
}

pub(crate) fn status(
    origin: Option<&str>,
    correlation: &str,
    reference: Option<&super::store::StatusReference>,
) -> Result<Option<super::store::Status>, String> {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as i64;
    Ok(read(|store| {
        match reference {
            Some(reference) => store
                .referenced_status(correlation, reference, wall)
                .map(Some),
            None => match origin {
                Some(origin) => store.status(origin, correlation, wall),
                None => Ok(None),
            },
        }
        .map_err(|e| e.to_string())
    })?
    .flatten())
}

pub(crate) fn suspend() -> Result<u64, String> {
    check_recovery()?;
    writer()
        .lock()
        .map_err(|_| "mesh store poisoned")?
        .suspend()
}

pub(crate) fn suspended() -> Result<bool, String> {
    Ok(writer()
        .lock()
        .map_err(|_| "mesh store poisoned")?
        .suspended)
}

pub(crate) fn resume(minimum: u64) -> Result<(), String> {
    let mut writer = writer().lock().map_err(|_| "mesh store poisoned")?;
    if writer.suspended {
        writer.generation = minimum.max(writer.generation);
        let minimum = writer.generation;
        if cfg!(debug_assertions)
            && std::env::var_os("FLOCK_TEST_MESH_OPEN_FAIL_FILE")
                .is_some_and(|path| Path::new(&path).exists())
        {
            return Err("injected mesh store open failure".into());
        }
        if cfg!(debug_assertions) {
            if let Some(path) = std::env::var_os("FLOCK_TEST_MESH_OPEN_WAIT_FILE") {
                while Path::new(&path).exists() {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        writer.open(
            &crate::config::state_dir().join("mesh-mail.sqlite"),
            minimum,
        )?;
    }
    Ok(())
}

/// Keep the last known generation and refuse workers until recovery completes.
pub(crate) fn failed(reason: String) {
    SUSPENDED.store(true, Ordering::Release);
    RECOVERING.store(false, Ordering::Release);
    if let Ok(mut writer) = writer().lock() {
        writer.store = None;
        writer.suspended = true;
        writer.reason = Some(reason);
    }
}

pub(crate) fn recovery_reason() -> Option<String> {
    if RECOVERING.load(Ordering::Acquire) {
        return Some("mesh store recovery in progress".into());
    }
    writer()
        .lock()
        .ok()
        .and_then(|writer| writer.reason.clone())
}

pub(crate) fn recovered() {
    RECOVERING.store(false, Ordering::Release);
    if let Ok(mut writer) = writer().lock() {
        writer.reason = None;
    }
}

/// One lazy WAL reader per socket waiter, with a verified reference cached once.
#[derive(Default)]
pub(crate) struct WaitReader {
    store: Option<Store>,
    reference: Option<super::store::StatusReference>,
}

impl WaitReader {
    pub(crate) fn status(
        &mut self,
        origin: Option<&str>,
        correlation: &str,
        reference: Option<&super::store::StatusReference>,
    ) -> Result<Option<super::store::Status>, String> {
        check_recovery()?;
        if SUSPENDED.load(Ordering::Acquire) {
            self.store = None;
            return Err("mesh store suspended for handoff".into());
        }
        if self.store.is_none() {
            let path = READ_PATH
                .read()
                .map_err(|_| "mesh reader poisoned")?
                .clone();
            let Some(path) = path else { return Ok(None) };
            self.store = Some(Store::read_only(&path).map_err(|e| e.to_string())?);
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis() as i64;
        let status = if let Some(cached) = &self.reference {
            Some(
                store
                    .status_key(&cached.key, wall, Some(cached))
                    .map_err(|e| e.to_string())?
                    .unwrap_or_else(|| super::store::Status {
                        state: "outcome_retention_elapsed".into(),
                        reference: cached.clone(),
                        reply: None,
                        detail: None,
                    }),
            )
        } else if let Some(reference) = reference {
            Some(
                store
                    .referenced_status(correlation, reference, wall)
                    .map_err(|e| e.to_string())?,
            )
        } else if let Some(origin) = origin {
            store
                .status(origin, correlation, wall)
                .map_err(|e| e.to_string())?
        } else {
            None
        };
        if let Some(status) = &status {
            self.reference = Some(status.reference.clone());
        }
        Ok(status)
    }
}

#[cfg(test)]
pub(crate) struct TestStore(std::path::PathBuf);

#[cfg(test)]
impl TestStore {
    pub(crate) fn new() -> Self {
        let key = super::key::MessageKey::mint("fixture.example".into(), 0).unwrap();
        let path = std::env::temp_dir().join(format!("flock-relay-{}", key.message_id));
        std::fs::create_dir_all(&path).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let store = Store::open(&path.join("mesh-mail.sqlite"), now).unwrap();
        *READ_PATH.write().unwrap() = Some(store.path().to_owned());
        SUSPENDED.store(false, Ordering::Release);
        RECOVERING.store(false, Ordering::Release);
        *writer().lock().unwrap() = Writer {
            store: Some(store),
            ..Default::default()
        };
        Self(path)
    }

    pub(crate) fn delivery(&self) -> super::delivery::Deliver {
        let key = super::key::MessageKey::mint("nodea".into(), 0).unwrap();
        super::delivery::Deliver {
            remaining_ms: super::store::CUSTODY_TTL_MS,
            envelope: super::store::Envelope {
                kind: Default::default(),
                origin_key: Vec::new(),
                signature: Vec::new(),
                return_binding: super::store::ReturnBinding::mint(
                    key.clone(),
                    "nodeb".into(),
                    Vec::new(),
                )
                .unwrap(),
                key,
                sender: "agent_nodea_sender".into(),
                target_agent: "agent_nodeb_recipient".into(),
                target_session: "session".into(),
                correlation_id: "question".into(),
                in_reply_to: None,
                request_key: None,
                intent: "\"needs_reply\"".into(),
                body: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
impl Drop for TestStore {
    fn drop(&mut self) {
        *writer().lock().unwrap() = Writer {
            suspended: true,
            ..Default::default()
        };
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let key = crate::mesh::key::MessageKey::mint("writer.test".into(), 0).unwrap();
            let dir = std::env::temp_dir().join(format!("flock-writer-{}", key.message_id));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("mail.sqlite")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn many_waiters_do_not_contend_on_the_writer() {
        let fixture = TestStore::new();
        let delivery = fixture.delivery();
        with_store(|store| {
            store
                .accept(
                    &delivery.envelope,
                    super::super::store::CUSTODY_TTL_MS,
                    super::super::store::Admission::Custody,
                    1,
                )
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let guard = writer().lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut reader = WaitReader::default();
                    let first = reader
                        .status(Some("nodea"), "question", None)
                        .unwrap()
                        .unwrap();
                    let second = reader
                        .status(Some("nodea"), "question", None)
                        .unwrap()
                        .unwrap();
                    assert_eq!(first.reference, second.reference);
                    tx.send(()).unwrap();
                })
            })
            .collect();
        let completed =
            (0..16).try_for_each(|_| rx.recv_timeout(std::time::Duration::from_secs(3)));
        drop(guard);
        completed.unwrap();
        for handle in handles {
            handle.join().unwrap();
        }
        SUSPENDED.store(true, Ordering::Release);
        assert!(WaitReader::default()
            .status(Some("nodea"), "question", None)
            .is_err());
    }

    #[test]
    fn suspended_writer_cannot_be_reopened_by_a_worker_and_rollback_restores_it() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut writer = Writer::default();
        writer.access(&path, |_| Ok(())).unwrap();
        let generation = writer.suspend().unwrap();
        assert!(generation > 0);
        assert!(writer.store.is_none());
        assert!(writer
            .access::<()>(&path, |_| panic!("worker entered"))
            .is_err());
        writer.open(&path, generation).unwrap();
        writer.access(&path, |_| Ok(())).unwrap();
        assert!(writer.suspend().unwrap() > generation);
    }

    #[test]
    fn importer_does_not_create_store_before_commit() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut writer = Writer::default();
        assert_eq!(writer.suspend().unwrap(), 0);
        assert!(writer.access(&path, |_| Ok(())).is_err());
        assert!(!path.exists());
        writer.open(&path, 0).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn older_generation_is_refused_without_opening_a_writer_or_changing_disk() {
        let fixture = Fixture::new();
        let path = fixture.path();
        let mut writer = Writer::default();
        writer.access(&path, |_| Ok(())).unwrap();
        let generation = writer.suspend().unwrap();
        let before = std::fs::read(&path).unwrap();
        let reason = writer.open(&path, generation + 1).unwrap_err();
        assert!(reason.contains("older than handoff generation"), "{reason}");
        assert!(writer.suspended);
        assert!(writer.store.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        writer.open(&path, generation).unwrap();
    }
}
