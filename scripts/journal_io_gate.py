#!/usr/bin/env python3
"""Keep journal file I/O behind the counted filesystem boundary (#3203).

This is an architecture gate, not a Rust sandbox. The journal reserves `fs`,
`OpenOptions`, and file constructors for filesystem.rs. Imports and qualified
paths are checked alike, so aliases, grouped imports, raw identifiers and leading
`::` cannot opt out. Outside imports and qualified paths require a reviewed allowlist
entry, including macros. External globs, namespace injection, path overrides and
included Rust code and symlinks are refused. Internal globs only reach other scanned modules.
Comments and literals are ignored. There is no escape marker.
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
    'std::io': 'stream traits, buffered adapters and error values',
    'std::path::Path': 'journal path arguments',
    'std::path::PathBuf': 'owned journal paths',
    'std::sync': 'in-memory synchronization and channels',
    'std::task::Poll': 'test readiness assertions',
    'std::time': 'stall windows and test deadlines',
    'std::thread': 'journal workers and test coordination',
    'std::mem::swap': 'publish the compacted index',
    'std::future::Future': 'ownership asynchronous result type',
    'std::ops::Deref': 'test stream wrapper',
    'std::env::temp_dir': 'isolated test fixture location',
    'std::env::current_exe': 'journal subprocess test executable',
    'std::env::var': 'journal subprocess test selection',
    'std::process::exit': 'journal subprocess test completion',
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
    'tokio::test': 'ownership asynchronous tests',
    'tokio::runtime::Builder': 'ownership test runtime',
    'tokio::spawn': 'ownership recovery task',
    'tokio::pin': 'ownership wait futures',
    'tokio::select': 'ownership progress and stall arbitration',
    'parking_lot::Mutex': 'in-memory worker state and test hooks',
    'tracing::warn': 'journal preservation and lifecycle diagnostics',
    'futures_util::poll': 'test future readiness',
    'crate::app::notifications::now_ms': 'journal admission timestamps',
    'crate::logging::client_wire_unavailable': 'journal availability diagnostics',
    'crate::logging::client_journal_replay': 'journal replay progress diagnostics',
    'crate::logging::capture_logs': 'test replay diagnostics',
    'crate::structured::launch::SessionConfigDir': 'isolated test directory lifetime',
    'crate::process::TracedCommand': 'journal subprocess test launcher',
    'crate::cli::exit::SUCCESS': 'journal subprocess expected status',
    'crate::web::wire::commands::ack': 'wire acknowledgement construction',
    'crate::web::wire::commands::Command': 'journal command input',
    'crate::web::wire::commands::ErrorCode': 'typed journal refusals',
    'crate::web::wire::commands::ServerFrame': 'stored acknowledgement type',
    'crate::web::wire::CREDENTIAL_CONNECTIONS': 'journal capacity bound',
    'crate::web::wire::COMMANDS_PER_SOCKET': 'journal capacity bound',
    'crate::web::wire::CONNECTIONS': 'journal global capacity bound',
    'crate::web::wire::MAX_MESSAGE': 'journal record size bound',
    'crate::web::wire::IDEMPOTENCY_WINDOW_MS': 'journal retention window',
    'crate::web::wire::tests::runtime': 'test asynchronous runtime',
}
BUILTIN_MACROS = {
    'assert', 'assert_eq', 'assert_ne', 'debug_assert', 'debug_assert_eq',
    'debug_assert_ne', 'matches', 'format', 'vec', 'panic', 'unreachable',
    'eprintln', 'println', 'stringify', 'concat', 'env', 'option_env',
    'file', 'line', 'column', 'todo', 'unimplemented',
}
JOURNAL = ('crate', 'web', 'wire', 'commands', 'journal')
OWNERSHIP = JOURNAL[:-1] + ('ownership',)
# Built-in attributes and serde's reviewed derive helper introduce no new code.
ATTRIBUTES = {'cfg', 'derive', 'serde', 'test', 'allow', 'deny', 'warn', 'forbid'}
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


def import_findings(code, path):
    """Resolve explicit relative paths; refuse source/namespace escape constructs.

    This intentionally does not follow re-exports. Outside modules cannot be
    imported wholesale, so every new external dependency requires review here.
    Internal globs are safe because their source is scanned by this same gate.
    """
    names = [name for name, _ in code]
    findings = []
    base = module_path(path)
    modules = [base]
    imports = [{}]
    imported = imports[-1]
    i = 0

    def check(parts, index):
        original = parts[:]
        if parts and parts[0] in imported:
            parts = list(imported[parts[0]]) + parts[1:]
        if parts[-1:] == ['self']:
            parts = parts[:-1]
        if not parts:
            return
        if parts[0] == 'super':
            context = modules[-1]
            while parts and parts[0] == 'super':
                context, parts = context[:-1], parts[1:]
            parts = list(context) + parts
        elif parts[0] == 'self':
            parts = list(modules[-1]) + parts[1:]
        elif parts[0] in {'filesystem', 'tests', 'compact', 'journal'}:
            # Bare local modules are resolved relative to the current Rust module.
            parts = list(modules[-1]) + parts
        # The counted boundary re-exports std for tests. Its non-filesystem
        # members must receive the same review as a direct std path.
        if parts[:len(JOURNAL)] == list(JOURNAL) and 'std' in parts[len(JOURNAL):]:
            tail = parts[parts.index('std', len(JOURNAL)):]
            if len(tail) > 1:
                parts = tail
        resolved = tuple(parts)
        internal = resolved[:len(JOURNAL)] == JOURNAL or (
            base[:len(OWNERSHIP)] == OWNERSHIP
            and resolved[:len(OWNERSHIP)] == OWNERSHIP)
        allowed = any(resolved[:len(key.split('::'))] == tuple(key.split('::'))
                      for key in ALLOWLIST)
        if not internal and (not allowed or '*' in parts):
            findings.append((code[index][1],
                             'outside journal import/path is not allowlisted: '
                             + '::'.join(original)))
        return resolved

    while i < len(names):
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
                    resolved = check(parts, index)
                    if alias != "*":
                        imported[alias] = resolved
                i = end
                continue
            except (ValueError, IndexError):
                findings.append((code[i][1], 'unsupported journal use tree'))
        if name == '{':
            child = modules[-1]
            if i >= 2 and names[i - 2] == 'mod':
                child += (names[i - 1],)
            modules.append(child)
            imports.append(imported.copy())
            imported = imports[-1]
        elif name == '}' and len(modules) > 1:
            modules.pop()
            imports.pop()
            imported = imports[-1]
        elif name in {'extern', 'macro_rules', 'macro_use'}:
            findings.append((code[i][1], 'external/macro namespace injection is forbidden'))
        elif name == 'mod' and names[i + 2:i + 3] == [';']:
            # The scanner must cover every out-of-line module's default location.
            child = modules[-1] + (names[i + 1],)
            if child[:len(JOURNAL)] != JOURNAL and child[:len(OWNERSHIP)] != OWNERSHIP:
                findings.append((code[i][1], 'module declaration escapes the journal source scan'))
        elif ((name in {'crate', 'super', 'self', 'std', 'core', 'alloc'}
               or name[:1].islower())
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
                if name in imported or name not in {'files', 'io', 'mpsc', 'oneshot', 'watch'}:
                    check(parts, i)
                i = end
                continue
        if (names[i + 1:i + 2] == ['!']
                and names[i + 2:i + 3] in (['('], ['{'], ['['])
                and IDENT.fullmatch(name)):
            if name not in BUILTIN_MACROS and name not in imported and not any(
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


def check_text(text: str, path: Path) -> list[tuple[int, str]]:
    if path == BOUNDARY:
        return []
    code = tokens(text)
    names = [name for name, _ in code]
    findings = [(text.count("\n", 0, offset) + 1, reason)
                for offset, reason in import_findings(code, path)]
    attribute_depth = 0
    for i, (name, offset) in enumerate(code):
        if name == "[" and (attribute_depth or names[max(0, i - 1):i] == ["#"]
                            or names[max(0, i - 2):i] == ["#", "!"]):
            attribute_depth += 1
        elif name == "]" and attribute_depth:
            attribute_depth -= 1
        following = names[i + 1:i + 3]
        reason = None
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


def check_tree(root: Path) -> list[str]:
    findings = []
    for path in protected_files(root):
        relative = path.relative_to(root)
        if path.is_symlink():
            findings.append(f"{relative}:1: symlinks escape the journal source scan")
            continue
        for line, reason in check_text(path.read_text(), relative):
            findings.append(f"{relative}:{line}: {reason}")
    return findings


def main() -> int:
    findings = check_tree(ROOT)
    for finding in findings:
        print(finding, file=sys.stderr)
    return int(bool(findings))


if __name__ == "__main__":
    raise SystemExit(main())
