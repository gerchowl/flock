//! Shared rules for harness session stores.
//!
//! A candidate must parse completely and carry one nonempty session identity:
//! Codex identity rows are `session_meta.payload.id`; Claude uses top-level
//! `sessionId`. Each must be a nonempty string. Every such row must agree,
//! and comparison is exact and case
//! sensitive. Missing identity is not evidence. Conflicts and malformed rows
//! refuse the scan. Follow symlinks and canonicalize every possible directory
//! or candidate, including broken links: only proven absence may be skipped.
//! Deduplicate directories and opened files by device and inode, so aliases
//! count once. Two distinct files with the requested identity are ambiguous,
//! even if one is stale. Any read, permission or canonicalization error on a
//! path that could contain the session refuses the entire lookup.
use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(crate) enum Layout {
    Codex,
    Claude,
}

fn invalid(path: &Path, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid session store {}: {message}", path.display()),
    )
}

// lstat distinguishes an absent name from a dangling link. If an ancestor is
// missing too, verify the nearest existing ancestor before proving absence.
fn resolve(path: &Path) -> io::Result<Option<(PathBuf, Metadata)>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let canonical = fs::canonicalize(path).map_err(|err| contextual(path, err))?;
            let metadata = fs::metadata(&canonical).map_err(|err| contextual(path, err))?;
            Ok(Some((canonical, metadata)))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent().filter(|parent| *parent != path) {
                resolve(parent)?;
            }
            Ok(None)
        }
        Err(err) => Err(contextual(path, err)),
    }
}
pub(crate) fn contextual(path: &Path, err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!("cannot read session store {}: {err}", path.display()),
    )
}

pub(crate) fn recorded_profile(record: &Path) -> io::Result<Option<PathBuf>> {
    if resolve(record)?.is_none() {
        return Ok(None);
    }
    let raw = fs::read_to_string(record).map_err(|err| contextual(record, err))?;
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        return Err(invalid(
            record,
            "profile path must be absolute and nonempty",
        ));
    }
    Ok(Some(path))
}

pub(crate) fn lookup(
    roots: impl IntoIterator<Item = PathBuf>,
    layout: Layout,
    id: &str,
) -> io::Result<Option<PathBuf>> {
    if id.is_empty()
        || id.contains(['/', '\\'])
        || id.chars().any(char::is_control)
        || matches!(id, "." | "..")
    {
        return Ok(None);
    }
    let mut scan = Scan {
        layout,
        id,
        directories: HashSet::new(),
        files: HashSet::new(),
        found: None,
    };
    for root in roots {
        scan.directory(&root)?;
    }
    Ok(scan.found)
}
struct Scan<'a> {
    layout: Layout,
    id: &'a str,
    directories: HashSet<(u64, u64)>,
    files: HashSet<(u64, u64)>,
    found: Option<PathBuf>,
}
impl Scan<'_> {
    fn directory(&mut self, path: &Path) -> io::Result<()> {
        let Some((canonical, metadata)) = resolve(path)? else {
            return Ok(());
        };
        if !metadata.is_dir() {
            return Err(invalid(path, "store root is not a directory"));
        }
        if !self
            .directories
            .insert(crate::platform::file_identity(&metadata))
        {
            return Ok(());
        }
        for entry in fs::read_dir(path).map_err(|err| contextual(path, err))? {
            let entry = entry.map_err(|err| contextual(path, err))?;
            let child = entry.path();
            let Some((_, metadata)) = resolve(&child)? else {
                return Err(invalid(&child, "entry vanished during enumeration"));
            };
            match self.layout {
                Layout::Codex if metadata.is_dir() => self.directory(&child)?,
                Layout::Codex => {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("rollout-")
                        && name.ends_with(&format!("-{}.jsonl", self.id))
                    {
                        self.candidate(&child)?;
                    }
                }
                Layout::Claude if metadata.is_dir() => {
                    self.candidate(&child.join(format!("{}.jsonl", self.id)))?
                }
                Layout::Claude => {}
            }
        }
        // Keep the canonical directory in the proof until enumeration ends.
        let current = fs::metadata(&canonical).map_err(|err| contextual(path, err))?;
        if crate::platform::file_identity(&current) != crate::platform::file_identity(&metadata) {
            return Err(invalid(path, "directory changed during enumeration"));
        }
        Ok(())
    }
    fn candidate(&mut self, path: &Path) -> io::Result<()> {
        let Some((canonical, metadata)) = resolve(path)? else {
            return Ok(());
        };
        if !metadata.is_file() {
            return Err(invalid(path, "candidate is not a regular file"));
        }
        let file = File::open(path).map_err(|err| contextual(path, err))?;
        let opened = file.metadata().map_err(|err| contextual(path, err))?;
        let identity = crate::platform::file_identity(&opened);
        if identity != crate::platform::file_identity(&metadata) {
            return Err(invalid(path, "candidate changed before read"));
        }
        if !self.files.insert(identity) {
            return Ok(());
        }
        let mut session = None;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|err| contextual(path, err))?;
            let row: serde_json::Value =
                serde_json::from_str(&line).map_err(|err| invalid(path, &err.to_string()))?;
            let field = match self.layout {
                Layout::Codex if row["type"] == "session_meta" => Some(&row["payload"]["id"]),
                Layout::Claude => row.get("sessionId"),
                Layout::Codex => None,
            };
            if let Some(field) = field {
                let id = field
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid(path, "missing session identity"))?;
                if session.as_deref().is_some_and(|previous| previous != id) {
                    return Err(invalid(path, "conflicting session identities"));
                }
                session = Some(id.to_owned());
            }
        }
        let current = fs::metadata(&canonical).map_err(|err| contextual(path, err))?;
        if crate::platform::file_identity(&current) != identity {
            return Err(invalid(path, "candidate changed during read"));
        }
        let session = session.ok_or_else(|| invalid(path, "missing session identity"))?;
        if session == self.id && self.found.replace(path.to_path_buf()).is_some() {
            return Err(invalid(path, "session id matches multiple transcripts"));
        }
        Ok(())
    }
}
