#!/usr/bin/env python3
"""Keep journal file I/O behind the counted filesystem boundary (#3203).

This is an architecture gate, not a Rust sandbox. The journal reserves `fs`,
`OpenOptions`, and file constructors for filesystem.rs. Imports and qualified
paths are checked alike, so aliases, grouped imports, raw identifiers and leading
`::` cannot opt out. Outside imports and qualified paths require a reviewed allowlist
entry, including macros. External globs, namespace injection, path overrides and
included Rust code and symlinks are refused. Only enumerated existing internal globs are allowed.
Comments and literals are ignored. There is no escape marker.

Known limits: this is a text scanner, not a Rust parser or type checker. It does
not infer receiver types, expand arbitrary macros, or model trait dispatch and
generic instantiation. The independent method-name layer defends receiver-typed
calls even when path resolution cannot identify their origin. References to
associated capability functions are refused too, preventing an indirect call
through a function value. Exact call-text
exceptions trust the reviewed receiver bindings in those source files; they are
not proof of Rust type identity. Alias collection covers the whole file and its
lexical/inline-module scopes before uses are checked. Unsupported source and
namespace injection constructs fail closed as documented above.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COMMANDS = Path("src/web/wire/commands")
BOUNDARY = COMMANDS / "journal/filesystem.rs"
IDENT = re.compile(r"(?:r#)?[A-Za-z_]\w*")
RAW = re.compile(r'(?:b|c)?r(#+|)"')
CHAR = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def tokens(text: str) -> list[tuple[str, int]]:
    """Rust code tokens with source offsets, excluding nested comments/literals."""
    result = []
    i = 0
    while i < len(text):
        if text[i].isspace():
            i += 1
        elif text.startswith("//", i):
            end = text.find("\n", i)
            i = len(text) if end < 0 else end
        elif text.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(text) and depth:
                if text.startswith("/*", i):
                    depth += 1
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
        elif match := RAW.match(text, i):
            end = text.find('"' + match[1], match.end())
            i = len(text) if end < 0 else end + 1 + len(match[1])
        elif text[i] == '"':
            i += 1
            while i < len(text):
                if text[i] == "\\":
                    i += 2
                elif text[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
        elif match := CHAR.match(text, i):
            i = match.end()
        elif match := IDENT.match(text, i):
            result.append((match[0].removeprefix("r#"), i))
            i = match.end()
        else:
            token = "::" if text.startswith("::", i) else text[i]
            result.append((token, i))
            i += len(token)
    return result


# Each entry is a reviewed item or an explicitly safe standard/library namespace.
# Descendants cover associated items (or the explicitly named safe namespace).
ALLOWLIST = {
    'std::collections::HashMap': 'journal indexes',
    'std::sync': 'in-memory synchronization and channels',
    'std::task::Poll': 'test readiness assertions',
    'std::time': 'stall windows and test deadlines',
    'std::thread': 'journal workers and test coordination',
    'std::mem::swap': 'publish the compacted index',
    'std::future::Future': 'ownership asynchronous result type',
    'serde::Deserialize': 'decode journal records',
    'serde::Serialize': 'encode journal records',
    'serde_json::Value': 'stored command arguments and outcomes',
    'serde_json::from_slice': 'decode persisted journal records',
    'serde_json::from_str': 'decode test journal records',
    'serde_json::from_value': 'decode stored acknowledgements',
    'serde_json::to_value': 'encode command arguments and outcomes',
    'serde_json::to_vec': 'encode persisted journal records',
    'serde_json::to_writer': 'stream journal encoding',
    'serde_json::json': 'archive records and test payloads',
    'sha2::Digest': 'command argument digest trait',
    'sha2::Sha256': 'command argument identity',
    'tokio::sync': 'ownership signals and asynchronous locks',
    'tokio::time': 'bounded ownership waits',
    'tokio::task::JoinHandle': 'ownership recovery worker handle',
    'tokio::task::yield_now': 'test worker coordination',
    'tokio::spawn': 'ownership recovery task',
    'tokio::pin': 'ownership wait futures',
    'tokio::select': 'ownership progress and stall arbitration',
    'parking_lot::Mutex': 'in-memory worker state and test hooks',
    'tracing::warn': 'journal preservation and lifecycle diagnostics',
    'futures_util::poll': 'test future readiness',
    'crate::app::notifications::now_ms': 'journal admission timestamps',
    'crate::logging::client_wire_unavailable': 'journal availability diagnostics',
    'crate::logging::client_journal_replay': 'journal replay progress diagnostics',
    'crate::web::wire::commands::ack': 'wire acknowledgement construction',
    'crate::web::wire::commands::Command': 'journal command input',
    'crate::web::wire::commands::ErrorCode': 'typed journal refusals',
    'crate::web::wire::commands::ServerFrame': 'stored acknowledgement type',
    'crate::web::wire::CREDENTIAL_CONNECTIONS': 'journal capacity bound',
    'crate::web::wire::COMMANDS_PER_SOCKET': 'journal capacity bound',
    'crate::web::wire::CONNECTIONS': 'journal global capacity bound',
    'crate::web::wire::MAX_MESSAGE': 'journal record size bound',
    'crate::web::wire::IDEMPOTENCY_WINDOW_MS': 'journal retention window',
}
# Path values are needed in signatures and imports, but this does not authorize
# filesystem methods. Namespace imports similarly grant no descendants.
TYPE_ONLY_ALLOWLIST = {
    'std::path::Path': 'journal path arguments, type references only',
    'std::path::PathBuf': 'owned journal paths, type references only',
    'std::io': 'namespace for the individual stream adapters below',
}
ALLOWLIST.update({
    'std::path::Path::new': 'pure path borrowing',
    'std::io::BufRead': 'counted stream trait',
    'std::io::Read': 'counted stream trait',
    'std::io::Write': 'counted stream trait',
    'std::io::Seek': 'counted stream positioning',
    'std::io::SeekFrom': 'stream offset values',
    'std::io::BufReader': 'buffer an existing counted stream',
    'std::io::BufWriter': 'buffer an existing counted stream',
    'std::io::Error': 'typed stream errors',
    'std::io::ErrorKind': 'typed stream error classification',
    'std::io::Result': 'stream result alias',
})
# These capabilities are restricted by source path, never a caller-supplied cfg.
TEST_ONLY_ALLOWLIST = {
    'std::ops::Deref': 'test stream wrapper',
    'std::env::temp_dir': 'isolated test fixture location',
    'std::env::current_exe': 'journal subprocess test executable',
    'std::env::var': 'journal subprocess test selection',
    'std::process::exit': 'journal subprocess test completion',
    'tokio::test': 'ownership asynchronous tests',
    'tokio::runtime::Builder': 'ownership test runtime',
    'crate::logging::capture_logs': 'test replay diagnostics',
    'crate::structured::launch::SessionConfigDir': 'isolated test directory lifetime',
    'crate::process::TracedCommand': 'journal subprocess test launcher',
    'crate::cli::exit::SUCCESS': 'journal subprocess expected status',
    'crate::web::wire::tests::runtime': 'test asynchronous runtime',
}
# Keep precisely one existing `use super::*` in each listed module. New globs,
# nested copies and duplicates require an explicit review here.
EXISTING_GLOBS = {
    COMMANDS / 'journal/compact.rs': 'compaction shares journal types and counted adapters',
    COMMANDS / 'journal/tests.rs': 'journal fixture access to private journal state',
    COMMANDS / 'journal/tests/capacity.rs': 'capacity fixtures share journal test helpers',
    COMMANDS / 'journal/tests/compaction.rs': 'compaction fixtures share journal test helpers',
    COMMANDS / 'ownership/tests.rs': 'ownership fixtures inspect private lifecycle state',
}
# Keys are (definition source path, macro name), with a review reason as value.
# No crate-defined macros currently need to cross into the journal.
CRATE_MACRO_ALLOWLIST = {}

# Independent of path/alias resolution: these std and Tokio names can perform
# filesystem or process operations on a value whose type the scanner cannot infer.
CAPABILITY_METHODS = frozenset({
    'read_dir', 'metadata', 'symlink_metadata', 'exists', 'try_exists',
    'is_file', 'is_dir', 'is_symlink', 'canonicalize', 'read_link',
    'read_to_string', 'read', 'write', 'create', 'create_new', 'open',
    'remove_file', 'remove_dir', 'remove_dir_all', 'create_dir', 'create_dir_all',
    'rename', 'copy', 'hard_link', 'set_permissions', 'spawn', 'output', 'status',
    # Stream, file-handle and lock operations also need counted-wrapper review.
    'read_exact', 'read_to_end', 'read_vectored', 'read_buf', 'read_buf_exact',
    'write_all', 'write_fmt', 'write_vectored', 'write_buf', 'write_all_buf',
    'flush', 'seek', 'rewind', 'stream_position', 'set_len', 'sync_all', 'sync_data',
    'try_clone', 'lock', 'lock_shared', 'try_lock', 'try_lock_shared', 'unlock',
    'set_modified', 'set_accessed', 'set_times',
    # Process management and current-directory mutation are capabilities too.
    'kill', 'wait', 'wait_with_output', 'try_wait', 'exec', 'set_current_dir',
})


def call_span(code, index):
    """Return the exact receiver/associated-call source span, including arguments."""
    names = [name for name, _ in code]
    if names[index - 1:index] not in (['.'], ['::']):
        return None
    opening = index + 1
    if names[opening:opening + 2] == ['::', '<']:
        opening += 2
        depth = 1
        while opening < len(names) and depth:
            depth += (names[opening] == '<') - (
                names[opening] == '>' and names[opening - 1] != '-')
            opening += 1
    if names[opening:opening + 1] != ['(']:
        # A capability-bearing generic reference that cannot be resolved must
        # not silently opt out of the independent call-name layer.
        return (code[index][1], code[index][1] + len(names[index])) if opening > index + 1 else None

    def matching(end):
        pairs = {')': '(', ']': '[', '}': '{', '>': '<'}
        left, depth = pairs[names[end]], 1
        cursor = end - 1
        while cursor >= 0:
            depth += (names[cursor] == names[end]) - (names[cursor] == left)
            if not depth:
                return cursor
            cursor -= 1
        return 0

    def receiver(end):
        if end < 0:
            return 0
        if names[end] == '?':
            return receiver(end - 1)
        start = matching(end) if names[end] in {')', ']', '}', '>'} else end
        if start and names[end] in {')', ']'} and (
                IDENT.fullmatch(names[start - 1]) or names[start - 1] in {')', ']', '>'}):
            start = receiver(start - 1)
        if start >= 2 and names[start - 1] in {'.', '::'}:
            start = receiver(start - 2)
        return start

    start = receiver(index - 2)
    stack = ['(']
    end = opening + 1
    pairs = {')': '(', ']': '[', '}': '{'}
    while end < len(names) and stack:
        if names[end] in {'(', '[', '{'}:
            stack.append(names[end])
        elif names[end] in pairs:
            if stack[-1] != pairs[names[end]]:
                return None
            stack.pop()
        end += 1
    if stack:
        return None
    return code[start][1], code[end - 1][1] + 1


def method_findings(text, code, path):
    if path == BOUNDARY or test_source(path):
        return []
    findings, seen = [], {}
    for i, (name, offset) in enumerate(code):
        if name not in CAPABILITY_METHODS:
            continue
        span = call_span(code, i)
        if span is None:
            if i and code[i - 1][0] == '::':
                findings.append((text.count('\n', 0, offset) + 1,
                                 f'associated capability reference requires review: {name}'))
            continue
        key = (path, text[span[0]:span[1]])
        seen[key] = seen.get(key, 0) + 1
        exception = CALL_EXCEPTIONS.get(key)
        if exception is None or seen[key] > exception[1]:
            findings.append((text.count('\n', 0, offset) + 1,
                             f'filesystem/process-capable call requires an exact exception: {name}'))
    return findings

# Each key is the file plus the complete, literal call text (including arguments
# and closure bodies). Values record the reason and maximum existing occurrences.
# Receiver bindings are trusted only for these reviewed source expressions.
CALL_EXCEPTIONS = {
    (COMMANDS / 'journal/compact.rs', r'''self.file.write_all(&line)'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal/compact.rs', r'''self.file.try_clone()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal/compact.rs', r'''std::thread::Builder::new()
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
            })'''): (
        'existing journal worker thread, not a child process; body scanned independently', 1),
    (COMMANDS / 'journal/compact.rs', r'''filesystem::remove_file(&path)'''): (
        'existing counted filesystem boundary adapter', 1),
    (COMMANDS / 'journal/compact.rs', r'''snapshot.file.sync_all()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal/compact.rs', r'''filesystem::rename(
                self.root.join("commands.compact"),
                self.root.join("commands.jsonl"),
            )'''): (
        'existing counted filesystem boundary adapter', 1),
    (COMMANDS / 'journal/compact.rs', r'''snapshot.file.try_clone()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 2),
    (COMMANDS / 'journal.rs', r'''self.source.read(bytes)'''): (
        'existing replay stream delegates to the counted File', 1),
    (COMMANDS / 'journal.rs', r'''Self::spawn(
            move || {
                let mut journal = Journal::lease(&data)?;
                journal.replay_progress = progress;
                journal.replay_advanced(0);
                if let Some(acquired) = acquired {
                    let _ = acquired.send(());
                }
                #[cfg(test)]
                tests::replay_barrier(&data)?;
                journal.replay()?;
                Ok(journal)
            },
            wake,
        )'''): (
        'existing journal Writer factory; closure uses the counted boundary', 1),
    (COMMANDS / 'journal.rs', r'''std::thread::Builder::new()
            .name("wire-journal".into())
            .spawn(move || {
                if let Ok(mut journal) = open().inspect_err(unavailable) {
                    let _ = opened.send(Ok(()));
                    while let Ok(message) = rx.recv() {
                        match message {
                            Message::Job((operation, reply)) => {
                                let now = match &operation {
                                    Operation::Claim { now, .. } => Some(*now),
                                    _ => None,
                                };
                                let result = journal.apply(operation);
                                // Counted I/O has no published completion yet.
                                worker_pending.fetch_sub(1, Ordering::AcqRel);
                                let _ = reply.send(result);
                                if let Some(now) = now {
                                    if let Some(admission) = weak.upgrade() {
                                        let tx = admission.lock().as_ref().cloned();
                                        if let Some(tx) = tx {
                                            journal.start_compaction(now, tx);
                                        }
                                    }
                                }
                            }
                            Message::Compacted(result) => journal.compacted(result),
                        }
                        wake.notify_one();
                    }
                    // The lease is released before shutdown completion becomes observable.
                    drop(journal);
                } else {
                    let _ = opened.send(Err(ErrorCode::SessionBusy));
                }
                closed.send_replace(true);
            })'''): (
        'existing journal worker thread, not a child process; body scanned independently', 1),
    (COMMANDS / 'journal.rs', r'''admission.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'journal.rs', r'''self
            .admission
            .lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'journal.rs', r'''self.admission.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'journal.rs', r'''file.sync_all()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 2),
    (COMMANDS / 'journal.rs', r'''lease.try_lock()'''): (
        'existing counted LockFile wrapper', 1),
    (COMMANDS / 'journal.rs', r'''file.metadata()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 2),
    (COMMANDS / 'journal.rs', r'''self.file.try_clone()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 3),
    (COMMANDS / 'journal.rs', r'''file.write_all(b"\n")'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 3),
    (COMMANDS / 'journal.rs', r'''file.flush()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''file.get_ref().sync_all()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.try_clone()?.seek(SeekFrom::Start(0))'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.metadata()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''file.write_all(&archive)'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.set_len(good)'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.sync_all()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 2),
    (COMMANDS / 'journal.rs', r'''self.file.write_all(&line[..line.len() / 2])'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.write_all(&line)'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.sync_data()'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'journal.rs', r'''self.file.set_len(before)'''): (
        'existing counted File/Stream or its buffered writer; boundary retains I/O observation', 1),
    (COMMANDS / 'ownership.rs', r'''self
            .0
            .writer
            .try_lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'ownership.rs', r'''self.0.pending.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 2),
    (COMMANDS / 'ownership.rs', r'''tokio::spawn(async move {
            let mut send = Some(send);
            let mut acquired = Some(acquired);
            loop {
                let (progress, mut advanced) = watch::channel((0, 0));
                let stalled = Arc::new(AtomicBool::new(false));
                let opening = owner.acquire_at(phase, acquired.take(), progress, stalled.clone());
                tokio::pin!(opening);
                let idle = tokio::time::sleep(REPLAY_STALL);
                tokio::pin!(idle);
                let mut observing = true;
                let result = loop {
                    tokio::select! {
                        biased;
                        result = &mut opening => break result,
                        changed = advanced.changed(), if observing => {
                            observing = changed.is_ok();
                            if observing {
                                idle.as_mut().reset(tokio::time::Instant::now() + REPLAY_STALL);
                                #[cfg(test)]
                                tests::replay_observed(&owner.0.data, *advanced.borrow());
                            }
                        }
                        _ = &mut idle => {
                            stalled.store(true, Ordering::Release);
                            let (bytes, total) = *advanced.borrow();
                            crate::logging::client_journal_replay("stalled", bytes, total);
                            if let Some(send) = send.take() {
                                let _ = send.send(Err(ErrorCode::SessionBusy));
                            }
                            owner.0.wake.notify_one();
                            // Retain the worker and its lease until it stops. A
                            // late completion must drain, never reopen admission.
                            break opening.await;
                        }
                    }
                };
                let delivered = send.take().is_some_and(|send| send.send(result).is_ok());
                // Synchronous startup already observes its reply. Only later
                // recovery or rollback needs to wake the running owner loop.
                if notify || !delivered {
                    owner.0.wake.notify_one();
                }
                if result.is_ok() || !owner.current(phase) {
                    break;
                }
                tokio::time::sleep(ACQUIRE_RETRY).await;
            }
        })'''): (
        'existing ownership future task, not a child process; body scanned independently', 1),
    (COMMANDS / 'ownership.rs', r'''self.0.writer.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'ownership.rs', r'''self.0.open_barrier.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'ownership.rs', r'''tokio::spawn(async move {
            let mut slot = owner.0.writer.lock().await;
            // An abandoned old drain must not close a newer recovered owner.
            if slot.phase <= phase {
                if let Some(writer) = slot.writer.take() {
                    writer.shutdown().await?;
                }
            }
            Ok::<_, ErrorCode>(())
        })'''): (
        'existing ownership future task, not a child process; body scanned independently', 1),
    (COMMANDS / 'ownership.rs', r'''owner.0.writer.lock()'''): (
        'existing in-memory ownership/admission mutex, not a filesystem lock', 1),
    (COMMANDS / 'ownership.rs', r'''tokio::spawn(async move { owner.wait_open(reply).await })'''): (
        'existing ownership future task, not a child process; body scanned independently', 1),
}


def test_source(path: Path) -> bool:
    relative = path.relative_to(COMMANDS)
    return any(relative == Path(tree + '/tests.rs')
               or relative.is_relative_to(Path(tree + '/tests'))
               for tree in ('journal', 'ownership'))


BUILTIN_MACROS = {
    'assert', 'assert_eq', 'assert_ne', 'debug_assert', 'debug_assert_eq',
    'debug_assert_ne', 'matches', 'format', 'vec', 'panic', 'unreachable',
    'eprintln', 'println', 'stringify', 'concat', 'env', 'option_env',
    'file', 'line', 'column', 'todo', 'unimplemented',
}
JOURNAL = ('crate', 'web', 'wire', 'commands', 'journal')
OWNERSHIP = JOURNAL[:-1] + ('ownership',)
# Built-in attributes and serde's reviewed derive helper introduce no new code.
ATTRIBUTES = {'cfg', 'derive', 'serde', 'test', 'allow', 'deny', 'warn', 'forbid', 'macro_export'}
DERIVES = {'Clone', 'Copy', 'Debug', 'Default', 'PartialEq', 'Eq', 'Hash'}


def module_path(path: Path) -> tuple[str, ...]:
    parts = path.relative_to(Path('src')).with_suffix('').parts
    return ('crate',) + (parts[:-1] if parts[-1] == 'mod' else parts)


def import_paths(names: list[str], start: int):
    """Expand use trees, retaining source indices for diagnostics and aliases."""
    entries = []

    def tree(i, prefix):
        if names[i:i + 1] == ['::']:
            i += 1
        parts = list(prefix)
        while i < len(names):
            name = names[i]
            if name == '{':
                i += 1
                while i < len(names) and names[i] != '}':
                    i = tree(i, parts)
                    if names[i:i + 1] == [',']:
                        i += 1
                return i + 1
            if not (IDENT.fullmatch(name) or name == '*'):
                raise ValueError('unsupported use tree')
            parts.append(name)
            offset_index = i
            i += 1
            if names[i:i + 1] == ['::']:
                i += 1
                continue
            alias = parts[-1]
            if names[i:i + 1] == ['as']:
                alias = names[i + 1]
                i += 2
            entries.append((parts, offset_index, alias))
            return i
        raise ValueError('unterminated use tree')

    end = tree(start, [])
    if names[end:end + 1] != [';']:
        raise ValueError('unsupported use tree')
    return entries, end + 1


class AliasIndex:
    """Collect use/type aliases before resolving any occurrence in a scope.

    Simple path aliases are followed recursively, including qualified aliases
    in inline modules. Wrapper arguments and tuple elements are still inspected
    by the ordinary path scan; generic parameters are not instantiated.
    """

    def __init__(self, names, base):
        self.scopes = [{'parent': None, 'module': base, 'aliases': {}}]
        self.at = [0] * len(names)
        self.modules = {base: 0}
        scope, i = 0, 0
        while i < len(names):
            self.at[i] = scope
            if names[i] == 'use':
                try:
                    entries, end = import_paths(names, i + 1)
                    for parts, _, alias in entries:
                        if alias != '*':
                            self.scopes[scope]['aliases'][alias] = parts
                    self.at[i:end] = [scope] * (end - i)
                    i = end
                    continue
                except (ValueError, IndexError):
                    pass  # The checking pass reports unsupported imports.
            elif names[i] == 'type' and names[i + 2:i + 3] == ['=']:
                parts, end = [], i + 3
                if names[end:end + 1] == ['::']:
                    end += 1
                while end < len(names) and IDENT.fullmatch(names[end]):
                    parts.append(names[end])
                    end += 1
                    if names[end:end + 1] != ['::']:
                        break
                    end += 1
                if parts:
                    self.scopes[scope]['aliases'][names[i + 1]] = parts
            elif names[i] == '{':
                module = self.scopes[scope]['module']
                is_module = i >= 2 and names[i - 2] == 'mod'
                if is_module:
                    module += (names[i - 1],)
                self.scopes.append({'parent': scope, 'module': module, 'aliases': {}})
                scope = len(self.scopes) - 1
                if is_module:
                    self.modules[module] = scope
            elif names[i] == '}' and self.scopes[scope]['parent'] is not None:
                scope = self.scopes[scope]['parent']
            i += 1

    def visible(self, scope):
        result = {}
        module = self.scopes[scope]['module']
        while scope is not None and self.scopes[scope]['module'] == module:
            for name in self.scopes[scope]['aliases']:
                result.setdefault(name, scope)
            scope = self.scopes[scope]['parent']
        return result

    def resolve(self, parts, scope, seen=frozenset()):
        if not parts:
            return parts
        module = self.scopes[scope]['module']
        visible = self.visible(scope)
        binding = None
        if parts[0] in visible:
            binding = (visible[parts[0]], parts[0], parts[1:])
        else:
            if parts[0] == 'self':
                parts = list(module) + parts[1:]
            elif parts[0] == 'super':
                while parts and parts[0] == 'super':
                    module, parts = module[:-1], parts[1:]
                parts = list(module) + parts
            elif tuple(module + (parts[0],)) in self.modules:
                parts = list(module) + parts
            for end in range(1, len(parts)):
                owner = self.modules.get(tuple(parts[:end]))
                if owner is not None and parts[end] in self.scopes[owner]['aliases']:
                    binding = (owner, parts[end], parts[end + 1:])
                    break
        if binding is None:
            return parts
        owner, name, tail = binding
        key = (owner, name)
        if key in seen:
            # An identity module re-export is harmless; real alias cycles fail.
            identity = list(self.scopes[owner]['module']) + [name]
            return identity + tail if parts == identity + tail else ['cyclic_alias', name]
        target = self.resolve(self.scopes[owner]['aliases'][name], owner, seen | {key})
        return target + tail


def import_findings(code, path, local_macros):
    """Resolve explicit relative paths; refuse source/namespace escape constructs.

    This intentionally does not follow re-exports. Outside modules cannot be
    imported wholesale, so every new external dependency requires review here.
    Only the enumerated existing internal globs may share scanned modules.
    """
    names = [name for name, _ in code]
    findings = []
    base = module_path(path)
    module = base
    aliases = AliasIndex(names, base)
    scope = 0
    imported = aliases.visible(scope)
    i = 0
    seen_globs = set()
    allowed_items = ALLOWLIST | (TEST_ONLY_ALLOWLIST if test_source(path) else {})

    def check(parts, index):
        original = parts[:]
        parts = aliases.resolve(parts, scope)
        if parts[-1:] == ['self']:
            parts = parts[:-1]
        if not parts:
            return
        if parts[0] in {'io', 'mpsc', 'oneshot', 'watch'}:
            namespace = ['std', 'io'] if parts[0] == 'io' else (
                ['std', 'sync', 'mpsc'] if parts[0] == 'mpsc' else ['tokio', 'sync', parts[0]])
            parts = namespace + parts[1:]
        elif parts[0] in {'filesystem', 'tests', 'compact', 'journal'}:
            # Bare local modules are resolved relative to the current Rust module.
            parts = list(module) + parts
        # The counted boundary re-exports std for tests. Its non-filesystem
        # members must receive the same review as a direct std path.
        if parts[:len(JOURNAL)] == list(JOURNAL) and 'std' in parts[len(JOURNAL):]:
            tail = parts[parts.index('std', len(JOURNAL)):]
            if len(tail) > 1:
                parts = tail
        # Parent globs also expose these standard path types. Preserve the
        # type-only restriction through internal aliases and qualified names.
        for item in ('Path', 'PathBuf'):
            if parts[0] == item or (parts[:len(JOURNAL)] == list(JOURNAL) and item in parts):
                parts = ['std', 'path'] + parts[parts.index(item):]
        resolved = tuple(parts)
        internal = resolved[:len(JOURNAL)] == JOURNAL or (
            base[:len(OWNERSHIP)] == OWNERSHIP
            and resolved[:len(OWNERSHIP)] == OWNERSHIP)
        allowed = any(resolved[:len(key.split('::'))] == tuple(key.split('::'))
                      for key in allowed_items)
        allowed = allowed or '::'.join(parts) in TYPE_ONLY_ALLOWLIST
        if '*' in parts:
            existing = (path in EXISTING_GLOBS and scope == 0
                        and original == ['super', '*'] and internal
                        and path not in seen_globs)
            seen_globs.add(path)
            if not existing:
                findings.append((code[index][1], 'journal glob import is not an enumerated existing use'))
        elif not internal and not allowed:
            findings.append((code[index][1],
                             'outside journal import/path is not allowlisted: '
                             + '::'.join(original)))
        return resolved

    while i < len(names):
        scope = aliases.at[i]
        imported = aliases.visible(scope)
        module = aliases.scopes[scope]['module']
        name = names[i]
        attribute = names[max(0, i - 2):i] == ['#', '['] or (
            names[max(0, i - 3):i] == ['#', '!', '['])
        if attribute and name not in ATTRIBUTES and names[i + 1:i + 2] != ['::']:
            check([name], i)
        if attribute and name == 'derive':
            end = i + 2
            while end < len(names) and names[end] != ')':
                parts = [names[end]]
                start = end
                while names[end + 1:end + 2] == ['::']:
                    parts.append(names[end + 2])
                    end += 2
                if IDENT.fullmatch(parts[0]) and parts[0] not in DERIVES:
                    if len(parts) > 1 or parts[0] not in imported:
                        if not any(key.endswith('::' + parts[0]) for key in ALLOWLIST):
                            check(parts, start)
                end += 1
        if name == 'use':
            try:
                entries, end = import_paths(names, i + 1)
                for parts, index, alias in entries:
                    check(parts, index)
                i = end
                continue
            except (ValueError, IndexError):
                findings.append((code[i][1], 'unsupported journal use tree'))
        if name in {'extern', 'macro_use'}:
            findings.append((code[i][1], 'external/macro namespace injection is forbidden'))
        elif name == 'mod' and names[i + 1:i + 2] == ['tests']:
            # File-path test permissions depend on every entry into a tests
            # subtree having the exact built-in cfg(test), not cfg_attr.
            if not test_source(path) and names[max(0, i - 7):i] != ['#', '[', 'cfg', '(', 'test', ')', ']']:
                findings.append((code[i][1], 'test modules require an explicit cfg(test) entry'))
        elif name == 'mod' and names[i + 2:i + 3] == [';']:
            # The scanner must cover every out-of-line module's default location.
            child = module + (names[i + 1],)
            if child[:len(JOURNAL)] != JOURNAL and child[:len(OWNERSHIP)] != OWNERSHIP:
                findings.append((code[i][1], 'module declaration escapes the journal source scan'))
        elif ((name in {'crate', 'super', 'self', 'std', 'core', 'alloc'}
               or name[:1].islower() or name in imported or name in {'Path', 'PathBuf'})
              and names[i + 1:i + 2] == ['::']
              and names[max(0, i - 1):i] != ['.']):
            # Skip pub(in ...) visibility paths: they import no executable item.
            if names[max(0, i - 2):i] == ['(', 'in']:
                while i < len(names) and names[i] != ')':
                    i += 1
                continue
            else:
                parts = [name]
                end = i + 1
                while (names[end:end + 1] == ['::'] and end + 1 < len(names)
                       and IDENT.fullmatch(names[end + 1])):
                    parts.append(names[end + 1])
                    end += 2
                # Local method/type paths are not outside dependencies. Explicit
                # roots and lowercase dependency paths still require review.
                if name in imported or name != 'files':
                    check(parts, i)
                i = end
                continue
        if (names[i + 1:i + 2] == ['!']
                and names[i + 2:i + 3] in (['('], ['{'], ['['])
                and IDENT.fullmatch(name)):
            if name not in BUILTIN_MACROS and name not in imported and name not in local_macros and not any(
                    key.endswith('::' + name) for key in ALLOWLIST):
                check([name], i)
        i += 1
    return findings


def boundary_path(names: list[str], index: int) -> bool:
    parts = []
    while index >= 0 and IDENT.fullmatch(names[index]):
        parts.insert(0, names[index])
        if index < 2 or names[index - 1] != "::":
            break
        index -= 2
    return (parts == ["filesystem"]
            or parts == ["self", "filesystem"]
            or parts == ["crate", "web", "wire", "commands", "journal", "filesystem"]
            or (len(parts) > 1 and parts[-1] == "filesystem"
                and all(part == "super" for part in parts[:-1])))


def check_text(text: str, path: Path, crate_macros=None, macro_uses=()) -> list[tuple[int, str]]:
    if path == BOUNDARY:
        return []
    code = tokens(text)
    names = [name for name, _ in code]
    local = macro_definitions(code)
    if crate_macros is None:
        crate_macros = {name: {path} for name in local}
    local |= {name for name, sources in crate_macros.items()
              if all(source == COMMANDS / 'journal.rs'
                     or source.is_relative_to(COMMANDS / 'journal')
                     or (source, name) in CRATE_MACRO_ALLOWLIST for source in sources)}
    findings = [(text.count("\n", 0, offset) + 1, reason)
                for offset, reason in import_findings(code, path, local)]
    findings.extend(method_findings(text, code, path))
    attribute_depth = 0
    for i, (name, offset) in enumerate(code):
        if name == "[" and (attribute_depth or names[max(0, i - 1):i] == ["#"]
                            or names[max(0, i - 2):i] == ["#", "!"]):
            attribute_depth += 1
        elif name == "]" and attribute_depth:
            attribute_depth -= 1
        following = names[i + 1:i + 3]
        reason = None
        if following[:1] == ['!'] and names[i + 2:i + 3] in (['('], ['{'], ['[']):
            trusted_name = name in BUILTIN_MACROS or any(
                key.endswith('::' + name) for key in ALLOWLIST)
            for source in crate_macros.get(name, set()) | set(macro_uses):
                inside = source == COMMANDS / 'journal.rs' or source.is_relative_to(COMMANDS / 'journal')
                if (not inside or trusted_name or source in macro_uses) and (source, name) not in CRATE_MACRO_ALLOWLIST:
                    findings.append((text.count('\n', 0, offset) + 1,
                                     f'unreviewed crate macro provenance: {name} from {source}'))
        if name == "fs" and not (i >= 2 and names[i - 1] == "::"
                                 and boundary_path(names, i - 2)):
            reason = "filesystem paths/imports must use filesystem::fs"
        elif name == "OpenOptions" or (name == "File" and following[:1] == ["::"]):
            reason = "file constructors belong in filesystem.rs"
        elif name in {"platform", "libc"}:
            reason = "native I/O adapters belong in filesystem.rs"
        elif name == "filesystem" and i and names[i - 1] == "as":
            reason = "the filesystem boundary name cannot be rebound"
        elif name == "mod" and following[:1] == ["filesystem"]:
            if path != COMMANDS / "journal.rs" or following != ["filesystem", ";"]:
                reason = "only journal.rs may declare the filesystem.rs boundary"
        elif name == ">" and following[:1] == ["::"]:
            reason = "type-qualified paths would bypass journal path resolution"
        elif name == "include" and following[:1] == ["!"]:
            reason = "included Rust code would escape the journal source scan"
        elif attribute_depth and name == "path" and following[:1] == ["="]:
            reason = "module path overrides would escape the journal source scan"
        if reason:
            findings.append((text.count("\n", 0, offset) + 1, reason))
    return findings


def protected_files(root: Path) -> list[Path]:
    files = {root / COMMANDS / "journal.rs", root / COMMANDS / "ownership.rs"}
    for path in [*(root / COMMANDS).glob("journal*"), root / COMMANDS / "ownership"]:
        if path.is_symlink():
            files.add(path)
        elif path.is_dir():
            files.update(item for item in path.rglob("*") if item.is_symlink())
            files.update(path.rglob("*.rs"))
        elif path.suffix == ".rs":
            files.add(path)
    return sorted(files)


def macro_definitions(code):
    names = [name for name, _ in code]
    return {names[i + 2] for i in range(len(names) - 2)
            if names[i:i + 2] == ['macro_rules', '!']}


def crate_macro_sources(root):
    """Inventory definitions and exported/imported macro namespaces crate-wide.

    Rust macro scope is textual and can cross module boundaries without `use`.
    Any colliding crate definition is therefore checked, even under a cfg or
    in a sibling that the gate does not try to prove unreachable.
    """
    definitions, macro_uses = {}, set()
    for source in (root / 'src').rglob('*.rs'):
        text = source.read_text()
        if not any(marker in text for marker in ('macro_rules', 'macro_export', 'macro_use')):
            continue
        code = tokens(text)
        relative = source.relative_to(root)
        for name in macro_definitions(code):
            if not IDENT.fullmatch(name):
                # A macro generating another macro can choose its name at an
                # expansion site. Refuse this uninspectable namespace too.
                macro_uses.add(relative)
            else:
                definitions.setdefault(name, set()).add(relative)
        names = [name for name, _ in code]
        for i, name in enumerate(names):
            if name in {'macro_use', 'macro_export'} and (
                    names[max(0, i - 2):i] == ['#', '[']
                    or names[max(0, i - 3):i] == ['#', '!', '[']):
                # macro_export names are already recorded by macro_rules above.
                # macro_use can inject names from a dependency we cannot inspect.
                if name == 'macro_use':
                    macro_uses.add(relative)
    return definitions, macro_uses


def check_tree(root: Path) -> list[str]:
    findings = []
    crate_macros, macro_uses = crate_macro_sources(root)
    for path in protected_files(root):
        relative = path.relative_to(root)
        if path.is_symlink():
            findings.append(f"{relative}:1: symlinks escape the journal source scan")
            continue
        for line, reason in check_text(path.read_text(), relative, crate_macros, macro_uses):
            findings.append(f"{relative}:{line}: {reason}")
    return findings


def main() -> int:
    findings = check_tree(ROOT)
    for finding in findings:
        print(finding, file=sys.stderr)
    return int(bool(findings))


if __name__ == "__main__":
    raise SystemExit(main())
