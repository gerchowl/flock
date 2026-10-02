#!/usr/bin/env python3
"""Report the tests this platform cannot run (#269).

`just check` on macOS used to run far fewer tests than CI runs on Linux, because
a slice of the suite was `#[cfg(not(target_os = "macos"))]`. Those tests are not
skipped on a Mac — they are compiled out. A green local run was therefore not
weak evidence about them; it was *no* evidence.

That gap is why a render-loop regression in #262 reached CI: `just check` passed
on macOS with 3104 tests, and ubuntu then failed
`api_ping::workspace_list_and_create_round_trip`, one of the compiled-out ones.
The headless server's event loop is exactly what those tests cover and exactly
what a Mac could not check.

The blanket gates are gone. What remains is one test that is Linux-only for a
reason that cannot be engineered away from a test (it reads `/proc/<pid>/cwd`).
This script is now a **ratchet**: it exists so that re-introducing a platform
gate is a visible decision rather than a silent loss of coverage, and so the
remaining gap keeps printing itself at the end of every `just check`.

It prints a notice rather than failing: the gap is a fact about the platform,
not a defect in the change being made. It runs at the end of `just check` so the
last thing you read is what your green run did NOT cover.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Both forms matter, and they differ in blast radius: `#![cfg(...)]` is an inner
# attribute that gates the WHOLE FILE, which is where most of the gap came from.
#
# Two spellings withhold a test from a macOS build, and both are counted. The
# original script matched only the first, which is how
# `pane_info_reports_foreground_cwd_without_changing_pane_cwd` stayed invisible
# in the count that was supposed to be the floor. `not(macos)` and `linux` are
# the same statement about a macOS build; treating them differently only let the
# gap be under-reported.
#
# `#[cfg(target_os = "macos")]` is deliberately absent: those tests exist HERE.
#
# The file-scope and item-scope patterns are built from ONE spelling list, because
# they used to disagree — `GATE_FILE` matched only `not(macos)` while the item
# pattern matched both. A file fully gated with `target_os = "linux"` was then
# reported as withholding a single gate instead of every test in it, which means
# the exact-count pin could be satisfied by a whole file being compiled out.
SPELLINGS = r'(?:not\(target_os\s*=\s*"macos"\)|target_os\s*=\s*"linux")'
GATE_FILE = re.compile(rf'#!\[cfg\({SPELLINGS}\)\]')
BLOCKING = re.compile(rf'#!?\[cfg\({SPELLINGS}\)\]')
TEST_ATTR = re.compile(r"#\[(?:tokio::)?test\]")

# Attributes are matched only at the start of a line, and comments are stripped
# first, so prose that NAMES a gate cannot be counted as one. That is not
# hypothetical: the file headers in tests/cli_wrapper.rs and tests/auto_detect.rs
# explain, in a comment, which gate they used to carry.
LINE_COMMENT = re.compile(r"//[^\n]*")
BLOCK_COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)


def _code_only(text: str) -> str:
    """Strip comments so gate counting reads code and not documentation."""
    return LINE_COMMENT.sub("", BLOCK_COMMENT.sub("", text))


def _fronts_a_test(code: str, after: int) -> bool:
    """Is the gate at `after` the one fronting a test function?

    A `#[cfg]` also lands on plain helpers, which cost no coverage on their own —
    a Linux-only `fn server_ptys` in a file whose tests all run everywhere
    withholds nothing. Counting those inflated the notice: with the real gap down
    to one test, "6 tests" where there is one is worse than useless, because it
    hides the number someone is meant to act on.
    """
    window = code[after : after + 400]
    head = window.split("fn ", 1)[0]
    return bool(TEST_ATTR.search(head))


def gated_tests(root: Path) -> dict[Path, int]:
    """Tests per file that do not exist in a macOS build.

    A file-level `#![cfg(...)]` withholds every test in the file; otherwise
    count the item-level gates that front a test.
    """
    counts: dict[Path, int] = {}
    for path in sorted(root.glob("tests/**/*.rs")):
        text = _code_only(path.read_text(encoding="utf-8"))
        rel = path.relative_to(root)
        if GATE_FILE.search(text):
            counts[rel] = len(TEST_ATTR.findall(text))
        else:
            item_gates = sum(
                1 for m in BLOCKING.finditer(text) if _fronts_a_test(text, m.end())
            )
            if item_gates:
                counts[rel] = item_gates
    return counts


def main(argv: list[str]) -> int:
    root = Path(__file__).resolve().parent.parent
    by_file = gated_tests(root)
    if not by_file:
        return 0

    if sys.platform != "darwin":
        # On Linux these all ran; nothing was withheld.
        return 0

    total = sum(by_file.values())

    print()
    print("  ⚠  platform coverage gap — this run did NOT verify everything")
    print()
    print(f"  {total} test(s) are gated off this platform and were COMPILED OUT of")
    print("  this build. They did not pass here; they do not exist here:")
    for path, count in sorted(by_file.items()):
        print(f"    {count:>3}  {path}")
    print()
    print("  Each gate should name, in a comment on the test itself, the reason it")
    print("  cannot run here. A gate nobody can explain is indistinguishable from")
    print("  an abandoned one (#269).")
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))