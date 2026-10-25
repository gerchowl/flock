#!/usr/bin/env python3
"""Decide whether a pull request may skip the Rust build/test legs.

Reads changed paths (one per line) on stdin, prints `skip_rust=true|false`.
Fails SAFE: skip only for an ordinary pull_request whose every path is
allow-listed. Any other path, no paths, a landing batch (`land/` head), or any
other event runs everything. Rust tests read a few doc-like files, so those are
carved out of the allow-lists (see DENY below).
"""
import sys

# Pure docs / website / markdown, and the web client (covered by the `client` job).
ALLOW_PREFIXES = ("docs/", "website/", "web/client/")
# Read by Rust or script tests, or by release tooling: never skippable.
DENY_PREFIXES = ("docs/next/", "web/client/test/fixtures/")
DENY_FILES = {"website/latest.json", "CHANGELOG.md"}


def allowed(path: str) -> bool:
    if path in DENY_FILES or path.startswith(DENY_PREFIXES):
        return False
    if path.startswith(ALLOW_PREFIXES):
        return True
    return "/" not in path and path.endswith(".md")


def may_skip(event: str, head_ref: str, paths: list[str]) -> bool:
    if event != "pull_request" or head_ref.casefold().startswith("land/"):
        return False
    paths = [p.strip() for p in paths if p.strip()]
    return bool(paths) and all(allowed(p) for p in paths)


def main(argv: list[str]) -> int:
    event, head_ref = argv[1], argv[2]
    skip = may_skip(event, head_ref, sys.stdin.read().splitlines())
    print(f"skip_rust={'true' if skip else 'false'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
