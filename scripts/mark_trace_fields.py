#!/usr/bin/env python3
"""Mark raw ?/% tracing-field debt with per-site guardrails-ok annotations.

The no-raw-trace-fields gate (guardrails) confines raw Debug/Display field
formatters to the schema surface (src/logging.rs). Every OTHER call site is
pre-existing debt, marked here so the gate can go hard immediately and police
new code, while the actual migration to the logging.rs facade happens in the
logging-redesign phase. docs/DEBT.md carries the auto-derived burn-down census.

The marker is a pure comment line placed directly ABOVE the flagged line (the
gate accepts that form) — trailing same-line markers are unstable: rustfmt
wraps over-long comments onto the following line, where they suppress nothing.

Runs the gate itself to find hits (no reimplemented detection = no drift) and
inserts markers bottom-up per file so line numbers stay valid. Idempotent: the
gate skips already-suppressed lines, so a re-run is a no-op.

Usage: python3 scripts/mark_trace_fields.py [gate-executable]
  gate-executable defaults to guardrails-no-raw-trace-fields (devShell PATH).
"""

import os
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

MARKER = "// guardrails-ok(no-raw-trace-fields): migrate to the logging.rs facade (logging redesign)"
HIT = re.compile(r"^  (?P<file>[^:]+):(?P<line>\d+):")


def main() -> int:
    gate = sys.argv[1] if len(sys.argv) > 1 else "guardrails-no-raw-trace-fields"
    root = Path(subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
    ).stdout.strip())
    env = dict(os.environ, GUARDRAILS_TRACE_ALLOW_GLOBS="*/logging.rs")
    files = subprocess.run(
        ["git", "ls-files", "src/*.rs", "src/**/*.rs"],
        check=True, capture_output=True, text=True, cwd=root,
    ).stdout.split()
    out = subprocess.run([gate, *files], capture_output=True, text=True, cwd=root, env=env)

    hits: dict[str, list[int]] = defaultdict(list)
    for raw in out.stdout.splitlines():
        m = HIT.match(raw)
        if m:
            hits[m.group("file")].append(int(m.group("line")))

    marked = 0
    for rel, linenos in sorted(hits.items()):
        path = root / rel
        lines = path.read_text().splitlines(keepends=True)
        # Bottom-up so earlier insertions don't shift pending line numbers.
        for no in sorted(linenos, reverse=True):
            indent = re.match(r"[ \t]*", lines[no - 1]).group(0)
            lines.insert(no - 1, f"{indent}{MARKER}\n")
            marked += 1
        path.write_text("".join(lines))
        print(f"marked {len(linenos):3d}  {rel}")

    print(f"total marked: {marked}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
