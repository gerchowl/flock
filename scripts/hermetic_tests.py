#!/usr/bin/env python3
"""Gate: test code must not assert against ambient machine state.

A test that reads the process working directory, the machine's hostname, or a
hardcoded FHS path is asserting about the developer's laptop rather than about
flock. It passes for whoever wrote it and fails, confusingly, for everyone else
— long after the change that exposed it.

Three live instances motivated this gate (#268):

* a sidebar ordering test sorted on the lowercased basename of
  ``std::env::current_dir()`` and asserted the result fell between "aaa" and
  "zzz", so it failed in a worktree whose name began with a digit;
* a fleet-snapshot test used a fixture peer named after a real machine in the
  fleet, and the self-exclusion filter dropped it when run on that machine;
* an integration test hardcoded ``/bin/bash``, which does not exist on NixOS.

What is flagged, in test code only:

``std::env::current_dir``   the cwd is whatever directory you happen to be in
``gethostname``/``hostname``  the real machine name leaks into assertions
``/bin/<x>`` for x != sh    only ``/bin/sh`` is guaranteed by POSIX

Scope is deliberately narrow: ``tests/`` plus ``#[cfg(test)]`` regions of ``.rs``
sources. Production code legitimately reads all three.

Escape hatch, matching the other gates: put ``guardrails-ok`` on the line (a
reason after it is encouraged, e.g. ``guardrails-ok(hermetic): asserts the
fallback when cwd is unreadable``).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ESCAPE = "guardrails-ok"

# `/bin/sh` is the one POSIX-guaranteed interpreter path. Everything else in
# /bin is a distribution choice — NixOS provides only /bin/sh.
ALLOWED_BIN_PATHS = {"/bin/sh"}

PATTERNS: list[tuple[str, re.Pattern[str], str]] = [
    (
        "current_dir",
        re.compile(r"\benv::current_dir\s*\("),
        "reads the process working directory — set the value explicitly instead, "
        "so the assertion is about flock and not about where the repo is checked out",
    ),
    (
        "hostname",
        re.compile(r"\b(gethostname|hostname\s*\(\)|HOSTNAME)\b"),
        "reads the machine's real hostname — use a fixture name that cannot collide "
        "with a real host (RFC 2606 reserves .invalid for exactly this)",
    ),
    (
        "fhs-path",
        re.compile(r"""["'](/bin/[A-Za-z0-9._-]+)["']"""),
        "hardcodes an FHS path that not every distribution provides "
        "(NixOS ships only /bin/sh)",
    ),
]


def _test_line_ranges(text: str, path: Path) -> list[tuple[int, int]]:
    """Line ranges (1-based, inclusive) that are test code.

    Everything under ``tests/`` counts. Elsewhere only ``#[cfg(test)]`` regions
    do, tracked by brace depth from the ``mod`` that follows the attribute.
    """
    lines = text.splitlines()
    if "tests/" in path.as_posix():
        return [(1, len(lines))]

    ranges: list[tuple[int, int]] = []
    idx = 0
    while idx < len(lines):
        if "cfg(test)" not in lines[idx]:
            idx += 1
            continue
        # Walk to the module's opening brace, then to its matching close.
        depth = 0
        started = False
        start = idx + 1
        cursor = idx
        while cursor < len(lines):
            for ch in lines[cursor]:
                if ch == "{":
                    depth += 1
                    started = True
                elif ch == "}":
                    depth -= 1
            if started and depth <= 0:
                break
            cursor += 1
        ranges.append((start, cursor + 1))
        idx = cursor + 1
    return ranges


def _in_ranges(line_no: int, ranges: list[tuple[int, int]]) -> bool:
    return any(start <= line_no <= end for start, end in ranges)


def check_text(text: str, path: Path) -> list[tuple[int, str, str]]:
    """Return (line_no, pattern_name, message) for each violation."""
    ranges = _test_line_ranges(text, path)
    if not ranges:
        return []

    findings: list[tuple[int, str, str]] = []
    lines = text.splitlines()
    for line_no, line in enumerate(lines, start=1):
        # The marker counts on the line itself or on the one above it: Rust
        # lines are long, and a trailing comment often will not fit.
        previous = lines[line_no - 2] if line_no >= 2 else ""
        if ESCAPE in line or ESCAPE in previous:
            continue
        # A line that is entirely a comment is prose about the rule, not a use
        # of it — this gate's own doc comments would otherwise trip it.
        if line.lstrip().startswith("//"):
            continue
        if not _in_ranges(line_no, ranges):
            continue
        for name, pattern, message in PATTERNS:
            match = pattern.search(line)
            if not match:
                continue
            if name == "fhs-path" and match.group(1) in ALLOWED_BIN_PATHS:
                continue
            findings.append((line_no, name, message))
    return findings


def main(argv: list[str]) -> int:
    failed = False
    for name in argv:
        path = Path(name)
        if path.suffix != ".rs":
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for line_no, pattern_name, message in check_text(text, path):
            failed = True
            print(f"{path}:{line_no}: [{pattern_name}] {message}")

    if failed:
        print()
        print("Tests must not assert against ambient machine state (#268).")
        print(f"Deliberate exception: add `{ESCAPE}(hermetic): <reason>` to the line.")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
