use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const TEMP_PREFIX: &str = ".flock-write-";
const ORPHAN_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) fn replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing destination directory"))?;
    let temp = parent.join(format!(
        "{TEMP_PREFIX}{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        // The rename is only durable once the directory entry is, so a crash
        // right after it cannot hand back the old file.
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Remove temp files [`replace`] left in `dir` when its process died between
/// creating one and renaming it. The writer's pid is in the name, so a file
/// whose process is still running is never touched, and neither is one younger
/// than [`ORPHAN_MIN_AGE`]: a live write holds its temp for milliseconds, so
/// age alone keeps a platform that cannot answer for a pid from racing it.
pub(super) fn sweep_orphans(dir: &Path) {
    let now = std::time::SystemTime::now();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(TEMP_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= ORPHAN_MIN_AGE);
        if old_enough && pid != std::process::id() && !crate::platform::process_exists(pid) {
            let _ = fs::remove_file(entry.path());
        }
    }
}
