#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path
from typing import Any

ASSET_TARGETS = (
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
)
EXPECTED_ASSET_NAMES = {target: f"flock-{target}" for target in ASSET_TARGETS}
HIDDEN_SUBJECTS = (
    "release:",
    "docs: update website manifest",
    "docs: update preview manifest",
    "chore: approve contributor",
    "chore: approve merged contributor",
)
TYPE_HEADINGS = {
    "feat": "Added",
    "fix": "Fixed",
    "perf": "Performance",
    "docs": "Maintenance",
    "ci": "Maintenance",
    "test": "Maintenance",
    "refactor": "Maintenance",
    "chore": "Maintenance",
}
TYPE_ORDER = ("Added", "Fixed", "Performance", "Maintenance", "Other")
COMMIT_RE = re.compile(r"^(?P<kind>[a-z]+)(?:\([^)]+\))?!?:\s+(?P<body>.+)$")


def run_git(args: list[str], cwd: Path | None = None) -> str:
    # stderr is captured, not inherited: a git refusal carries the explanation a
    # reader needs, and it belongs on the exception the caller handles.
    return subprocess.check_output(
        ["git", *args], text=True, cwd=cwd, stderr=subprocess.PIPE
    ).strip()


def optional_git(args: list[str], cwd: Path | None = None) -> str | None:
    """git's answer, or None when it has none — as opposed to a failure."""
    result = subprocess.run(
        ["git", *args], capture_output=True, text=True, check=False, cwd=cwd
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def normalize_version(version: str) -> str:
    return version.strip().removeprefix("v")


def latest_stable_tag(cwd: Path | None = None) -> str | None:
    """The most recent `v*` tag, or None — which is this repository's state.

    `git describe` exits 128 with "No names found" when no tag matches, and
    `check_output` turns that into an exception. A repository that has not cut
    its first release is in exactly that state, so the answer is None rather
    than a crash on the path that is looking for one.
    """
    return optional_git(["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"], cwd)


def first_commit(cwd: Path | None = None) -> str | None:
    return optional_git(["rev-list", "--max-parents=0", "HEAD"], cwd)


def commit_exists(commit: str, cwd: Path | None = None) -> bool:
    return (
        optional_git(["rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"], cwd)
        is not None
    )


def read_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def previous_preview_commit(path: Path, cwd: Path | None = None) -> str | None:
    """The commit the current preview build came from, if this repository has it.

    The manifest's recorded sha is the one thing the next preview's notes are
    diffed from, and `git log <sha>..<commit>` fails outright on a sha this
    repository cannot resolve. That is not hypothetical input: the manifest is
    generated data whose recorded commit can become unreachable (a rebase, a
    squashed import, a file carried in from another lineage), and the failure
    mode is a preview run that dies before it publishes rather than one that
    notes something. A commit that does not resolve here is not a previous
    preview, so it is treated as absent and the caller falls back.
    """
    data = read_json(path)
    if not data:
        return None
    commit = data.get("commit")
    if not isinstance(commit, str) or not commit.strip():
        return None
    return commit if commit_exists(commit, cwd) else None


def preview_base_ref(manifest: Path, cwd: Path | None = None) -> str | None:
    """What the next preview's notes are diffed against, or None for all of it."""
    return previous_preview_commit(manifest, cwd) or latest_stable_tag(cwd) or first_commit(cwd)


def hidden_subject(subject: str) -> bool:
    lowered = subject.strip().lower()
    return any(lowered.startswith(prefix) for prefix in HIDDEN_SUBJECTS)


def latest_publishable_commit(ref: str) -> str:
    output = run_git(["log", "--pretty=format:%H%x00%s", ref])
    for line in output.splitlines():
        commit, _, subject = line.partition("\x00")
        if commit and not hidden_subject(subject):
            return commit
    raise SystemExit(f"no publishable commit found in {ref}")


def commit_subjects(previous: str | None, commit: str, cwd: Path | None = None) -> list[str]:
    rev_range = commit if previous is None else f"{previous}..{commit}"
    output = run_git(["log", "--pretty=format:%s", rev_range], cwd)
    if not output:
        return []
    subjects = []
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if hidden_subject(stripped):
            continue
        subjects.append(stripped)
    return subjects


def humanize_subject(subject: str) -> tuple[str, str]:
    match = COMMIT_RE.match(subject)
    if not match:
        return "Other", subject[0].upper() + subject[1:]
    kind = match.group("kind")
    body = match.group("body").strip()
    heading = TYPE_HEADINGS.get(kind, "Other")
    if body:
        body = body[0].upper() + body[1:]
    else:
        body = subject
    return heading, body


def build_notes(
    previous: str | None,
    commit: str,
    build_id: str,
    base_version: str,
    repo: str,
    cwd: Path | None = None,
) -> str:
    short = commit[:12]
    # With nothing to diff from, the compare link is the whole history rather
    # than a range — a URL naming `None` would 404 for every reader.
    compare_base = previous or first_commit(cwd) or commit
    compare = f"https://github.com/{repo}/compare/{compare_base}...{commit}"
    lines = [
        f"Preview build {build_id}",
        "",
        f"Built from `{short}` on `main`.",
        f"Base stable: v{normalize_version(base_version)}",
        f"Compare: {compare}",
        "",
    ]
    grouped: dict[str, list[str]] = {heading: [] for heading in TYPE_ORDER}
    for subject in commit_subjects(previous, commit, cwd):
        heading, body = humanize_subject(subject)
        grouped.setdefault(heading, []).append(body)

    wrote = False
    for heading in TYPE_ORDER:
        items = grouped.get(heading, [])
        if not items:
            continue
        wrote = True
        lines.append(f"### {heading}")
        for item in items:
            lines.append(f"- {item}")
        lines.append("")

    if not wrote:
        lines.extend(["### Changed", "- Rebuilt preview from the current main branch.", ""])

    return "\n".join(lines).rstrip() + "\n"


def default_asset_urls(repo: str, tag: str) -> dict[str, str]:
    return {
        target: f"https://github.com/{repo}/releases/download/{tag}/{EXPECTED_ASSET_NAMES[target]}"
        for target in ASSET_TARGETS
    }


def read_sha_file(path: Path | None) -> dict[str, str]:
    if path is None:
        return {}
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise SystemExit("sha file must be a JSON object")
    return {str(key): str(value) for key, value in data.items()}


def asset_objects(urls: dict[str, str], shas: dict[str, str]) -> dict[str, dict[str, str]]:
    assets: dict[str, dict[str, str]] = {}
    for target in ASSET_TARGETS:
        url = urls[target]
        entry = {"url": url}
        sha = shas.get(target)
        if sha:
            entry["sha256"] = sha
        assets[target] = entry
    return assets


def asset_url(entry: Any) -> str:
    """The download URL of an asset entry, which may be a bare string or an object."""
    if isinstance(entry, str):
        return entry.strip()
    if isinstance(entry, dict):
        return str(entry.get("url") or "").strip()
    return ""


def foreign_build_entries(builds: Any, repo: str) -> list[str]:
    """Archived preview builds whose assets are not this repository's releases.

    The mirror image of `scripts/changelog.py`'s check on the stable archive, and
    for the same reason: `build_manifest` carries the previous builds forward,
    so an entry left in the file is an entry re-published on every preview run,
    forever. A preview manifest pointing at another repository's builds hands a
    preview-channel user that repository's binary.
    """
    if not isinstance(builds, dict):
        return []

    foreign: list[str] = []
    for build_id, build in builds.items():
        if not isinstance(build, dict):
            continue
        assets = build.get("assets")
        if not isinstance(assets, dict):
            continue
        expected = default_asset_urls(repo, str(build.get("tag", "")))
        if any(asset_url(assets.get(target)) != expected[target] for target in ASSET_TARGETS):
            foreign.append(str(build_id))
    return sorted(foreign)


def build_manifest(
    output: Path,
    repo: str,
    tag: str,
    build_id: str,
    commit: str,
    built_at: str,
    base_version: str,
    protocol: int,
    notes: str,
    shas: dict[str, str],
    retain: int,
) -> str:
    urls = default_asset_urls(repo, tag)
    assets = asset_objects(urls, shas)
    current = read_json(output) or {}
    builds = current.get("builds") if isinstance(current.get("builds"), dict) else {}
    builds = dict(builds)
    dropped = foreign_build_entries(builds, repo)
    for stale_build_id in dropped:
        del builds[stale_build_id]
    builds[build_id] = {
        "base_version": normalize_version(base_version),
        "commit": commit,
        "built_at": built_at,
        "protocol": protocol,
        "tag": tag,
        "assets": assets,
    }
    ordered_builds = {
        key: builds[key]
        for key in sorted(
            builds,
            key=lambda key: str(builds[key].get("built_at", "")),
            reverse=True,
        )[:retain]
    }
    manifest = {
        "schema_version": 1,
        "channel": "preview",
        "base_version": normalize_version(base_version),
        "build_id": build_id,
        "commit": commit,
        "built_at": built_at,
        "protocol": protocol,
        "notes": notes.strip(),
        "assets": assets,
        "builds": ordered_builds,
    }
    return json.dumps(manifest, indent=2) + "\n"


def cmd_notes(args: argparse.Namespace) -> int:
    previous = args.previous or preview_base_ref(Path(args.manifest))
    notes = build_notes(previous, args.commit, args.build_id, args.base_version, args.repo)
    Path(args.output).write_text(notes, encoding="utf-8")
    return 0


def cmd_manifest(args: argparse.Namespace) -> int:
    notes = Path(args.notes).read_text(encoding="utf-8")
    shas = read_sha_file(Path(args.sha_file) if args.sha_file else None)
    existing = read_json(Path(args.output)) or {}
    dropped = foreign_build_entries(
        existing.get("builds") if isinstance(existing.get("builds"), dict) else {},
        args.repo,
    )
    content = build_manifest(
        output=Path(args.output),
        repo=args.repo,
        tag=args.tag,
        build_id=args.build_id,
        commit=args.commit,
        built_at=args.built_at,
        base_version=args.base_version,
        protocol=args.protocol,
        notes=notes,
        shas=shas,
        retain=args.retain,
    )
    Path(args.output).write_text(content, encoding="utf-8")
    if dropped:
        print(
            f"dropped {len(dropped)} archived preview build(s) not published by "
            f"{args.repo}: {', '.join(dropped)}"
        )
    return 0


def cmd_current_commit(args: argparse.Namespace) -> int:
    commit = previous_preview_commit(Path(args.manifest))
    if commit:
        print(commit)
    return 0


def cmd_select_commit(args: argparse.Namespace) -> int:
    print(latest_publishable_commit(args.ref))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Preview channel release helpers")
    sub = parser.add_subparsers(required=True)

    notes = sub.add_parser("notes")
    notes.add_argument("--manifest", default="website/preview.json")
    notes.add_argument("--previous")
    notes.add_argument("--commit", required=True)
    notes.add_argument("--build-id", required=True)
    notes.add_argument("--base-version", required=True)
    notes.add_argument("--repo", default="gerchowl/flock")
    notes.add_argument("--output", required=True)
    notes.set_defaults(func=cmd_notes)

    manifest = sub.add_parser("manifest")
    manifest.add_argument("--output", default="website/preview.json")
    manifest.add_argument("--repo", default="gerchowl/flock")
    manifest.add_argument("--tag", required=True)
    manifest.add_argument("--build-id", required=True)
    manifest.add_argument("--commit", required=True)
    manifest.add_argument("--built-at", required=True)
    manifest.add_argument("--base-version", required=True)
    manifest.add_argument("--protocol", required=True, type=int)
    manifest.add_argument("--notes", required=True)
    manifest.add_argument("--sha-file")
    manifest.add_argument("--retain", type=int, default=30)
    manifest.set_defaults(func=cmd_manifest)

    current = sub.add_parser("current-commit")
    current.add_argument("--manifest", default="website/preview.json")
    current.set_defaults(func=cmd_current_commit)

    select = sub.add_parser("select-commit")
    select.add_argument("--ref", default="origin/main")
    select.set_defaults(func=cmd_select_commit)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
