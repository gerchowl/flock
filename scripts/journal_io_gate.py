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
not infer receiver types or expand macros. Every capability identifier token,
including declarations, bare references, macro arguments and macro bodies, needs
a reviewed file/item/receiver anchor. Boundary and fixture code also need anchors.
Arguments and closure bodies remain independently scanned; receiver literals are
retained. Imports pin the complete use leaf, absolute root and binding, including
explicit `as` clauses. Type and function/value aliases require complete declaration
permissions, including initializer literals because the callable target carries
risk. Approved and discovered alias names join the token ban throughout
the tree, conservatively across scopes and cfgs. Function references, bindings,
calls and namespace uses have separate anchors. No implicit `as _` exemption is
granted. Declaration anchors are distinct from expression anchors. Duplicate
uses with the same anchor share its permission; another item cannot reuse it.
Receiver bindings remain a review responsibility, not proof of Rust type identity.
Every in-tree macro_rules! definition requires an explicit reasoned allowlist
entry reviewed for capability synthesis. No definitions currently need permission.
Expansion-time identifier construction from fragments (for example a paste-style
procedural macro) is not modeled: the outside import/path/macro allowlist refuses
such crates and their macros outside the counted filesystem boundary unless
explicitly reviewed. Binding discovery follows syntax and reviewed paths, not
arbitrary function effects. Generic, Self-dependent and type-qualified aliases
require explicit binding review. There is no shape-based metavariable or repetition
analysis. Alias collection covers the whole file and
its lexical/inline-module scopes before uses are checked. Unsupported source and
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


def tokens(text: str, *, literals=False) -> list[tuple[str, int]]:
    """Rust tokens and offsets, excluding comments and, by default, literals."""
    result = []
    i = 0
    while i < len(text):
        start, literal = i, False
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
            literal = True
            end = text.find('"' + match[1], match.end())
            i = len(text) if end < 0 else end + 1 + len(match[1])
        elif text[i] == '"':
            literal = True
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
            literal = True
            i = match.end()
        elif match := IDENT.match(text, i):
            result.append((match[0].removeprefix("r#"), i))
            i = match.end()
        else:
            token = "::" if text.startswith("::", i) else text[i]
            result.append((token, i))
            i += len(token)
        if literals and literal:
            result.append((text[start:i], start))
    return result


# Each entry is a reviewed item or an explicitly safe standard/library namespace.
# Descendants cover associated items (or the explicitly named safe namespace).
ALLOWLIST = {
    'std::collections::BTreeMap': 'recovered issuance consumption totals',
    'std::collections::BTreeSet': 'verified request scopes',
    'crate::api::gateway_grants::ClockPair': 'destination clock evidence value',
    'crate::structured::permissions::auth::gateway': 'A1 lease and pure grant evaluation; no journal file handles escape',
    'crate::structured::permissions::auth::tokens::Scope': 'verified request scope values',
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
    'std::panic::catch_unwind': 'injected crash after durable append',
    'std::panic::AssertUnwindSafe': 'isolated crash fixture is discarded after unwinding',
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
# Also required for every in-tree definition, reviewed for capability synthesis.
# The current journal tree defines no macros, so no entries are needed.
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


def delimiter_pairs(names):
    """Index balanced Rust token trees once for item and receiver boundaries."""
    pairs, stack = {}, []
    for i, name in enumerate(names):
        if name in {'(', '[', '{'}:
            stack.append(i)
        elif name in {')', ']', '}'} and stack:
            opening = stack.pop()
            if names[opening] == {')': '(', ']': '[', '}': '{'}[name]:
                pairs[opening], pairs[i] = i, opening
    return pairs


def item_paths(names, pairs):
    """Lexical module/impl/function identities, independent of call order.

    Closures and ordinary blocks inherit their enclosing item. Macro token trees
    get their own identity so generated items cannot borrow a real item's pins.
    Impl headers retain trait and generic tokens to distinguish implementations.
    """
    at, stack, path = [], [], ()
    i = 0
    while i < len(names):
        name, identity = names[i], None
        named = i + 1 + (names[i:i + 2] == ['static', 'mut'])
        anonymous_const = names[i:i + 2] == ['const', '{']
        if (name in {'impl', 'macro_rules'} or anonymous_const
                or (name in {'fn', 'mod', 'trait', 'struct', 'enum', 'union', 'const', 'static'}
                    and names[max(0, i - 1):i] != ["'"]
                    and named < len(names) and IDENT.fullmatch(names[named])
                    and names[named] != 'fn')):
            end = i + 1
            while end < len(names) and names[end] not in {'{', ';'}:
                if names[end] in {'(', '['}:
                    end = pairs.get(end, len(names))
                end += 1
            if name == 'impl':
                identity = ' '.join(names[i:end])
            elif name == 'macro_rules':
                identity = 'macro ' + ''.join(names[i + 2:end])
            else:
                identity = name + ' ' + ('block' if anonymous_const else names[named])
            at.extend([path + (identity,)] * (end - i))
            i = end
            if i >= len(names):
                break
            name = names[i]
        at.append(path)
        if name == '{':
            stack.append(path)
            if identity is not None:
                path += (identity,)
        elif name == '}' and stack:
            path = stack.pop()
        i += 1
    return at


def call_receiver(names, index, pairs):
    """Receiver token span for a method or associated capability identifier."""
    if names[max(0, index - 1):index] not in (['.'], ['::']):
        return None

    def receiver(end):
        if end < 0:
            return 0
        if names[end] == '?':
            return receiver(end - 1)
        start = pairs.get(end, end) if names[end] in {')', ']', '}'} else end
        if start and names[end] in {')', ']'} and (
                IDENT.fullmatch(names[start - 1]) or names[start - 1] in {')', ']'}):
            start = receiver(start - 1)
        if start and names[start - 1] in {'.', '::'}:
            if start >= 2 and (IDENT.fullmatch(names[start - 2])
                               or names[start - 2].isdecimal()
                               or names[start - 2] in {')', ']', '}', '?'}):
                start = receiver(start - 2)
            elif names[start - 1] == '::':
                start -= 1
        return start

    return receiver(index - 2), index - 1


# Binding permissions pin complete declarations; aliases need separate use anchors.
IMPORT_EXCEPTIONS = {
    (COMMANDS / 'journal/filesystem.rs', (), ('crate', 'platform', 'structured'), 'files', True): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('::', 'std', 'fs', 'remove_file'), 'remove_file', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('::', 'std', 'fs', 'rename'), 'rename', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'std', 'fs', 'remove_file'), 'remove_file', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'std', 'fs', 'rename'), 'rename', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'files'), 'files', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('super', 'super', 'copy'), 'copy', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('super', 'super', 'rename'), 'rename', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('super', 'super', 'write'), 'write', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal.rs', (), ('filesystem', 'files'), 'files', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal.rs', (), ('tests', 'Stream'), 'Stream', False): 'existing counted boundary/stream interface; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('crate', 'platform', 'file_lock', 'LockFile'), 'LockFile', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('::', 'std', 'fs', 'File'), 'File', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'std', 'fs', 'self'), 'fs', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'Observation'), 'Observation', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'std'), 'std', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'File'), 'File', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', (), ('counted', 'LockFile'), 'LockFile', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'collections', 'HashMap'), 'HashMap', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'io', 'self'), 'io', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'io', 'Read'), 'Read', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'io', 'Seek'), 'Seek', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'io', 'Write'), 'Write', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'path', 'Path'), 'Path', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'path', 'PathBuf'), 'PathBuf', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'sync', 'Arc'), 'Arc', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'sync', 'Mutex'), 'Mutex', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'sync', 'OnceLock'), 'OnceLock', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted',), ('::', 'std', 'sync', 'Weak'), 'Weak', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std'), ('::', 'std', '*'), '*', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('super', 'super', 'File'), 'File', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('super', 'super', 'OpenOptions'), 'OpenOptions', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod std', 'mod fs'), ('::', 'std', 'fs', '*'), '*', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod files'), ('super', '*'), '*', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod files'), ('crate', 'platform', 'structured', 'check_private_dir'), 'check_private_dir', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
    (COMMANDS / 'journal/filesystem.rs', ('mod counted', 'mod files'), ('crate', 'platform', 'structured', 'create_private_dir'), 'create_private_dir', False): 'existing counted boundary dependency/re-export; complete source leaf and binding reviewed',
}
ALIAS_EXCEPTIONS = {
    (COMMANDS / 'journal/tests.rs', ('impl std :: ops :: Deref for Stream',), 'type', ('type', 'Target', '=', 'File', ';'), 'Target'): 'existing stream type alias to the counted File; target and binding pinned together',
    (COMMANDS / 'journal.rs', (), 'type', ('type', 'Stream', '=', 'File', ';'), 'Stream'): 'existing stream type alias to the counted File; target and binding pinned together',
}
# Constructor/native namespaces also matter when hidden behind a new binding.
CAPABILITY_BINDINGS = CAPABILITY_METHODS | {
    'File', 'OpenOptions', 'LockFile', 'fs', 'filesystem', 'files', 'platform', 'libc',
}


def operation_tail(names, index):
    """Token after a capability name and any explicit generic arguments."""
    cursor = index + 1
    if names[cursor:cursor + 2] == ['::', '<']:
        cursor += 2
        depth = 1
        while cursor < len(names) and depth:
            depth += (names[cursor] == '<') - (
                names[cursor] == '>' and names[cursor - 1] != '-')
            cursor += 1
    return cursor


def capability_reference(names, index):
    tail = operation_tail(names, index)
    return (names[max(0, index - 1):index] != ['.']
            and names[tail:tail + 1] not in (['('], ['::'], ['.'], ['{'], [':']))


def binding_inventory(code):
    """Collect complete use leaves and value/type binding declarations."""
    names = [name for name, _ in code]
    pairs = delimiter_pairs(names)
    imports, bindings, imported = [], [], set()
    for i, name in enumerate(names):
        if i in imported:
            continue
        if name == 'use':
            try:
                entries, end = import_paths(names, i + 1, preserve_roots=True)
            except (ValueError, IndexError):
                imports.append((i, None, None, False))
                continue
            imported.update(range(i, end))
            for parts, index, binding in entries:
                renamed = names[index + 1:index + 2] == ['as']
                if not renamed and parts[-1] == 'self':
                    binding = parts[-2]
                imports.append((index, tuple(parts), binding, renamed))
        elif name in {'type', 'const', 'static', 'let'}:
            cursor = i + 1
            while cursor < len(names) and names[cursor] not in {'=', ';', '{', '}'}:
                cursor = pairs.get(cursor, cursor) + 1 if names[cursor] in {'(', '['} else cursor + 1
            if names[cursor:cursor + 1] != ['=']:
                continue
            end = cursor + 1
            while end < len(names) and names[end] not in {';', '}'}:
                end = pairs.get(end, end) + 1 if names[end] in {'(', '[', '{'} else end + 1
            if names[end:end + 1] != [';']:
                continue
            pattern = names[i + 1:cursor]
            if ':' in pattern:
                pattern = pattern[:pattern.index(':')]
            bound = ([part for j, part in enumerate(pattern) if IDENT.fullmatch(part)
                      and part not in {'mut', 'ref', '_'}
                      and pattern[j + 1:j + 2] not in (['('], ['::'], ['{'])] if name == 'let'
                     else [names[i + 1 + (names[i + 1:i + 2] == ['mut'])]])
            bindings.append((i, cursor + 1, end, name, tuple(bound)))
    return imports, bindings, imported


def capability_path(parts, aliases, *, unknown=True):
    parts = [part for part in parts if part not in {'::', 'self'}]
    if not parts:
        return False
    if parts[:2] == ['filesystem', 'std'] or (
            tuple(parts[:len(JOURNAL)]) == JOURNAL and 'std' in parts[len(JOURNAL):]):
        parts = parts[parts.index('std'):]
    if any(part in CAPABILITY_BINDINGS | aliases for part in parts):
        return True
    if parts[0] in {'io', 'mpsc', 'oneshot', 'watch'}:
        prefix = ['std', 'io'] if parts[0] == 'io' else (
            ['std', 'sync', 'mpsc'] if parts[0] == 'mpsc' else ['tokio', 'sync', parts[0]])
        parts = prefix + parts[1:]
    path = '::'.join(parts)
    if any(path == prefix or path.startswith(prefix + '::') for prefix in ALLOWLIST):
        return False
    return (path not in TYPE_ONLY_ALLOWLIST and len(parts) > 1
            and (unknown or parts[0] in {'std', 'core', 'alloc', 'crate'}))


def alias_context(sources, extra=()):
    """Conservatively propagate capability bindings across the protected tree.

    A global name set deliberately over-approximates Rust scopes and cfgs. New
    spellings never become invisible through a re-export or another alias.
    """
    contexts = {}
    aliases = (set(extra) | {key[-1] for key in ALIAS_EXCEPTIONS}
               | {key[-2] for key in IMPORT_EXCEPTIONS if key[-1] and key[-2] != '_'})
    for path, text in sources.items():
        code = tokens(text)
        contexts[path] = (code, binding_inventory(code))
    changed = True
    while changed:
        before = set(aliases)
        for source, (code, (imports, bindings, _)) in contexts.items():
            names = [name for name, _ in code]
            index = AliasIndex(names, module_path(source))

            def risky_path(parts, at, *, unknown=True):
                resolved = (list(parts[1:]) if parts[:1] == ('::',) else
                            index.resolve(list(parts), index.at[at]))
                return capability_path(resolved, aliases, unknown=unknown)
            for at, parts, binding, renamed in imports:
                if parts is None:
                    continue
                if renamed and risky_path(parts, at) and binding != '_':
                    aliases.add(binding)
            for opening, start, end, kind, bound in bindings:
                risky = False
                for at in range(start, end):
                    if not IDENT.fullmatch(names[at]):
                        continue
                    if names[at - 1] == '::' and at >= 2 and IDENT.fullmatch(names[at - 2]):
                        continue
                    cursor, parts = at + 1, [names[at]]
                    if names[at - 1] == '::':
                        parts.insert(0, '::')
                    while names[cursor:cursor + 1] == ['::'] and cursor + 1 < end and IDENT.fullmatch(names[cursor + 1]):
                        parts.append(names[cursor + 1])
                        cursor += 2
                    if kind == 'type' or capability_reference(names, cursor - 1):
                        risky |= risky_path(tuple(parts), at, unknown=kind == 'type')
                opaque_type = kind == 'type' and (
                    '<' in names[opening:start] or 'Self' in names[start:end]
                    or any(names[i:i + 2] == ['>', '::'] for i in range(start, end)))
                if risky or opaque_type or (kind in {'const', 'static', 'let'} and 'fn' in names[opening:start]):
                    aliases.update(set(bound) - CAPABILITY_METHODS)
        changed = aliases != before
    return aliases, contexts


def capability_anchor(text, code, names, pairs, items, index, *, declared=False):
    receiver = call_receiver(names, index, pairs)
    if receiver is not None:
        start, end = receiver
        receiver = ''.join(token for token, _ in tokens(
            text[code[start][1]:code[end][1]], literals=True))
        if capability_reference(names, index):
            receiver = '<reference> ' + receiver
    elif names[max(0, index - 1):index] in (['fn'], ['mod'], ['struct'], ['enum'], ['trait']):
        receiver = '<definition>'
    elif declared or names[index + 1:index + 2] == [':']:
        receiver = '<binding>'
    else:
        tail = operation_tail(names, index)
        receiver = ('<namespace>' if names[tail:tail + 1] == ['::'] else
                    '<call>' if names[tail:tail + 1] == ['('] else '<reference>')
    return items[index], receiver, names[index]


def binding_fingerprint(text, code, start, end):
    return tuple(token for token, _ in tokens(
        text[code[start][1]:code[end][1] + 1], literals=True))


def capability_findings(text, code, path, alias_names=()):
    aliases, contexts = alias_context({path: text}, alias_names)
    _, (imports, bindings, imported) = contexts[path]
    names = [name for name, _ in code]
    pairs = delimiter_pairs(names)
    items = item_paths(names, pairs)
    capabilities = CAPABILITY_METHODS | aliases
    findings, covered, declared = [], set(imported), set()
    for index, parts, binding, renamed in imports:
        if parts is None:
            findings.append((text.count('\n', 0, code[index][1]) + 1, 'unsupported capability use tree'))
        elif path == BOUNDARY or any(name in capabilities for name in (*parts, binding)):
            key = (path, items[index], parts, binding, renamed)
            if not IMPORT_EXCEPTIONS.get(key, '').strip():
                findings.append((text.count('\n', 0, code[index][1]) + 1,
                                 'capability import requires a complete leaf/binding exception'))
    for start, rhs, end, kind, bound in bindings:
        declared.update(i for i in range(start + 1, rhs - 1) if names[i] in bound)
        for binding in set(bound) & aliases:
            key = (path, items[start], kind, binding_fingerprint(text, code, start, end), binding)
            if ALIAS_EXCEPTIONS.get(key, '').strip():
                covered.update(i for i in range(start, end + 1)
                               if i < rhs or names[i] not in bound)
            else:
                findings.append((text.count('\n', 0, code[start][1]) + 1,
                                 'capability binding requires an exact declaration exception'))
    for i, (name, offset) in enumerate(code):
        if name in capabilities and i not in covered:
            item, receiver, method = capability_anchor(
                text, code, names, pairs, items, i, declared=i in declared)
            if (path, item, receiver, method) not in CALL_EXCEPTIONS:
                findings.append((text.count('\n', 0, offset) + 1,
                                 f'capability identifier requires an anchored exception: {name}'))
    return findings


# Group identical method sets within one file/item; expand exact independent anchors.
# Binding, reference, call and namespace roles have separate permissions.
CALL_EXCEPTIONS = {
    (COMMANDS / path, item, receiver, method): reason
    for path, item, receivers, methods, reason in [
        ('journal/tests/grants.rs', ('fn final_lifecycle_use_is_not_admitted_twice',), ('Writer',), 'spawn', 'existing journal worker with a bounded sync barrier, never a child process'),
        ('journal/tests/grants.rs', ('fn crash_before_append_spends_nothing_on_restart',), ('Journal',), 'open', 'isolated A1 fixtures and counted journal replay for transactional recovery assertions'),
        ('journal/tests/grants.rs', ('fn claim_holds_generation_lease_through_sync',), ('Journal', 'Store'), 'open', 'isolated A1 fixtures and counted journal replay for transactional recovery assertions'),
        ('journal/tests/grants.rs', ('fn count_ordinals_and_standing_claims_survive_outcomes_and_restart',), ('Journal',), 'open', 'isolated A1 fixtures and counted journal replay for transactional recovery assertions'),
        ('journal/tests/grants.rs', ('fn in_flight_compaction_cannot_publish_over_consumption',), ('Journal',), 'open', 'isolated A1 fixtures and counted journal replay for transactional recovery assertions'),
        ('journal/tests/grants.rs', ('fn request',), ('Store',), 'open', 'open the isolated A1 grant namespace, never a journal handle'),
        ('journal/tests/grants.rs', ('fn final_lifecycle_use_is_not_admitted_twice',), ('Journal',), 'open', 'replay isolated claims through the existing counted journal opener'),
        ('journal/tests/grants.rs', ('fn final_lifecycle_use_is_not_admitted_twice',), ('std::thread',), 'spawn', 'one joined request producer with a bounded start wait'),
        ('journal/tests/grants.rs', ('fn retry_is_free_but_payload_target_and_destination_are_bound',), ('Journal',), 'open', 'replay isolated claims through the existing counted journal opener'),
        ('journal/tests/grants.rs', ('fn failed_append_and_precommit_drop_spend_nothing',), ('Journal',), 'open', 'replay isolated claims through the existing counted journal opener'),
        ('journal/tests/grants.rs', ('fn crash_after_commit_before_publication_recovers_spent_pending_claim',), ('Journal',), 'open', 'replay isolated claims through the existing counted journal opener'),
        ('journal/tests/grants.rs', ('fn revoke_first_fences_claim_and_retry_without_spending',), ('Journal',), 'open', 'replay isolated claims through the existing counted journal opener'),
        ('journal/tests/grants.rs', ('fn compaction_retains_grant_totals_and_outcomes_beyond_replay_window',), ('Journal',), 'open', 'isolated replay through the counted journal boundary for retention and crash recovery'),
        ('journal/tests/grants.rs', ('fn retained_grants_refuse_capacity_without_eviction_or_restored_uses',), ('Journal',), 'open', 'isolated replay through the counted journal boundary for retention and crash recovery'),
        ('journal/tests/grants.rs', ('fn crash_at_claim_dispatch_and_result_never_redispatches_after_recovery',), ('Journal',), 'open', 'isolated replay through the counted journal boundary for retention and crash recovery'),
        ('journal/tests/grants.rs', ('fn grant_crash_child',), ('Journal',), 'open', 'isolated child replays through the counted journal boundary'),
        ('journal/tests/grants.rs', ('fn grant_crash_child',), ('effect',), 'write_all sync_all', 'a counted private fixture stream models one durable external effect'),
        ('journal/tests/grants.rs', ('fn crash_at_claim_dispatch_and_result_never_redispatches_after_recovery',), ('<binding>', '<reference>'), 'status', 'bounded child exit observation rejects unexpected or silent producers'),
        ('journal/tests/grants.rs', ('fn crash_at_claim_dispatch_and_result_never_redispatches_after_recovery',), ('child',), 'try_wait kill wait', 'observe and reap only the isolated crash child started by this fixture'),
        ('journal/tests/grants.rs', ('fn crash_at_claim_dispatch_and_result_never_redispatches_after_recovery',), ('super::super::filesystem::fs',), 'read', 'read the isolated durable effect through the counted filesystem boundary'),
        ('journal/tests/grants.rs', ('fn missing_authoritative_log_refuses_restart_without_recreating_uses',), ('Journal',), 'open', 'isolated replay refuses a lost authoritative log'),
        ('journal/tests/grants.rs', ('fn missing_authoritative_log_refuses_restart_without_recreating_uses',), ('super::super::filesystem::fs',), 'remove_file read', 'isolated log-loss fixture through the counted filesystem boundary'),
        ('journal/compact.rs', ('impl Read for Positioned', 'fn read'), ('<definition>',), 'read', 'existing journal method declaration, not an expression permission'),
        ('journal/compact.rs', ('impl Read for Positioned', 'fn read'), ('<namespace>',), 'files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/compact.rs', ('impl Snapshot', 'fn copy_to'), ('self.file',), 'write_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal/compact.rs', ('impl Journal', 'fn start_compaction'), ('<namespace>',), 'files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/compact.rs', ('impl Journal', 'fn start_compaction'), ('self.file',), 'try_clone', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal/compact.rs', ('impl Journal', 'fn start_compaction'), ('std::thread::Builder::new().name("wire-compact".into())',), 'spawn', 'existing journal worker thread, not a child process; body scanned independently'),
        ('journal/compact.rs', ('impl Journal', 'fn start_compaction'), ('filesystem',), 'remove_file', 'existing counted filesystem boundary adapter'),
        ('journal/compact.rs', ('impl Journal', 'fn compacted'), ('<namespace>',), 'Stream files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/compact.rs', ('impl Journal', 'fn compacted'), ('snapshot.file',), 'sync_all try_clone', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal/compact.rs', ('impl Journal', 'fn compacted'), ('filesystem',), 'rename', 'existing counted filesystem boundary adapter'),
        ('journal/filesystem.rs', ('fn canonical_path',), ('path',), 'canonicalize', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('fn sync_directory',), ('File',), 'open', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('fn sync_directory',), ('File::open(path)?',), 'sync_all', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Observation', 'fn start'), ('registry()',), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Observation', 'fn sample'), ('self.0.counts',), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn observe'), ('registry()', 'observer.counts'), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn wrap'), ('registry()',), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn open'), ('<definition>', '::std::fs::File'), 'open', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn create'), ('<definition>', '::std::fs::File'), 'create', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn try_clone'), ('<definition>', 'self.raw'), 'try_clone', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn metadata'), ('<definition>', 'self.raw'), 'metadata', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn set_len'), ('<definition>', 'self.raw'), 'set_len', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn sync_all'), ('<definition>', 'self.raw'), 'sync_all', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl File', 'fn sync_data'), ('<definition>', 'self.raw'), 'sync_data', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Read for File', 'fn read'), ('<definition>', 'self.raw'), 'read', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Seek for File', 'fn seek'), ('<definition>', 'self.raw'), 'seek', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Write for File', 'fn write'), ('<definition>', 'self.raw'), 'write', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Write for File', 'fn write'), ('self.path',), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl Write for File', 'fn flush'), ('<definition>', 'self.raw'), 'flush', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl OpenOptions', 'fn write'), ('<definition>', 'self.0'), 'write', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl OpenOptions', 'fn read'), ('<definition>', 'self.0'), 'read', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl OpenOptions', 'fn create_new'), ('<definition>', 'self.0'), 'create_new', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl OpenOptions', 'fn create'), ('<definition>', 'self.0'), 'create', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'impl OpenOptions', 'fn open'), ('<definition>', 'self.0'), 'open', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn write'), ('<definition>',), 'write', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn write'), ('File',), 'create', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn write'), ('File::create(path)?',), 'write_all', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn copy'), ('<definition>', 'io'), 'copy', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn copy'), ('File',), 'create open', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn copy'), ('<binding>', 'source', '<reference>'), 'metadata', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn copy'), ('metadata',), 'is_file', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn copy'), ('destination.raw',), 'set_permissions', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn rename'), ('<definition>', '::std::fs'), 'rename', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn rename'), ('registry()', 'path'), 'lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'mod files'), ('<definition>',), 'files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/filesystem.rs', ('mod counted', 'impl LockFile', 'fn try_lock'), ('<definition>', 'self.0'), 'try_lock', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('crate::structured::launch::SessionConfigDir', 'std::fs::File', 'std::fs::OpenOptions::new().read(true).write(true).append(true)'), 'create', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('original', 'extra', 'replacement'), 'write_all', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('std::fs',), 'copy write', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('std::fs::OpenOptions::new()',), 'read', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('std::fs::OpenOptions::new().read(true)',), 'write', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('std::fs::OpenOptions::new().read(true).write(true).append(true).create(true).truncate(false)',), 'create_new', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('std::fs::OpenOptions::new().read(true).write(true).append(true).create(true).truncate(false).create_new(false)', 'OpenOptions::new().append(true)'), 'open', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('OpenOptions::new().append(true).open(&compact).unwrap()',), 'try_clone', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/filesystem.rs', ('mod counted', 'fn counts_new_handles_clones_copies_and_replacement'), ('<call>',), 'rename', 'reviewed counted filesystem boundary and its adapter fixtures'),
        ('journal/tests/capacity.rs', ('fn near_global_cap_hot_operations_write_only_one_record',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/capacity.rs', ('fn near_global_cap_hot_operations_write_only_one_record',), ('journal.file',), 'try_clone', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/capacity.rs', ('fn near_global_cap_hot_operations_write_only_one_record',), ('file',), 'flush write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn expiry_threshold_drops_only_expired_records_and_releases_exact_capacity',), ('filesystem::fs',), 'metadata', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn expiry_threshold_drops_only_expired_records_and_releases_exact_capacity',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn pause_prefix',), ('PAUSE.get().unwrap()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn concurrent_appends_and_multiple_catchup_rounds_keep_exact_index_and_first_outcomes',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn concurrent_appends_and_multiple_catchup_rounds_keep_exact_index_and_first_outcomes',), ('PAUSE.get_or_init(||Mutex::new(None))',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn concurrent_appends_and_multiple_catchup_rounds_keep_exact_index_and_first_outcomes',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn opaque_rows_fence_compaction_and_remain_byte_exact',), ('journal.file',), 'sync_all write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn opaque_rows_fence_compaction_and_remain_byte_exact',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn opaque_rows_fence_compaction_and_remain_byte_exact',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn failed_append_during_snapshot_cannot_publish_over_uncertain_tail',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn shutdown_drains_cancelled_jobs_closes_every_clone_and_releases_lease_before_reopen',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn shutdown_drains_cancelled_jobs_closes_every_clone_and_releases_lease_before_reopen',), ('Writer',), 'open spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn shutdown_drains_cancelled_jobs_closes_every_clone_and_releases_lease_before_reopen',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn compaction_crash_child',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn crash_at_each_compaction_step_leaves_a_complete_replayable_old_or_new_log',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn crash_at_each_compaction_step_leaves_a_complete_replayable_old_or_new_log',), ('<binding>', '<reference>'), 'status', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn crash_at_each_compaction_step_leaves_a_complete_replayable_old_or_new_log',), ('child',), 'kill try_wait wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn crash_at_each_compaction_step_leaves_a_complete_replayable_old_or_new_log',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn published',), ('PUBLISHED.get().unwrap()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_claim_clock_drives_threshold_compaction_and_shutdown_drains_it',), ('PUBLISHED.get_or_init(||Mutex::new(None))',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_claim_clock_drives_threshold_compaction_and_shutdown_drains_it',), ('Writer',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_claim_clock_drives_threshold_compaction_and_shutdown_drains_it',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn replacement_descriptor_supports_later_sweeps_appends_and_future_timestamps',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn lost_primary_during_copy_is_fenced_without_resurrecting_the_journal',), ('filesystem::fs',), 'read rename', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn lost_primary_during_copy_is_fenced_without_resurrecting_the_journal',), ('primary',), 'exists', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn opaque_row_discovered_by_copier_preserves_the_log_and_fences_writes',), ('journal.file',), 'sync_all write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn opaque_row_discovered_by_copier_preserves_the_log_and_fences_writes',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn expiry_reclaims_full_capacity_without_log_growth_after_sweep_or_reopen',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_capacity_refusal_triggers_expiry_sweep_and_retry_succeeds',), ('PUBLISHED.get_or_init(||Mutex::new(None))',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_capacity_refusal_triggers_expiry_sweep_and_retry_succeeds',), ('Writer',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests/compaction.rs', ('fn writer_capacity_refusal_triggers_expiry_sweep_and_retry_succeeds',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn root',), ('crate::structured::launch::SessionConfigDir',), 'create', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn setup',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn replay_rebuilds_index_counts_and_first_outcome',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn replay_rebuilds_index_counts_and_first_outcome',), ('filesystem::fs',), 'read_to_string', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn claim_and_finish_write_one_record_independent_of_retained_size',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn claim_and_finish_write_one_record_independent_of_retained_size',), ('journal.file',), 'sync_all write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn claim_and_finish_write_one_record_independent_of_retained_size',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn principal_global_and_byte_caps_refuse_without_eviction',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn absent_expired_and_lost_claims_fail_closed',), ('filesystem::fs',), 'remove_file', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn absent_expired_and_lost_claims_fail_closed',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn torn_tail_is_archived_before_truncation_and_append_remains_readable',), ('journal.file',), 'sync_all write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn torn_tail_is_archived_before_truncation_and_append_remains_readable',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn torn_tail_is_archived_before_truncation_and_append_remains_readable',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn complete_corruption_and_oversized_rows_preserve_raw_and_other_results',), ('journal.file',), 'write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn complete_corruption_and_oversized_rows_preserve_raw_and_other_results',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn complete_corruption_and_oversized_rows_preserve_raw_and_other_results',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn archive_full_preserves_the_active_tail',), ('journal.file',), 'write_all', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn archive_full_preserves_the_active_tail',), ('archive',), 'set_len', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn archive_full_preserves_the_active_tail',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn archive_full_preserves_the_active_tail',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn append_and_sync_failures_roll_back_before_publishing_index',), ('filesystem::fs',), 'read', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn append_and_sync_failures_roll_back_before_publishing_index',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn failed_outcome_keeps_pending_and_failed_rollback_requires_recovery',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn exclusive_writer_lease_prevents_stale_parallel_indexes',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn submit',), ('std::thread',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn worker_never_blocks_submit_on_sync_and_queue_remains_bounded',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn worker_never_blocks_submit_on_sync_and_queue_remains_bounded',), ('Writer', 'tokio'), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn worker_never_blocks_submit_on_sync_and_queue_remains_bounded',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn crash_after_synced_outcome_before_index_publication_replays_the_ack',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn public_writer_opens_and_wakes_only_after_durable_completion',), ('Writer', 'Journal'), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn public_writer_opens_and_wakes_only_after_durable_completion',), ('filesystem::fs',), 'read_to_string', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn public_writer_opens_and_wakes_only_after_durable_completion',), ('std::thread',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('struct Stream',), ('<definition>',), 'Stream', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/tests.rs', ('impl From < File > for Stream',), ('<reference>',), 'Stream', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/tests.rs', ('impl std :: ops :: Deref for Stream',), ('<reference>',), 'Stream', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/tests.rs', ('impl Write for Stream',), ('<reference>',), 'Stream', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal/tests.rs', ('impl Write for Stream', 'fn write'), ('<definition>', 'self.file'), 'write', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Write for Stream', 'fn flush'), ('<definition>', 'self.file'), 'flush', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_open'), ('Self',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_open'), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn replay_reports_byte_progress_without_record_contents',), ('Journal',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn take_replay_steps',), ('replay_steps()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('fn replay_barrier',), ('replay_hooks()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_step_replay'), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_step_replay'), ('replay_steps()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_stall_replay'), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal/tests.rs', ('impl Writer', 'fn test_stall_replay'), ('replay_hooks()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('journal.rs', ('impl < R : Read > Read for ReplayReader < R >', 'fn read'), ('<definition>',), 'read', 'existing journal method declaration, not an expression permission'),
        ('journal.rs', ('impl < R : Read > Read for ReplayReader < R >', 'fn read'), ('self.source',), 'read', 'existing replay stream delegates to the counted File'),
        ('journal.rs', ('impl Writer', 'fn open'), ('<definition>',), 'open', 'existing journal method declaration, not an expression permission'),
        ('journal.rs', ('impl Writer', 'fn open_signalled'), ('Self',), 'spawn', 'existing journal Writer factory; closure uses the counted boundary'),
        ('journal.rs', ('impl Writer', 'fn spawn'), ('<definition>',), 'spawn', 'existing journal method declaration, not an expression permission'),
        ('journal.rs', ('impl Writer', 'fn spawn'), ('<binding>', '<call>'), 'open', 'existing Writer factory closure binding and invocation; body uses counted adapters'),
        ('journal.rs', ('impl Writer', 'fn spawn'), ('std::thread::Builder::new().name("wire-journal".into())',), 'spawn', 'existing journal worker thread, not a child process; body scanned independently'),
        ('journal.rs', ('impl Writer', 'fn spawn'), ('admission',), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('journal.rs', ('impl Writer', 'fn submit'), ('self.admission',), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('journal.rs', ('impl Writer', 'fn shutdown'), ('self.admission',), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('journal.rs', ('fn private_created',), ('<namespace>',), 'files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal.rs', ('fn private_created',), ('file',), 'sync_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('struct Journal',), ('<reference>',), 'Stream', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal.rs', ('impl Journal', 'fn open'), ('<definition>',), 'open', 'existing journal method declaration, not an expression permission'),
        ('journal.rs', ('impl Journal', 'fn lease'), ('<namespace>',), 'Stream files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('journal.rs', ('impl Journal', 'fn lease'), ('lease',), 'try_lock', 'existing counted LockFile wrapper'),
        ('journal.rs', ('impl Journal', 'fn lease'), ('file',), 'metadata', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn seed_capacity_test'), ('self.file',), 'metadata try_clone', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn seed_capacity_test'), ('file',), 'flush write_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn seed_capacity_test'), ('file.get_ref()',), 'sync_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn seed_capacity_test'), ('self.file.try_clone()?',), 'seek', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn replay'), ('self.file',), 'set_len sync_all try_clone', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn replay'), ('file',), 'metadata sync_all write_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn append'), ('self.file',), 'set_len sync_all sync_data write_all', 'existing counted File/Stream or its buffered writer; boundary retains I/O observation'),
        ('journal.rs', ('impl Journal', 'fn apply'), ('<namespace>',), 'files', 'existing use of reviewed counted stream/namespace alias; this use site reviewed separately'),
        ('ownership/tests.rs', ('fn fixture',), ('crate::structured::launch::SessionConfigDir',), 'create', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn install',), ('owner.0.pending', 'owner.0.writer'), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn apply',), ('owner.0.writer',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn commit_prepares_without_lease_then_replays_after_exporter_freeze',), ('importer.0.writer',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn commit_prepares_without_lease_then_replays_after_exporter_freeze',), ('Writer',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn rollback_replays_before_opening_and_shutdown_cannot_be_undone_by_recovery',), ('owner.0.writer',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_drains_accepted_write_and_releases_lease_with_handles_alive',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_drains_accepted_write_and_releases_lease_with_handles_alive',), ('tokio',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_waits_for_inflight_replay_and_fences_its_late_completion',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_waits_for_inflight_replay_and_fences_its_late_completion',), ('owner.0.open_barrier',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_during_leased_replay_is_bounded_and_fences_late_completion',), ('tokio',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn shutdown_during_leased_replay_is_bounded_and_fences_late_completion',), ('Writer',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn replay_observed',), ('replay_events()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn take_open_barrier',), ('open_hooks()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('impl Ownership', 'fn test_watch_replay'), ('replay_events()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('impl Ownership', 'fn test_stall_next_open'), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('impl Ownership', 'fn test_stall_next_open'), ('open_hooks()',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn stalled_drain',), ('<binding>', '<reference>'), 'wait', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn stalled_drain',), ('tokio',), 'spawn', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn stalled_drain',), ('owner.0.pending',), 'lock', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership/tests.rs', ('fn stalled_drain',), ('Writer',), 'open', 'reviewed isolated fixtures, counted streams, synchronization and subprocess assertions'),
        ('ownership.rs', ('impl Ownership', 'fn submit'), ('self.0.writer',), 'try_lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('ownership.rs', ('impl Ownership', 'fn abandoned'), ('self.0.pending',), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('ownership.rs', ('impl Ownership', 'fn recover'), ('tokio',), 'spawn', 'existing ownership future task, not a child process; body scanned independently'),
        ('ownership.rs', ('impl Ownership', 'fn acquire_at'), ('self.0.writer', 'self.0.open_barrier', 'self.0.pending'), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('ownership.rs', ('impl Ownership', 'fn freeze'), ('tokio',), 'spawn', 'existing ownership future task, not a child process; body scanned independently'),
        ('ownership.rs', ('impl Ownership', 'fn freeze'), ('owner.0.writer',), 'lock', 'existing in-memory ownership/admission mutex, not a filesystem lock'),
        ('ownership.rs', ('impl Ownership', 'fn reopen'), ('tokio',), 'spawn', 'existing ownership future task, not a child process; body scanned independently'),
    ]
    for receiver in receivers
    for method in methods.split()
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


def import_paths(names: list[str], start: int, *, preserve_roots=False):
    """Expand use trees, retaining source indices for diagnostics and aliases."""
    entries = []

    def tree(i, prefix):
        parts = list(prefix)
        if names[i:i + 1] == ['::']:
            i += 1
            if preserve_roots:
                parts = ['::']
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
    aliases = AliasIndex(names, base)
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


def check_text(text: str, path: Path, crate_macros=None, macro_uses=(), alias_names=()) -> list[tuple[int, str]]:
    code = tokens(text)
    names = [name for name, _ in code]
    findings = capability_findings(text, code, path, alias_names)
    for i, (name, offset) in enumerate(code[:-2]):
        if names[i:i + 2] == ['macro_rules', '!'] and not CRATE_MACRO_ALLOWLIST.get((path, names[i + 2]), '').strip():
            findings.append((text.count('\n', 0, offset) + 1,
                             'in-tree macro definition requires capability-synthesis review'))
    if path == BOUNDARY:
        return findings
    local = macro_definitions(code)
    if crate_macros is None:
        crate_macros = {name: {path} for name in local}
    local |= {name for name, sources in crate_macros.items()
              if all(source == COMMANDS / 'journal.rs'
                     or source.is_relative_to(COMMANDS / 'journal')
                     or (source, name) in CRATE_MACRO_ALLOWLIST for source in sources)}
    findings.extend((text.count("\n", 0, offset) + 1, reason)
                    for offset, reason in import_findings(code, path, local))
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
    paths = protected_files(root)
    sources = {path.relative_to(root): path.read_text() for path in paths if not path.is_symlink()}
    aliases, _ = alias_context(sources)
    for path in paths:
        relative = path.relative_to(root)
        if path.is_symlink():
            findings.append(f"{relative}:1: symlinks escape the journal source scan")
            continue
        for line, reason in check_text(sources[relative], relative, crate_macros, macro_uses, aliases):
            findings.append(f"{relative}:{line}: {reason}")
    return findings


def main() -> int:
    findings = check_tree(ROOT)
    for finding in findings:
        print(finding, file=sys.stderr)
    return int(bool(findings))


if __name__ == "__main__":
    raise SystemExit(main())
