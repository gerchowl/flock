//! Journal filesystem boundary: standard I/O in production, counted I/O in tests.
#[cfg(not(test))]
pub(super) use crate::platform::{file_lock::LockFile, structured as files};
#[cfg(not(test))]
pub(super) use ::std::fs::{remove_file, rename, File};
#[cfg(test)]
pub(super) use counted::std::fs::{self, remove_file, rename};
#[cfg(test)]
pub(in crate::web::wire) use counted::Observation;
#[cfg(test)]
pub(super) use counted::{files, std, File, LockFile};

/// Resolve the journal root inside the filesystem boundary.
pub(super) fn canonical_path(path: &::std::path::Path) -> ::std::io::Result<::std::path::PathBuf> {
    path.canonicalize()
}

/// Sync the containing directory without exposing a new file constructor.
pub(super) fn sync_directory(path: &::std::path::Path) -> ::std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod counted {
    use ::std::{
        collections::HashMap,
        io::{self, Read, Seek, Write},
        path::{Path, PathBuf},
        sync::{Arc, Mutex, OnceLock, Weak},
    };

    #[derive(Clone, Copy, Debug, Default)]
    pub(in crate::web::wire) struct Counts {
        pub appended: u64,
        pub compacted: u64,
        pub other: u64,
        pub renames: usize,
    }
    #[derive(Default)]
    struct Registry {
        observers: HashMap<PathBuf, Weak<ObservationState>>,
        handles: Vec<Weak<Mutex<PathBuf>>>,
    }
    fn registry() -> &'static Mutex<Registry> {
        static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
        REGISTRY.get_or_init(Default::default)
    }
    #[derive(Default)]
    struct ObservationState {
        counts: Mutex<Counts>,
        changed: tokio::sync::Notify,
    }
    #[derive(Clone)]
    pub(in crate::web::wire) struct Observation(Arc<ObservationState>);
    impl Observation {
        pub(in crate::web::wire) fn start(root: PathBuf) -> Self {
            let state = Arc::new(ObservationState::default());
            let mut registry = registry().lock().unwrap();
            registry
                .observers
                .retain(|_, state| state.strong_count() != 0);
            assert!(registry
                .observers
                .insert(root, Arc::downgrade(&state))
                .is_none());
            Self(state)
        }
        pub(in crate::web::wire) fn sample(&self) -> Counts {
            *self.0.counts.lock().unwrap()
        }
        pub(in crate::web::wire) async fn renamed(&self) {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.sample().renames != 0 {
                    return;
                }
                changed.await;
            }
        }
    }
    fn observe(path: &Path, update: impl Fn(&mut Counts)) {
        let registry = registry().lock().unwrap();
        for (root, observer) in &registry.observers {
            if path.starts_with(root) {
                if let Some(observer) = observer.upgrade() {
                    update(&mut observer.counts.lock().unwrap());
                    observer.changed.notify_waiters();
                }
            }
        }
    }

    /// No raw file escapes this boundary. Platform adapters below unwrap internally.
    pub(in crate::web::wire::commands::journal) struct File {
        raw: ::std::fs::File,
        path: Arc<Mutex<PathBuf>>,
    }
    impl File {
        fn wrap(raw: ::std::fs::File, path: &Path) -> Self {
            let path = Arc::new(Mutex::new(path.to_owned()));
            let mut registry = registry().lock().unwrap();
            registry.handles.retain(|path| path.strong_count() != 0);
            registry.handles.push(Arc::downgrade(&path));
            Self { raw, path }
        }
        pub(in crate::web::wire::commands::journal) fn open(
            path: impl AsRef<Path>,
        ) -> io::Result<Self> {
            Ok(Self::wrap(
                ::std::fs::File::open(path.as_ref())?,
                path.as_ref(),
            ))
        }
        pub(in crate::web::wire::commands::journal) fn create(
            path: impl AsRef<Path>,
        ) -> io::Result<Self> {
            Ok(Self::wrap(
                ::std::fs::File::create(path.as_ref())?,
                path.as_ref(),
            ))
        }
        pub(in crate::web::wire::commands::journal) fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                raw: self.raw.try_clone()?,
                path: self.path.clone(),
            })
        }
        pub(in crate::web::wire::commands::journal) fn metadata(
            &self,
        ) -> io::Result<::std::fs::Metadata> {
            self.raw.metadata()
        }
        pub(in crate::web::wire::commands::journal) fn set_len(&self, size: u64) -> io::Result<()> {
            self.raw.set_len(size)
        }
        pub(in crate::web::wire::commands::journal) fn sync_all(&self) -> io::Result<()> {
            self.raw.sync_all()
        }
        pub(in crate::web::wire::commands::journal) fn sync_data(&self) -> io::Result<()> {
            self.raw.sync_data()
        }
    }
    impl Read for File {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.raw.read(bytes)
        }
    }
    impl Seek for File {
        fn seek(&mut self, at: io::SeekFrom) -> io::Result<u64> {
            self.raw.seek(at)
        }
    }
    impl Write for File {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = self.raw.write(bytes)?;
            let path = self.path.lock().unwrap().clone();
            observe(&path, |counts| {
                match path.file_name().and_then(|s| s.to_str()) {
                    Some("commands.jsonl") => counts.appended += count as u64,
                    Some("commands.compact") => counts.compacted += count as u64,
                    _ => counts.other += count as u64,
                }
            });
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.raw.flush()
        }
    }

    pub(in crate::web::wire::commands::journal) struct OpenOptions(::std::fs::OpenOptions);
    impl OpenOptions {
        pub(in crate::web::wire::commands::journal) fn new() -> Self {
            Self(::std::fs::OpenOptions::new())
        }
        pub(in crate::web::wire::commands::journal) fn write(&mut self, value: bool) -> &mut Self {
            self.0.write(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn read(&mut self, value: bool) -> &mut Self {
            self.0.read(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn append(&mut self, value: bool) -> &mut Self {
            self.0.append(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn create_new(
            &mut self,
            value: bool,
        ) -> &mut Self {
            self.0.create_new(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn create(&mut self, value: bool) -> &mut Self {
            self.0.create(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn truncate(
            &mut self,
            value: bool,
        ) -> &mut Self {
            self.0.truncate(value);
            self
        }
        pub(in crate::web::wire::commands::journal) fn open(
            &self,
            path: impl AsRef<Path>,
        ) -> io::Result<File> {
            Ok(File::wrap(self.0.open(path.as_ref())?, path.as_ref()))
        }
    }
    pub(in crate::web::wire::commands::journal) fn write(
        path: impl AsRef<Path>,
        bytes: impl AsRef<[u8]>,
    ) -> io::Result<()> {
        File::create(path)?.write_all(bytes.as_ref())
    }
    pub(in crate::web::wire::commands::journal) fn copy(
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> io::Result<u64> {
        let mut source = File::open(from)?;
        let metadata = source.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy source is not a regular file",
            ));
        }
        let mut destination = File::create(to)?;
        let bytes = io::copy(&mut source, &mut destination)?;
        destination.raw.set_permissions(metadata.permissions())?;
        Ok(bytes)
    }
    pub(in crate::web::wire::commands::journal) fn rename(
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> io::Result<()> {
        ::std::fs::rename(from.as_ref(), to.as_ref())?;
        {
            let registry = registry().lock().unwrap();
            for path in registry.handles.iter().filter_map(Weak::upgrade) {
                let mut path = path.lock().unwrap();
                if *path == from.as_ref() {
                    *path = to.as_ref().to_owned();
                }
            }
        }
        observe(to.as_ref(), |counts| counts.renames += 1);
        Ok(())
    }

    // These aliases redirect all journal-local standard and platform filesystem APIs
    // in test builds, including a new std::fs::write or OpenOptions handle.
    pub(in crate::web::wire::commands::journal) mod std {
        pub use ::std::*;
        pub mod fs {
            pub(in crate::web::wire::commands::journal) use super::super::{
                copy, rename, write, File, OpenOptions,
            };
            pub use ::std::fs::*;
        }
    }
    pub(in crate::web::wire::commands::journal) mod files {
        use super::*;
        pub(crate) use crate::platform::structured::{check_private_dir, create_private_dir};
        pub fn create_private_file(path: &Path) -> io::Result<File> {
            Ok(File::wrap(
                crate::platform::structured::create_private_file(path)?,
                path,
            ))
        }
        pub fn open_append_file(path: &Path) -> io::Result<File> {
            Ok(File::wrap(
                crate::platform::structured::open_append_file(path)?,
                path,
            ))
        }
        pub fn rollout_identity(
            file: &File,
            path: &Path,
        ) -> io::Result<crate::platform::structured::FileIdentity> {
            crate::platform::structured::rollout_identity(&file.raw, path)
        }
        pub fn read_file_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
            crate::platform::structured::read_file_at(&file.raw, bytes, offset)
        }
    }
    pub(in crate::web::wire::commands::journal) struct LockFile(
        crate::platform::file_lock::LockFile,
    );
    impl LockFile {
        pub(in crate::web::wire::commands::journal) fn new(file: File) -> Self {
            Self(crate::platform::file_lock::LockFile::new(file.raw))
        }
        pub(in crate::web::wire::commands::journal) fn try_lock(
            &self,
        ) -> Result<(), ::std::fs::TryLockError> {
            self.0.try_lock()
        }
    }

    #[test]
    fn counts_new_handles_clones_copies_and_replacement() {
        let root =
            crate::structured::launch::SessionConfigDir::create(&::std::env::temp_dir()).unwrap();
        let observation = Observation::start(root.path().to_owned());
        let primary = root.path().join("commands.jsonl");
        let compact = root.path().join("commands.compact");
        let mut original = std::fs::File::create(&primary).unwrap();
        original.write_all(b"a").unwrap();
        std::fs::write(&primary, b"bb").unwrap();
        let mut extra = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .create(true)
            .truncate(false)
            .create_new(false)
            .open(&primary)
            .unwrap();
        extra.write_all(b"c").unwrap();
        assert_eq!(std::fs::copy(&primary, &compact).unwrap(), 3);
        let mut replacement = OpenOptions::new()
            .append(true)
            .open(&compact)
            .unwrap()
            .try_clone()
            .unwrap();
        rename(&compact, &primary).unwrap();
        replacement.write_all(b"d").unwrap();
        let counts = observation.sample();
        assert_eq!(counts.appended, 5);
        assert_eq!(counts.compacted, 3);
        assert_eq!(counts.other, 0);
        assert_eq!(counts.renames, 1);
    }
}
