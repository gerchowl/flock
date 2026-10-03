#!/usr/bin/env python3
"""Gate: every host a test names must be a declared fiction.

`scripts/ssh_hosts_gate.py` protects one person's real fleet, locally, from state
only they have — so it cannot run in CI and cannot help a contributor. This is
the half that binds: the repository *declares* the host labels its fixtures may
use, and a host-shaped string in test code that is not declared fails. A real
machine is undeclared by construction, so the rule needs no private data and
works for anyone who clones it.

That is the whole argument for an allow-list over a deny-list: a deny-list of
real names either has to be published (which re-leaks it) or kept private (which
means it is absent in CI, and every candidate is then unflagged).

Scope is test code — `tests/` plus `#[cfg(test)]` regions — and the value must
appear in a host-shaped position: an ssh destination, a reported host, an origin,
a routing endpoint, or an `agent_<host>_<suffix>` id. Prose in `docs/` is not a
fixture and is not checked here; the names that leak there are caught in review,
and the vocabulary stays the thing a contributor must opt into explicitly.

Escape hatch, matching the other gates: ``guardrails-ok(fixture): <reason>``.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ESCAPE = "guardrails-ok"
DECLARATION = Path(__file__).resolve().parent / "fixture-hosts.toml"

# Values under these keys are being used as a host. Deliberately narrow: `name`,
# `label`, `route` and `via` hold phrases and human labels, and including them
# turned every one-word string in the test tree into a "host".
HOST_KEYS = (
    "host",
    "hostname",
    "origin",
    "ssh",
    "ssh_target",
    "sshTarget",
    "proxy_jump",
    "ProxyJump",
    "from_host",
    "to_host",
    "origin_last_host",
)

_KEYED = re.compile(
    r"\b(?:" + "|".join(sorted(HOST_KEYS, key=len, reverse=True)) + r")\b\s*[:=]\s*\"([^\"]*)\""
)
_JSON_HOST = re.compile(
    r""""(?:host|hostname|origin|ssh|ssh_target|proxy_jump|from_host|to_host)"\s*:\s*"([^"]*)\""""
)
# `agent_<host>_<suffix>`: the host component is only judged when it is
# structured, for the same reason as `_labels_in` — `agent_status_id` is not a
# machine named "status".
_STRING_LITERAL = re.compile(r'"([^"\\]*)"')
_AGENT_ID = re.compile(r"\bagent_([a-z][a-z0-9-]*-[a-z0-9-]+|[a-z0-9-]*\.[a-z0-9-]+)_[A-Za-z0-9]+\b")

# A value that cannot be a host label: a placeholder, a path, or prose. Searched,
# not matched — `re.match` would only anchor the leading alternative, and
# "not a summary" would sail through as a hostname.
_NOT_A_HOST = re.compile(r"(^[<{])|(\s)|(\.\.)|(/)")


def declared_labels(path: Path = DECLARATION) -> set[str]:
    """The host labels this repository declares for its fixtures.

    Absent or unreadable is a hard error, not an empty allow-list: an empty
    allow-list would flag every fixture in the tree, and a silently missing
    declaration would flag nothing.
    """
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise SystemExit(f"error: missing {path}; fixtures have no declared hosts") from exc
    except tomllib.TOMLDecodeError as exc:
        raise SystemExit(f"error: cannot parse {path}: {exc}") from exc
    return {h.lower() for h in data.get("hosts", [])}


# RFC 2606 reserves these TLDs precisely so a name cannot resolve.
_RESERVED_SUFFIXES = (".invalid", ".test", ".example", ".localhost")


def _is_reserved(host: str) -> bool:
    """True for an RFC 2606 name, which cannot resolve by construction."""
    return host.lower().endswith(_RESERVED_SUFFIXES)


def _labels_in(value: str) -> list[str]:
    """Host labels a host-shaped value could refer to.

    Only *structured* names are returned — a dotted name, a hyphenated one, or a
    `user@host` destination. A bare lowercase word is not judged here: host fields
    legitimately hold `panel`, `status`, `session` and a dozen other single words,
    so checking bare words turns a precise rule into noise, and noise is what gets
    a gate `--no-verify`'d. Bare words are exactly what the private
    `scripts/ssh_hosts_gate.py` covers, because it knows the real labels and so
    has no false positives to suppress.
    """
    if not value or _NOT_A_HOST.search(value):
        return []
    host = value.rsplit("@", 1)[-1].split(":", 1)[0]
    out = []
    if "." in host or "-" in host or len(host) > 12:
        out.append(host)
    # `spoke9.invalid` and `spoke9` are one fiction, so the short form of a
    # reserved name is not a separate thing to declare.
    if "." in host and not _is_reserved(host):
        out.append(host.split(".", 1)[0])
    return out


def check_text(
    text: str, path: Path, allowed: set[str], ranges: list[tuple[int, int]]
) -> list[tuple[int, str, str]]:
    """Return (line_no, found_label, kind) for each undeclared host."""
    findings: list[tuple[int, str, str]] = []
    lines = text.splitlines()
    for line_no, line in enumerate(lines, start=1):
        previous = lines[line_no - 2] if line_no >= 2 else ""
        if ESCAPE in line or ESCAPE in previous:
            continue
        if line.lstrip().startswith("//"):
            continue
        if not any(start <= line_no <= end for start, end in ranges):
            continue
        seen: set[str] = set()
        for pattern, kind in ((_KEYED, "host-field"), (_JSON_HOST, "host-field")):
            for match in pattern.finditer(line):
                for label in _labels_in(match.group(1)):
                    seen.add(label)
        for literal in _STRING_LITERAL.findall(line):
            for match in _AGENT_ID.finditer(literal):
                seen.add(match.group(1))
        for label in seen:
            low = label.lower()
            # A reserved name needs no declaration, and neither does its short
            # form: `spoke9.invalid` and `spoke9` are the same fiction.
            if low in allowed or _is_reserved(low):
                continue
            findings.append((line_no, label, kind))
    return findings


try:  # reuse the single implementation of "what is test code"
    from scripts.hermetic_tests import _test_line_ranges
except ImportError:  # invoked as a script from the repo root
    from hermetic_tests import _test_line_ranges


def main(argv: list[str]) -> int:
    allowed = declared_labels()
    failed = False
    for name in argv:
        path = Path(name)
        if path.suffix != ".rs":
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        ranges = _test_line_ranges(text, path)
        if not ranges:
            continue
        for line_no, label, kind in check_text(text, path, allowed, ranges):
            failed = True
            print(
                f"{path}:{line_no}: [{kind}] {label!r} is not a declared fixture "
                f"host. Declare it in {DECLARATION.name}, or use an RFC 2606 "
                f"`.invalid` name."
            )

    if failed:
        print()
        print("Fixture hosts must be declared fictions (#510).")
        print(
            f"Add the label to `{DECLARATION.name}` under `[hosts]`, or put "
            f"`{ESCAPE}(fixture): <reason>` on the line."
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))