#!/usr/bin/env python3
"""Report the tests this platform cannot run (#269).

`just check` on macOS runs ~97 fewer tests than CI runs on Linux, because a
slice of the suite is `#[cfg(not(target_os = "macos"))]`. Those tests are not
skipped on a Mac — they are compiled out. A green local run is therefore not
weak evidence about them; it is *no* evidence.

That gap is why a render-loop regression in #262 reached CI: `just check` passed
on macOS with 3104 tests, and ubuntu then failed
`api_ping::workspace_list_and_create_round_trip`, one of the compiled-out ones.
The headless server's event loop is exactly what those tests cover and exactly
what a Mac cannot check.

This prints a notice rather than failing: the gap is a fact about the platform,
not a defect in the change being made. It runs at the end of `just check` so the
last thing you read is what your green run did NOT cover.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# Both forms matter, and they differ in blast radius: `#![cfg(...)]` is an inner
# attribute that gates the WHOLE FILE, which is where most of the gap comes from.
GATE_ITEM = re.compile(r'#\[cfg\(not\(target_os\s*=\s*"macos"\)\)\]')
GATE_FILE = re.compile(r'#!\[cfg\(not\(target_os\s*=\s*"macos"\)\)\]')
TEST_ATTR = re.compile(r"#\[(?:tokio::)?test\]")

# Areas whose behaviour is predominantly exercised by the gated tests. Named so
# the notice can say what is at stake, not just how many tests are missing.
AT_RISK = (
    "src/server/headless.rs (the render/stream loop)",
    "PTY sizing and pane geometry",
    "the socket API's pane read/write surface",
)


def gated_tests(root: Path) -> dict[Path, int]:
    """Tests per file that do not exist in a macOS build.

    A file-level `#![cfg(...)]` withholds every test in the file; otherwise
    count the item-level gates, each of which fronts one test.
    """
    counts: dict[Path, int] = {}
    for path in sorted(root.glob("tests/**/*.rs")):
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(root)
        if GATE_FILE.search(text):
            counts[rel] = len(TEST_ATTR.findall(text))
        else:
            item_gates = len(GATE_ITEM.findall(text))
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
    print(f"  {total} tests are gated #[cfg(not(target_os = \"macos\"))] and were")
    print("  COMPILED OUT of this build. They did not pass here; they do not exist here:")
    for path, count in sorted(by_file.items()):
        print(f"    {count:>3}  {path}")
    print()
    print("  Mostly covering:")
    for area in AT_RISK:
        print(f"    - {area}")
    print()
    print("  If you touched any of those, verify on Linux before landing — CI will")
    print("  otherwise be the first thing that runs them (#269).")
    print()
    print("  (Counted from the cfg attributes; the measured delta between the")
    print("   ubuntu and macos CI jobs on one commit was 97 tests, so treat this")
    print("   as a floor.)")
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
