#!/usr/bin/env python3
"""Gate: do not commit a real machine's name into this public repository.

`flock` is a public fork and it federates a private SSH fleet, so a hostname
that reaches a commit is a hostname the world can read, in the tracker as well
as in the tree. It happened: 92 files, 97 issue bodies, 28 comments and two
titles (#510, #511). It then happened twice more within the hour, from ordinary
feature commits (#514, #515) — which is the argument for a gate rather than a
cleanup.

The rule is not "grep for my machines". It is **context-scoped**: a candidate is
flagged only where a value is being used *as a host* — an ssh destination, a
reported hostname, a peer identity, an agent-id component. Whole-word matching
was measured and rejected: of 91 labels derived from a real ssh setup, one is
also a Nerd Font glyph name and one is a common English word that appears in
tracked files hundreds of times. A fuzzy gate fires on those constantly and is
`--no-verify`'d within a day, which is worse than no gate.

Where the candidates come from — derived, never maintained, because a
hand-written list dies at the first machine rename:

* ``~/.ssh/config`` — ``Host`` aliases plus ``HostName`` / ``ProxyJump`` values
* ``~/.ssh/known_hosts`` — plaintext entries (hashed ``|1|`` entries yield no
  label and are skipped; that is a coverage loss, not a failure)
* the local machine: ``uname -s`` and ``scutil --get LocalHostName``
* ``tailscale status --json`` DNS names, when tailscale is installed and answers

Two deliberate properties:

* **Fails open.** No ssh state means no candidates means nothing to flag. That is
  correct here — the state being protected is one person's, and a third-party
  contributor has none of it. This gate therefore cannot and does not run in CI;
  the CI-bindable rules are `scripts/fixture_hosts.py` and the icon-collision
  test in `src/server_icons.rs`.
* **Read-only, and never writes the value anywhere.** Only this host's own ssh
  state is read, and a finding is printed to stderr for a human. Nothing is
  copied into the repository, and the output is not something to paste into a
  commit message.

Escape hatch, matching the other gates: ``guardrails-ok(ssh-hosts): <reason>``.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ESCAPE = "guardrails-ok"

ROOT = Path(__file__).resolve().parent.parent

# Never treated as a machine identity: too short to be one, or not a name at all.
_SKIP_LABELS = {
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "broadcasthost",
    "all",
    "none",
    "self",
    "home",
}

# A dotted name under one of these is a public service you have ssh'd to, not a
# machine you own: `git@github.com` is in everyone's known_hosts, and expanding
# `github.com` to the label `github` turns every git URL in the tree into a
# finding. A fleet host is a short label, a `.local`, or a tailnet name.
# Checked before `_PUBLIC_TLDS`, because these end in a public TLD while being
# exactly the fleet names this gate exists for.
_PRIVATE_SUFFIXES = (".ts.net", ".local", ".internal", ".home.arpa", ".lan")

_PUBLIC_TLDS = {
    "com", "org", "net", "io", "dev", "sh", "app", "ai", "co", "me", "gov",
    "edu", "cloud", "services", "io", "uk", "de", "fr", "nl", "eu",
}

_IPV4 = re.compile(r"^\d{1,3}(\.\d{1,3}){3}$")
_IPV6 = re.compile(r"^[0-9a-fA-F:]+$")
_LABEL_OK = re.compile(r"^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$")

# Where a string is being used AS a host. Each entry is (rule, pattern); capture
# group 1, when present, is the host portion of the match.
CONTEXTS: list[tuple[str, re.Pattern[str]]] = [
    (
        "ssh-target",
        re.compile(
            r"""\b(?:ssh|ssh_target|sshTarget|ProxyJump|HostName)\b\s*[:=]\s*["']([^"']+)["']"""
        ),
    ),
    (
        "json-host-field",
        re.compile(
            r""""(?:host|hostname|origin|from_host|to_host|ssh|ssh_target|proxy_jump|peer)"\s*:\s*"([^"]+)\""""
        ),
    ),
    (
        # Rust writes `peer.host = Some("atlas".into())`; the JSON-shaped rule
        # never sees it, and this gate has to work on .rs files.
        "rust-host-field",
        re.compile(
            r"""\b(?:host|hostname|origin|from_host|to_host|peer)\s*[:=]\s*(?:Some\(\s*)?"([^"]+)\""""
        ),
    ),
    ("user-at-host", re.compile(r"\b[A-Za-z0-9._-]+@([A-Za-z0-9][A-Za-z0-9.-]*)\b")),
    (
        "agent-id",
        re.compile(r"\bagent_([a-z][a-z0-9-]*)_[A-Za-z0-9]+\b"),
    ),
]

SCANNED_SUFFIXES = {
    ".rs",
    ".md",
    ".mdx",
    ".toml",
    ".json",
    ".yml",
    ".yaml",
    ".py",
    ".sh",
    ".txt",
}


# --------------------------------------------------------------------------
# icon vocabulary: public names that must never be read as a machine identity
# --------------------------------------------------------------------------
def icon_names(root: Path = ROOT) -> set[str]:
    """The registered server-icon names, lowercased.

    They are Nerd Font glyph names — third-party vocabulary, and several are
    ordinary English words. Suppressing them here is what lets the host rules
    stay narrow without flagging a legitimate icon.
    """
    src = root / "src" / "server_icons.rs"
    try:
        lines = src.read_text(encoding="utf-8").splitlines()
    except OSError:
        return set()
    try:
        start = next(i for i, l in enumerate(lines) if l.startswith("pub fn known_names()"))
        open_i = next(i for i in range(start, len(lines)) if lines[i].strip() == "&[")
        end = next(i for i in range(open_i, len(lines)) if lines[i].strip() == "]")
    except StopIteration:
        return set()
    names: set[str] = set()
    for line in lines[open_i + 1 : end]:
        names.update(re.findall(r'"([^"]+)"', line))
    return {n.lower() for n in names}


# --------------------------------------------------------------------------
# candidate derivation
# --------------------------------------------------------------------------
def _split_destination(value: str) -> list[str]:
    """Host labels a destination could refer to, most specific first.

    ``user@host.example:22`` yields ``host.example`` and ``host`` — the dotted
    form is what the config and known_hosts hold, the short form is what a peer
    entry or a prose reference uses. A comma-separated ProxyJump chain and a
    phrase such as ``via hub`` both yield one entry per hop-shaped token.
    """
    value = value.strip().strip("\"'")
    if not value:
        return []
    out: list[str] = []
    for part in re.split(r"[,\s]+", value):
        # A leading `-` is an ssh OPTION, not a destination — the case that
        # #392 exists for, and one this gate must not launder into a hostname.
        if not part or part.startswith("-"):
            continue
        host = part.rsplit("@", 1)[-1].split(":", 1)[0]
        if not host or host.startswith("-"):
            continue
        # A public service is not a machine, and neither is its short form:
        # expanding `github.com` to `github` would flag every git URL in the tree.
        if _is_public_service(host):
            continue
        out.append(host)
        if "." in host:
            out.append(host.split(".", 1)[0])
    return out


def _is_public_service(host: str) -> bool:
    """A dotted name outside the private suffixes is a hosted service.

    Being in `~/.ssh/config` does not make `gitlab.psi.ch` a machine of the
    author's — it is a hosted GitLab, and flagging it in a test that legitimately
    mentions it is the false positive that gets a gate bypassed. A fleet is single
    labels, `.local`, or MagicDNS, which is exactly what survives this.
    """
    low = host.lower()
    if low.endswith(_PRIVATE_SUFFIXES):
        return False
    return "." in low


def _usable(label: str, skip: set[str]) -> bool:
    low = label.lower()
    if len(low) < 3 or low in skip or low in _SKIP_LABELS:
        return False
    if _is_public_service(label):
        return False
    if _IPV4.match(low) or (":" in low and _IPV6.match(low)):
        return False
    # Per-label validation, so a dotted name is checked as the labels it is made of.
    if not all(_LABEL_OK.match(part) for part in low.split(".")):
        return False
    # Hashed known_hosts entries carry no recoverable label.
    if label.startswith("|"):
        return False
    return True


def _run(cmd: list[str], timeout: int = 3) -> str:
    try:
        return subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout, check=False
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def candidates(home: Path | None = None, include_local: bool = True) -> dict[str, set[str]]:
    """Map each machine label to the ssh-state source that produced it.

    ``include_local`` covers the sources that are not files under ``home`` — the
    local hostname and the tailnet. It is false whenever a caller passes an
    explicit ``home``, because those sources cannot be redirected and reading the
    real machine from a test over a fake one is exactly the ambient state
    AGENTS.md forbids.
    """
    home = Path(home or Path.home())
    include_local = include_local and home == Path.home()
    skip = icon_names()
    found: dict[str, set[str]] = {}

    def add(label: str, source: str) -> None:
        if _usable(label, skip):
            found.setdefault(label.lower(), set()).add(source)

    config = home / ".ssh" / "config"
    try:
        text = config.read_text(encoding="utf-8", errors="replace")
    except OSError:
        text = ""
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.lower().startswith("host "):
            for alias in stripped.split()[1:]:
                if "*" not in alias and "?" not in alias:
                    add(alias, "ssh config Host")
        m = re.match(r"(?i)^\s*(?:hostname|proxyjump)\s+(.+)$", line)
        if m:
            for label in _split_destination(m.group(1)):
                add(label, "ssh config HostName/ProxyJump")

    known_hosts = home / ".ssh" / "known_hosts"
    try:
        kh = known_hosts.read_text(encoding="utf-8", errors="replace")
    except OSError:
        kh = ""
    for line in kh.splitlines():
        if not line or line.startswith("#"):
            continue
        for entry in line.split():
            if entry.startswith("|"):
                continue  # hashed: no label to recover
            for label in _split_destination(entry):
                add(label, "known_hosts")

    for cmd, source in (
        (["uname", "-s"], "local hostname"),
        (["scutil", "--get", "LocalHostName"], "local LocalHostName"),
        (["hostname", "-s"], "local short hostname"),
    ):
        if not include_local:
            break
        out = _run(cmd)
        if out:
            add(out, source)

    if include_local and (
        _run(["command", "-v", "tailscale"]) or Path("/Applications/Tailscale.app").exists()
    ):
        raw = _run(["tailscale", "status", "--json"], timeout=5)
        if raw:
            try:
                status = json.loads(raw)
            except json.JSONDecodeError:
                status = {}
            for peer in (status.get("Peer") or {}).values():
                name = peer.get("DNSName") or ""
                if name:
                    add(name.split(".", 1)[0], "tailscale peer")
                for host in peer.get("TailscaleIPs") or []:
                    add(host, "tailscale peer")
    return found


# --------------------------------------------------------------------------
# checking
# --------------------------------------------------------------------------
def check_text(
    text: str, path: Path, labels: dict[str, set[str]]
) -> list[tuple[int, str, str, str]]:
    """Return (line_no, rule, matched_label, source) for each violation."""
    if not labels:
        return []
    lines = text.splitlines()
    findings: list[tuple[int, str, str, str]] = []
    for line_no, line in enumerate(lines, start=1):
        previous = lines[line_no - 2] if line_no >= 2 else ""
        if ESCAPE in line or ESCAPE in previous:
            continue
        for rule, pattern in CONTEXTS:
            for match in pattern.finditer(line):
                raw = match.group(1) if match.groups() else match.group(0)
                candidates_here = _split_destination(raw) or [raw]
                # A suffixed alias (`kiln-dev`, `node-b`) still names its machine:
                # the part before the hyphen is a candidate the author owns.
                extra = [
                    part.split("-", 1)[0]
                    for part in list(candidates_here)
                    if "-" in part and len(part.split("-", 1)[0]) >= 3
                ]
                for label in candidates_here + extra:
                    low = label.lower()
                    if low in labels:
                        sources = ", ".join(sorted(labels[low]))
                        findings.append(
                            (line_no, rule, label, sources)
                        )
                        break
    return findings


def main(argv: list[str], labels: dict[str, set[str]] | None = None) -> int:
    labels = candidates() if labels is None else labels
    verbose = "--verbose" in argv
    paths = [a for a in argv if not a.startswith("--")]
    if not labels:
        if verbose:
            print("ssh-hosts: no local ssh state; nothing to check (fails open)")
        return 0

    failed = False
    for name in paths:
        path = Path(name)
        if path.suffix not in SCANNED_SUFFIXES:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for line_no, rule, label, sources in check_text(text, path, labels):
            failed = True
            print(
                f"{path}:{line_no}: [{rule}] {label!r} is one of this machine's "
                f"own hosts (from {sources}). This repository is public."
            )

    if failed:
        print()
        print("Do not commit a machine's name — it is published with the fork (#510).")
        print(
            "Use a fictional fixture label (see scripts/fixture-hosts.toml), or "
            f"add `{ESCAPE}(ssh-hosts): <reason>` to the line."
        )
        return 1
    if verbose:
        print(f"ssh-hosts: {len(labels)} local host labels checked, clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))