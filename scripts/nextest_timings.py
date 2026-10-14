"""Reproduce #1317 sizing from CI JUnit artifacts, including partial runs.

Download with --download, then render the same input again without network access.
Missing artifacts fail the download rather than silently shrinking the sample.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
import json
import math
from pathlib import Path
import subprocess
import xml.etree.ElementTree as ET

LEGS = ("macos-15", "ubuntu-latest")


def download(root: Path, count: int) -> None:
    root.mkdir(parents=True, exist_ok=True)
    runs = json.loads(subprocess.check_output([
        "gh", "run", "list", "--repo", "gerchowl/flock", "--workflow", "ci.yml",
        "--branch", "dev", "--status", "completed", "--limit", str(count),
        "--json", "databaseId,conclusion,createdAt",
    ], text=True))
    if len(runs) != count:
        raise ValueError(f"requested {count} runs, found {len(runs)}")
    (root / "runs.json").write_text(json.dumps(runs, indent=2) + "\n")
    for run in runs:
        subprocess.run([
            "gh", "run", "download", str(run["databaseId"]), "--repo", "gerchowl/flock",
            "--pattern", "junit-*", "--dir", str(root / str(run["databaseId"])),
        ], check=True)


def samples(root: Path, runs: list[dict]) -> dict:
    values = defaultdict(list)
    for run in runs:
        reports = sorted((root / str(run["databaseId"])).glob("junit-*/*.xml"))
        if not reports:
            raise ValueError(f"no reports for run {run['databaseId']}")
        for path in reports:
            leg = next((leg for leg in LEGS if path.parent.name.startswith(f"junit-{leg}-")), None)
            if leg is None:
                raise ValueError(f"unknown runner: {path}")
            for suite in ET.parse(path).getroot().iter("testsuite"):
                binary = suite.attrib["name"].split("::", 1)[-1]
                for case in suite.findall("testcase"):
                    if case.find("skipped") is not None:
                        continue
                    duration = float(case.attrib["time"])
                    if not math.isfinite(duration) or duration < 0:
                        raise ValueError(f"invalid duration in {path}: {duration}")
                    values[binary, leg].append(duration)
    if {leg for _, leg in values} != set(LEGS):
        raise ValueError("measurements must include both CI platforms")
    return values


def p99(values: list[float]) -> float:
    """Nearest-rank percentile of recorded test executions, not binary totals."""
    return sorted(values)[math.ceil(0.99 * len(values)) - 1]


def report(root: Path) -> str:
    runs = json.loads((root / "runs.json").read_text())
    values = samples(root, runs)
    lines = [
        "# Nextest hang-ceiling measurements (#1317)", "",
        "Reproduce: `python3 scripts/nextest_timings.py .local/nextest-timings --download > docs/next/nextest-timings.md`.",
        "Omit `--download` to render the saved input again. Requires authenticated `gh` and Python 3.", "",
        f"Latest {len(runs)} completed dev CI runs at collection time, including cancelled runs.",
        f"Recorded executions: {sum(map(len, values.values())):,}; binaries: {len({binary for binary, _ in values})}.",
        "Partial runs contribute only recorded executions, never zeroes for missing tests.",
        "JUnit records final attempts and may omit earlier retries or tests cancelled before reporting.",
        "This is an observed sample, not a guarantee about future load or absent binaries.",
        "p99 uses nearest rank per binary per platform. The larger platform p99 sets sizing.",
        "Ceilings round up to 60-second periods: at least 10× p99 and at least 180 seconds.", "",
        "Platform gaps: macOS live_handoff is excluded by `.config/ci-shards.toml`.",
        "Ubuntu session_health is unobserved because `tests/session_health.rs` is macOS-only",
        '(`#![cfg(target_os = "macos")]`): it tests macOS session loss using `sandbox-exec` and mach-lookup denial.', "",
        "| Run | Created (UTC) | Result | Reports |", "| --- | --- | --- | ---: |",
    ]
    for run in runs:
        count = len(list((root / str(run["databaseId"])).glob("junit-*/*.xml")))
        lines.append(f"| [{run['databaseId']}](https://github.com/gerchowl/flock/actions/runs/{run['databaseId']}) | {run['createdAt']} | {run['conclusion']} | {count} |")
    lines += ["", "| Binary | macOS n / p99 s / max s | Ubuntu n / p99 s / max s | Minimum ceiling s |",
              "| --- | ---: | ---: | ---: |"]
    for binary in sorted({binary for binary, _ in values}):
        cells = []
        worst = 0.0
        for leg in LEGS:
            data = values.get((binary, leg), [])
            if data:
                tail = p99(data)
                worst = max(worst, tail)
                cells.append(f"{len(data)} / {tail:.3f} / {max(data):.3f}")
            else:
                cells.append("unobserved")
        minimum = max(180, math.ceil(10 * worst / 60) * 60)
        lines.append(f"| `{binary}` | {' | '.join(cells)} | {minimum} |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--runs", type=int, default=20)
    args = parser.parse_args()
    if args.download:
        download(args.root, args.runs)
    print(report(args.root), end="")


if __name__ == "__main__":
    main()
