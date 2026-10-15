#!/usr/bin/env python3
"""Split one CI leg's nextest run into parallel shards, and prove nothing fell out (#991).

The serial-pty group (`.config/nextest.toml`) runs one test at a time, so on a
single runner it was a ~20-minute chain that every PR waited on. CI now builds
the test binaries once per platform into a nextest archive and runs the group
as several shard jobs, each still one-at-a-time on its own runner, next to a
few parallel shards for everything else. `.config/ci-shards.toml` says which binary goes
where. This script is the only place that turns that file into filters.

Subcommands:

  filter   print the nextest filter for one shard of one leg
  verify   list every shard's tests from the archive and fail unless they
           partition the leg's base set exactly, then write the shard matrix
           to $GITHUB_OUTPUT
  summary  render JUnit reports as a Markdown job summary: per-shard wall
           clock, slowest tests, per-binary totals

`verify` asks nextest itself which tests each filter selects rather than
re-implementing the filter language, so a typo, a renamed binary or a new
serial-pty binary that no shard names fails the build instead of silently
shrinking coverage.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SHARDS_TOML = ROOT / ".config" / "ci-shards.toml"
NEXTEST_TOML = ROOT / ".config" / "nextest.toml"

SERIAL_GROUP = "serial-pty"


@dataclass(frozen=True)
class Shard:
    name: str
    binaries: tuple[str, ...]
    serial: bool
    rest: bool = False


@dataclass(frozen=True)
class Config:
    legs: dict[str, str]
    shards: tuple[Shard, ...]

    def shard(self, name: str) -> Shard | None:
        for shard in self.shards:
            if shard.name == name:
                return shard
        return None

    def named_binaries(self) -> list[str]:
        return [b for s in self.shards for b in s.binaries]


def load_config(path: Path = SHARDS_TOML) -> Config:
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    legs = {name: leg["filter"] for name, leg in data["legs"].items()}
    shards = tuple(
        Shard(s["name"], tuple(s.get("binaries", ())), bool(s["serial"]), bool(s.get("rest", False)))
        for s in data["shards"]
    )
    names = [s.name for s in shards]
    if len(set(names)) != len(names):
        raise SystemExit(f"ci-shards.toml: shard names must be unique: {names}")
    rest = [s.name for s in shards if s.rest]
    if len(rest) != 1:
        raise SystemExit(f"ci-shards.toml: exactly one shard must say rest = true, found {rest}")
    for shard in shards:
        if shard.rest == bool(shard.binaries):
            raise SystemExit(f"ci-shards.toml: {shard.name} needs either binaries or rest = true")
    return Config(legs, shards)


def _binaries_expr(binaries: tuple[str, ...] | list[str]) -> str:
    return " | ".join(f"binary({b})" for b in binaries)


def shard_filter(config: Config, leg: str, shard_name: str) -> str:
    """The nextest filter for `shard_name` on `leg`, always within the leg's base set."""
    if leg not in config.legs:
        raise SystemExit(f"unknown leg {leg!r}; known: {sorted(config.legs)}")
    shard = config.shard(shard_name)
    if shard is None:
        raise SystemExit(f"unknown shard {shard_name!r}; known: {[s.name for s in config.shards]}")
    base = config.legs[leg]
    if shard.rest:
        return f"({base}) & not ({_binaries_expr(config.named_binaries())})"
    return f"({base}) & ({_binaries_expr(shard.binaries)})"


def serial_group_binaries(nextest_toml: Path = NEXTEST_TOML) -> set[str]:
    """Every binary `.config/nextest.toml` puts in the serial-pty group."""
    data = tomllib.loads(nextest_toml.read_text(encoding="utf-8"))
    found: set[str] = set()
    # A literal glob has equality semantics in nextest. Reject every other
    # expression rather than guessing which binaries it puts in this group.
    selector = r"binary\(\s*(=?[A-Za-z0-9_.-]+)\s*\)"
    union = rf"\s*{selector}(?:\s*\|\s*{selector})*\s*"
    for profile in ("default", "ci"):
        overrides = data.get("profile", {}).get(profile, {}).get("overrides", [])
        for index, override in enumerate(overrides):
            if override.get("test-group") != SERIAL_GROUP:
                continue
            expression = override.get("filter", "")
            if "platform" in override:
                raise SystemExit(
                    f"{nextest_toml}: unsupported {SERIAL_GROUP} platform condition in "
                    f"profile.{profile}.overrides[{index}]: {override['platform']!r}"
                )
            if not re.fullmatch(union, expression):
                raise SystemExit(
                    f"{nextest_toml}: unsupported {SERIAL_GROUP} filter in "
                    f"profile.{profile}.overrides[{index}]: {expression!r}. "
                    "Use a union of literal binary(name) or binary(=name) selectors"
                )
            found.update(name.removeprefix("=") for name in re.findall(selector, expression))
    return found


def check_assignment(config: Config, serial: set[str]) -> list[str]:
    """Problems with how ci-shards.toml places binaries relative to the serial-pty group."""
    problems = []
    seen: dict[str, str] = {}
    for shard in config.shards:
        for binary in shard.binaries:
            if binary in seen:
                problems.append(f"{binary} is in both {seen[binary]} and {shard.name}")
            seen[binary] = shard.name
            if shard.serial and binary not in serial:
                problems.append(
                    f"{binary} is in serial shard {shard.name} but not in the {SERIAL_GROUP} "
                    "group, so it would run in parallel there; keep the two files in step"
                )
            if not shard.serial and binary in serial:
                problems.append(
                    f"{binary} is in the {SERIAL_GROUP} group but in parallel shard {shard.name}; "
                    "move it to a serial shard"
                )
    for binary in sorted(serial - seen.keys()):
        problems.append(
            f"{binary} is in the {SERIAL_GROUP} group but no serial shard names it, so the rest "
            "shard would carry it; add it to a serial shard in .config/ci-shards.toml"
        )
    for shard in config.shards:
        if shard.rest and shard.serial:
            problems.append(f"the rest shard {shard.name} cannot be serial")
    return problems


def check_partition(base: set[str], shards: dict[str, set[str]]) -> list[str]:
    """Problems with `shards` as a partition of `base`: overlap, gaps, strays."""
    problems = []
    owner: dict[str, str] = {}
    for name, tests in shards.items():
        for test in tests:
            if test in owner:
                problems.append(f"{test} runs in both {owner[test]} and {name}")
            owner[test] = name
    union = set(owner)
    for test in sorted(base - union)[:50]:
        problems.append(f"{test} is in the base set but no shard runs it")
    for test in sorted(union - base)[:50]:
        problems.append(f"{test} runs in {owner[test]} but is not in the base set")
    return problems


def nextest_list(archive: str, expr: str) -> dict:
    """nextest's JSON listing of `expr` from `archive`."""
    out = subprocess.run(
        [
            "cargo", "nextest", "list",
            "--archive-file", archive,
            "--workspace-remap", str(ROOT),
            "--message-format", "json",
            "-E", expr,
        ],
        check=True,
        stdout=subprocess.PIPE,
        cwd=ROOT,
    ).stdout
    return json.loads(out)


def matched_tests(listing: dict) -> set[str]:
    tests = set()
    for binary_id, suite in listing["rust-suites"].items():
        for name, case in suite.get("testcases", {}).items():
            if case.get("filter-match", {}).get("status") == "matches":
                tests.add(f"{binary_id} {name}")
    return tests


def binary_names(listing: dict) -> set[str]:
    return {suite["binary-name"] for suite in listing["rust-suites"].values()}


def matched_binary_names(listing: dict) -> set[str]:
    """Binaries with at least one selected testcase, for truthful shard labels."""
    return {
        suite["binary-name"]
        for suite in listing["rust-suites"].values()
        if any(case.get("filter-match", {}).get("status") == "matches"
               for case in suite.get("testcases", {}).values())
    }


def verify(config: Config, leg: str, archive: str) -> int:
    problems = check_assignment(config, serial_group_binaries())
    known = binary_names(nextest_list(archive, "all()"))
    for binary in config.named_binaries():
        if binary not in known:
            problems.append(f"{binary} is named in ci-shards.toml but is not a test binary")

    base = matched_tests(nextest_list(archive, config.legs[leg]))
    listings = {
        shard.name: nextest_list(archive, shard_filter(config, leg, shard.name))
        for shard in config.shards
    }
    selected = {name: matched_tests(listing) for name, listing in listings.items()}
    problems += check_partition(base, selected)

    for name, tests in selected.items():
        print(f"{leg} {name}: {len(tests)} tests")
    print(f"{leg} total: {sum(map(len, selected.values()))} of {len(base)} in the base set")
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1

    # A shard can be empty on one leg (macOS skips live_handoff), and nextest
    # fails a run that selects nothing, so only non-empty shards get a job.
    matrix = [
        {"name": s.name, "label": "rest" if s.rest else ", ".join(sorted(matched_binary_names(listings[s.name])))}
        for s in config.shards
        if selected[s.name]
    ]
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as fh:
            fh.write(f"shards={json.dumps(matrix)}\n")
    return 0


@dataclass(frozen=True)
class Case:
    binary: str
    name: str
    seconds: float
    outcome: str


@dataclass(frozen=True)
class Report:
    shard: str
    seconds: float
    cases: tuple[Case, ...]


def parse_junit(path: Path, shard: str) -> Report:
    root = ET.parse(path).getroot()
    cases = []
    for suite in root.iter("testsuite"):
        binary = suite.get("name", "").split("::", 1)[-1]
        for case in suite.iter("testcase"):
            if case.find("failure") is not None or case.find("error") is not None:
                outcome = "failed"
            elif case.find("flakyFailure") is not None or case.find("flakyError") is not None:
                outcome = "flaky"
            elif case.find("skipped") is not None:
                outcome = "skipped"
            else:
                outcome = "passed"
            cases.append(Case(binary, case.get("name", ""), float(case.get("time", 0)), outcome))
    return Report(shard, float(root.get("time", 0)), tuple(cases))


def render_summary(title: str, reports: list[Report], top: int = 20) -> str:
    lines = [f"## {title}", ""]
    lines += ["| shard | wall clock | tests | failed | flaky |", "|---|---:|---:|---:|---:|"]
    for r in sorted(reports, key=lambda r: -r.seconds):
        failed = sum(c.outcome == "failed" for c in r.cases)
        flaky = sum(c.outcome == "flaky" for c in r.cases)
        lines.append(f"| {r.shard} | {r.seconds:.0f} s | {len(r.cases)} | {failed} | {flaky} |")

    cases = [(r.shard, c) for r in reports for c in r.cases]
    lines += ["", f"### Slowest {top} tests", "", "| test | shard | time |", "|---|---|---:|"]
    for shard, c in sorted(cases, key=lambda sc: -sc[1].seconds)[:top]:
        lines.append(f"| `{c.binary}::{c.name}` | {shard} | {c.seconds:.1f} s |")

    totals: dict[tuple[str, str], list[float]] = {}
    for shard, c in cases:
        totals.setdefault((c.binary, shard), []).append(c.seconds)
    lines += ["", "### Per-binary totals", "", "| binary | shard | tests | total |", "|---|---|---:|---:|"]
    for (binary, shard), times in sorted(totals.items(), key=lambda kv: -sum(kv[1])):
        lines.append(f"| {binary} | {shard} | {len(times)} | {sum(times):.0f} s |")
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("filter")
    p.add_argument("--leg", required=True)
    p.add_argument("--shard", required=True)
    p = sub.add_parser("verify")
    p.add_argument("--leg", required=True)
    p.add_argument("--archive-file", required=True)
    p = sub.add_parser("summary")
    p.add_argument("--title", required=True)
    p.add_argument(
        "reports",
        nargs="*",
        help="SHARD=PATH pairs, or directories holding <shard>/junit.xml",
    )
    p.add_argument("--strip-prefix", default="", help="drop this from directory-derived shard names")
    args = parser.parse_args(argv)

    if args.cmd == "filter":
        print(shard_filter(load_config(), args.leg, args.shard))
        return 0
    if args.cmd == "verify":
        return verify(load_config(), args.leg, args.archive_file)

    reports = []
    for spec in args.reports:
        if "=" in spec:
            shard, path = spec.split("=", 1)
            if Path(path).is_file():
                reports.append(parse_junit(Path(path), shard))
            continue
        for path in sorted(Path(spec).glob("*/junit.xml")):
            reports.append(parse_junit(path, path.parent.name.removeprefix(args.strip_prefix)))
    sys.stdout.write(render_summary(args.title, reports))
    return 0


if __name__ == "__main__":
    sys.exit(main())
