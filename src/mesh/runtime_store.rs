//! Process-wide custody writer gate shared by enrollment and mail workers.
use super::store::Store;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

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
        }
        self.suspended = false;
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
        Ok(self.generation)
    }
}

fn writer() -> &'static Mutex<Writer> {
    static WRITER: OnceLock<Mutex<Writer>> = OnceLock::new();
    WRITER.get_or_init(Default::default)
}

pub(crate) fn with_store<T>(f: impl FnOnce(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let mut writer = writer().lock().map_err(|_| "mesh store poisoned")?;
    writer.access(&crate::config::state_dir().join("mesh-mail.sqlite"), f)
}

/// Queries never bootstrap a writer, migrate a database, or advance its clock.
pub(crate) fn read<T>(f: impl FnOnce(&Store) -> Result<T, String>) -> Result<Option<T>, String> {
    let writer = writer().lock().map_err(|_| "mesh store poisoned")?;
    if writer.suspended {
        return Err("mesh store suspended for handoff".into());
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
        writer.open(
            &crate::config::state_dir().join("mesh-mail.sqlite"),
            minimum,
        )?;
    }
    Ok(())
}

/// Keep the last known generation and refuse workers until recovery completes.
pub(crate) fn failed(reason: String) {
    if let Ok(mut writer) = writer().lock() {
        writer.store = None;
        writer.suspended = true;
        writer.reason = Some(reason);
    }
}

pub(crate) fn recovery_reason() -> Option<String> {
    writer()
        .lock()
        .ok()
        .and_then(|writer| writer.reason.clone())
}

pub(crate) fn recovered() {
    if let Ok(mut writer) = writer().lock() {
        writer.reason = None;
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
