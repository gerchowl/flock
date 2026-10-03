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
# Rust writes `peer.host = Some("atlas".into())`; the JSON-shaped rule never sees it.
_RUST_HOST = re.compile(
    r"""\b(?:host|hostname|origin|from_host|to_host|peer)\s*[:=]\s*Some\(\s*"([^"]+)\""""
)
_STRING_LITERAL = re.compile(r'"([^"\\]*)"')
# Prose has no `host = "..."` key to key on, so it gets the three shapes that are
# unambiguous in a sentence. A bare or hyphenated word in prose is deliberately
# NOT judged: every commit message is full of identifiers (`sidecar`, `nextest`,
# `no-such-host`), and judging those is how a gate teaches --no-verify.
# A two-character host is an initial or a placeholder, not a machine: a trailer
# like `Co-authored-by: T <t@t>` is a person's attribution, not a fleet member.
# The negative lookbehind keeps a git ref out of it — `rust-toolchain@stable` is a
# GitHub Action ref, not a login.
_PROSE_USER_AT_HOST = re.compile(
    r"(?<![/\w.-])[A-Za-z0-9._-]+@([A-Za-z0-9][A-Za-z0-9.-]{3,})\b"
)
# Shapes that appear in documentation ABOUT this syntax rather than as a machine.
# This is a vocabulary of placeholders, not a list of anything private.
_PLACEHOLDER_HOSTS = {"host", "hostname", "machine", "example", "your-host", "yourhost"}
# Attribution trailers carry a person's email identity, not a machine on the
# author's fleet: `Co-authored-by: someone <someone@their-corp.example>` is a
# credit line. Judged, it flags every co-author on a public domain.
_ATTRIBUTION = re.compile(
    r"^(co-authored-by|signed-off-by|reviewed-by|tested-by|helped-by)\s*:",
    re.IGNORECASE,
)
# A dotted name under a private or tailnet suffix: a fleet FQDN. Public domains
# (`flock.dev`, `github.com/…`) are how commit messages legitimately name things,
# and a hosted service is not a machine.
# Only `.ts.net` and `.home.arpa`: unambiguous MagicDNS shapes. `.local` and
# `.internal` are NOT included, because prose legitimately says `config.local`
# (a filename) and a commit message that mentions one is not leaking a machine.
_PROSE_TAILNET = re.compile(r"\b([A-Za-z0-9][A-Za-z0-9.-]*\.(?:ts\.net|home\.arpa))\b")
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


# A hosted service is how prose legitimately names things — `flock.dev`,
# `github.com/…`, `support@github.com`. Judging it is how a gate learns to be
# bypassed, and it is never the author's own machine.
_PUBLIC_TLDS = {
    "com", "org", "net", "io", "dev", "sh", "app", "ai", "co", "me", "gov",
    "edu", "cloud", "services", "uk", "de", "fr", "nl", "eu", "us", "info",
}
_PRIVATE_SUFFIXES = (".ts.net", ".local", ".internal", ".home.arpa", ".lan")


def _is_public_service(host: str) -> bool:
    low = host.lower()
    if low.endswith(_PRIVATE_SUFFIXES):
        return False
    return "." in low and low.rsplit(".", 1)[-1] in _PUBLIC_TLDS


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
    """Return (line_no, found_label, kind) for each undeclared host in test code.

    Judged here: a host-shaped string in a keyed field (`host`, `origin`, `ssh`,
    `ssh_target`, …), Rust's `host = Some("…")`, a JSON host field, and the host
    component of an `agent_<host>_<suffix>` id when that component is structured.

    Deliberately NOT judged here: the prose shapes. Inside a `.rs` test a
    `user@host` is far more often terminal content — a screen dump, a byte
    fixture — than a dial target, and judging those is how a gate ends up
    flagging `b"…_delta@omega"`. Prose gets its own path, in `check_free_text`.
    """
    findings: list[tuple[int, str, str]] = []
    all_lines = text.splitlines()
    for line_no, line in enumerate(all_lines, start=1):
        previous = all_lines[line_no - 2] if line_no >= 2 else ""
        if ESCAPE in line or ESCAPE in previous:
            continue
        if line.lstrip().startswith("//"):
            continue
        if not any(start <= line_no <= end for start, end in ranges):
            continue
        seen: set[str] = set()
        for pattern in (_KEYED, _JSON_HOST, _RUST_HOST):
            for match in pattern.finditer(line):
                seen.update(_labels_in(match.group(1)))
        for literal in _STRING_LITERAL.findall(line):
            for match in _AGENT_ID.finditer(literal):
                seen.add(match.group(1))
        for label in seen:
            low = label.lower()
            # A reserved name needs no declaration, and neither does its short
            # form: `spoke9.invalid` and `spoke9` are the same fiction. Nor does a
            # hosted service: a git fixture legitimately says `github.com`.
            if low in allowed or _is_reserved(low) or _is_public_service(low):
                continue
            findings.append((line_no, label, "host-field"))
    return findings



# --------------------------------------------------------------------------
def check_free_text(
    text: str, allowed: set[str], labels: dict[str, set[str]]
) -> list[tuple[int, str, str]]:
    """Check prose — the same two rule sets, without the test-region scoping.

    This is where the original leak was worst: 97 issue and PR bodies, 28
    comments, 2 titles, and commit subjects that `scripts/changelog.py` turns into
    release notes. A file-scanning gate cannot reach any of it.

    Both halves apply. The public rules need nothing private, so they run in CI
    over a PR body. The private rules (derived from this machine's ssh state) are
    empty in CI and that is the honest limit: a bare hostname in a tracker body is
    caught locally, and a structured one is caught everywhere.
    """
    findings: list[tuple[int, str, str]] = []
    for line_no, line in enumerate(text.splitlines(), start=1):
        if _ATTRIBUTION.match(line.strip()):
            continue
        seen: set[str] = set()
        for pattern in (_KEYED, _JSON_HOST):
            for match in pattern.finditer(line):
                seen.update(_labels_in(match.group(1)))
        for literal in _STRING_LITERAL.findall(line):
            for match in _AGENT_ID.finditer(literal):
                seen.add(match.group(1))
        # `user@host` is a host however it is spelled, so the bare form is fine
        # here — unlike a keyed field.
        for match in _PROSE_USER_AT_HOST.finditer(line):
            if match.group(1).lower() not in _PLACEHOLDER_HOSTS:
                seen.add(match.group(1))
        for match in _PROSE_TAILNET.finditer(line):
            seen.add(match.group(1))
        for label in seen:
            low = label.lower()
            if low in allowed or _is_reserved(label) or _is_public_service(label):
                continue
            findings.append((line_no, label, "host-field"))

        if labels:
            try:
                import ssh_hosts_gate as private
            except ImportError:  # pragma: no cover - import shape
                private = None
            if private is not None:
                for line_no, rule, label, _src in private.check_text(
                    line + "\n", Path("<text>"), labels
                ):
                    findings.append((line_no, label, f"private:{rule}"))
    return findings


def _read_free_text(args: list[str]) -> list[tuple[str, str]]:
    """(label, text) pairs from --message-file / --body-file / --text-file."""
    out: list[tuple[str, str]] = []
    for i, arg in enumerate(args):
        if arg.startswith("--") and i + 1 < len(args) and arg in (
            "--message-file",
            "--body-file",
            "--text-file",
        ):
            path = Path(args[i + 1])
            try:
                out.append((arg.lstrip("-"), path.read_text(encoding="utf-8")))
            except OSError:
                continue
    return out


try:  # reuse the single implementation of "what is test code"
    from scripts.hermetic_tests import _test_line_ranges
except ImportError:  # invoked as a script from the repo root
    from hermetic_tests import _test_line_ranges


def main(argv: list[str]) -> int:
    allowed = declared_labels()
    failed = False

    # --- prose: commit messages, PR/issue titles, bodies, comments ----------
    free = _read_free_text(argv)
    if free:
        labels: dict[str, set[str]] = {}
        if "--no-private" not in argv:
            try:
                import ssh_hosts_gate

                labels = ssh_hosts_gate.candidates()
            except ImportError:
                labels = {}
        for source, text in free:
            for line_no, label, kind in check_free_text(text, allowed, labels):
                failed = True
                print(
                    f"{source}:{line_no}: [{kind}] {label!r} is a machine name. "
                    f"This repository, its issues and its pull requests are public."
                )
        if failed:
            print()
            print("Commit messages, titles and bodies are published too (#510).")
            print(f"Escape hatch: `{ESCAPE}(fixture): <reason>` on the line.")
        return 1 if failed else 0

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