//! Fixed-prefix copying and bounded publication. Opaque journals stay untouched.
#[cfg(test)]
use super::std;
use super::*;
/// Growth threshold; expired claims independently trigger sweeps without new bytes.
pub(super) const THRESHOLD: u64 = 8 * 1024 * 1024;
/// The writer copies at most two bounded records while publishing.
const TAIL_BYTES: u64 = (ENTRY_BYTES * 2) as u64;
struct Positioned {
    file: File,
    offset: u64,
}
impl Read for Positioned {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = files::read_file_at(&self.file, bytes, self.offset)?;
        self.offset += count as u64;
        Ok(count)
    }
}
pub(super) struct Snapshot {
    reader: io::BufReader<Positioned>,
    file: File,
    index: Index,
    offset: u64,
    bytes: u64,
    now: u64,
    limit: usize,
    #[cfg(test)]
    hook: Option<fn(&str)>,
}
pub(super) enum Continue {
    CatchUp(Snapshot, u64),
    Retire(Index),
}
impl Snapshot {
    fn copy_to(&mut self, target: u64) -> io::Result<()> {
        while self.offset < target {
            let mut line = Vec::new();
            self.reader
                .by_ref()
                .take((target - self.offset).min(ENTRY_BYTES as u64 + 1))
                .read_until(b'\n', &mut line)?;
            if line.last() != Some(&b'\n') || line.len() > ENTRY_BYTES {
                return Err(io::Error::other("invalid compaction prefix"));
            }
            self.offset += line.len() as u64;
            let record = Record::decode(&line)
                .ok_or_else(|| io::Error::other("opaque compaction record"))?;
            if self.now.saturating_sub(record.admitted_ms)
                < super::super::super::IDEMPOTENCY_WINDOW_MS
            {
                if !self.index.fold(record, self.limit) {
                    return Err(io::Error::other("invalid compaction record"));
                }
                self.file.write_all(&line)?;
                #[cfg(test)]
                self.checkpoint("row_written");
                self.bytes += line.len() as u64;
            }
        }
        Ok(())
    }
    #[cfg(test)]
    fn checkpoint(&self, step: &str) {
        if let Some(hook) = self.hook {
            hook(step);
        }
    }
}
impl Journal {
    pub(super) fn start_compaction(&mut self, now: u64, tx: mpsc::SyncSender<Message>) {
        let expired = self.index.oldest_ms.is_some_and(|oldest| {
            now.saturating_sub(oldest) >= super::super::super::IDEMPOTENCY_WINDOW_MS
        });
        if self.blocked
            || self.compaction.is_some()
            || (self.bytes < self.next_compaction && !expired)
        {
            return;
        }
        if let Err(error) = files::rollout_identity(&self.file, &self.root.join("commands.jsonl")) {
            self.blocked = true;
            unavailable(&error);
            return;
        }
        let source = match self.file.try_clone() {
            Ok(file) => file,
            Err(error) => {
                unavailable(&error);
                return;
            }
        };
        let root = self.root.clone();
        let prefix = self.bytes;
        let limit = self.global_limit;
        #[cfg(test)]
        let hook = self.compaction_hook;
        let (advance, control) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("wire-compact".into())
            .spawn(move || {
                let initial = (|| -> io::Result<Snapshot> {
                    let path = root.join("commands.compact");
                    match filesystem::remove_file(&path) {
                        Ok(()) => (),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e),
                    }
                    drop(files::create_private_file(&path)?);
                    let mut snapshot = Snapshot {
                        reader: io::BufReader::new(Positioned {
                            file: source,
                            offset: 0,
                        }),
                        file: files::open_append_file(&path)?,
                        // Reserve on the copier so the bounded publication tail cannot rehash.
                        index: Index {
                            records: HashMap::with_capacity(limit),
                            counts: HashMap::with_capacity(limit),
                            oldest_ms: None,
                        },
                        offset: 0,
                        bytes: 0,
                        now,
                        limit,
                        #[cfg(test)]
                        hook,
                    };
                    #[cfg(test)]
                    snapshot.checkpoint("created");
                    snapshot.copy_to(prefix)?;
                    #[cfg(test)]
                    snapshot.checkpoint("prefix_copied");
                    Ok(snapshot)
                })();
                if tx.send(Message::Compacted(initial)).is_err() {
                    return;
                }
                while let Ok(next) = control.recv() {
                    match next {
                        Continue::CatchUp(mut snapshot, target) => {
                            let result = snapshot.copy_to(target).map(|()| snapshot);
                            if tx.send(Message::Compacted(result)).is_err() {
                                break;
                            }
                        }
                        Continue::Retire(index) => {
                            drop(index);
                            break;
                        }
                    }
                }
            });
        match worker {
            Ok(_) => self.compaction = Some(advance),
            Err(error) => unavailable(&error),
        }
        self.next_compaction = self.bytes.saturating_add(THRESHOLD);
    }
    pub(super) fn compacted(&mut self, result: io::Result<Snapshot>) {
        let Some(control) = self.compaction.take() else {
            return;
        };
        let mut snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.blocked = true;
                unavailable(&error);
                return;
            }
        };
        // A failed append may have left a torn tail. Keep the authoritative log intact.
        if self.blocked {
            let _ = control.send(Continue::Retire(snapshot.index));
            return;
        }
        if let Err(error) = files::rollout_identity(&self.file, &self.root.join("commands.jsonl")) {
            self.blocked = true;
            unavailable(&error);
            let _ = control.send(Continue::Retire(snapshot.index));
            return;
        }
        if self.bytes.saturating_sub(snapshot.offset) > TAIL_BYTES {
            if control
                .send(Continue::CatchUp(snapshot, self.bytes))
                .is_ok()
            {
                self.compaction = Some(control);
            }
            return;
        }
        let published = (|| -> io::Result<()> {
            snapshot.copy_to(self.bytes)?;
            #[cfg(test)]
            snapshot.checkpoint("catchup_copied");
            snapshot.file.sync_all()?;
            #[cfg(test)]
            snapshot.checkpoint("synced");
            filesystem::rename(
                self.root.join("commands.compact"),
                self.root.join("commands.jsonl"),
            )?;
            #[cfg(test)]
            snapshot.checkpoint("renamed");
            // Adopt the renamed file even if directory sync fails, then fence writes.
            #[cfg(test)]
            let file = Stream::from(snapshot.file.try_clone()?);
            #[cfg(not(test))]
            let file = snapshot.file.try_clone()?;
            self.file = file;
            self.bytes = snapshot.bytes;
            std::mem::swap(&mut self.index, &mut snapshot.index);
            filesystem::sync_directory(&self.root)?;
            #[cfg(test)]
            snapshot.checkpoint("directory_synced");
            Ok(())
        })();
        if let Err(error) = published {
            self.blocked = true;
            unavailable(&error);
        }
        self.next_compaction = self.bytes.saturating_add(THRESHOLD);
        // Destruction of the old index is also off the writer's request path.
        let _ = control.send(Continue::Retire(snapshot.index));
    }
}
