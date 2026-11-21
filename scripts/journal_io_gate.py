#!/usr/bin/env python3
"""Keep journal file I/O behind the counted filesystem boundary (#3203).

This is an architecture gate, not a Rust sandbox. The journal reserves `fs`,
`OpenOptions`, and file constructors for filesystem.rs. Imports and qualified
paths are checked alike, so aliases, grouped imports, raw identifiers and leading
`::` cannot opt out. Comments and literals are ignored. There is no escape marker.
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
    findings = []
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
    for path in (root / COMMANDS).glob("journal*"):
        if path.is_dir():
            files.update(path.rglob("*.rs"))
        elif path.suffix == ".rs":
            files.add(path)
    return sorted(files)


def check_tree(root: Path) -> list[str]:
    findings = []
    for path in protected_files(root):
        relative = path.relative_to(root)
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
