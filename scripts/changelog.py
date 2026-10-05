#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import date
from pathlib import Path
from typing import Any

try:  # imported as part of the `scripts` package by the maintenance tests
    from scripts.conventional_commits import (
        Commit,
        classify_commits,
        git_commits,
        recommended_bump,
    )
except ImportError:  # executed directly as scripts/changelog.py
    from conventional_commits import (  # type: ignore[no-redef]
        Commit,
        classify_commits,
        git_commits,
        recommended_bump,
    )

DEFAULT_LIVE_MANIFEST_URL = "https://flock.dev/latest.json"

SECTION_RE = re.compile(r"^##\s+(?:\[(?P<bracketed>[^\]]+)\]|(?P<plain>.+?))\s*$", re.MULTILINE)
VERSION_WITH_DATE_RE = re.compile(r"^(?P<version>.+?)\s+-\s+\d{4}-\d{2}-\d{2}$")
DEFAULT_RELEASE_REPO = "gerchowl/flock"
DEFAULT_LATEST_JSON_PATH = Path("website/latest.json")
DEFAULT_PRODUCT_ANNOUNCEMENT_PATH = Path("docs/next/product-announcement.json")
PROTOCOL_SOURCE_PATH = Path("src/protocol/wire.rs")
ASSET_TARGETS = (
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
)
EXPECTED_ASSET_NAMES = {target: f"flock-{target}" for target in ASSET_TARGETS}

# The manifest a repository publishes before it has published a release. It is a
# sentinel, not a version: `0.0.0` is below every real build of this binary, so
# `flk update` reports "already up to date" instead of fetching a binary, and
# remote bootstrap fails with "the release manifest does not include flock
# <your version>" instead of installing one. Both failure modes are loud; the
# alternative — advertising another project's number — is not. See ADR-0025.
NO_STABLE_RELEASE_VERSION = "0.0.0"
NO_STABLE_RELEASE_NOTES = (
    "### Maintenance\n"
    "- No stable Flock release has been published from this repository yet. This "
    "manifest advertises no installable version on purpose: `flk update` has "
    "nothing to install, and remote bootstrap asks you to build Flock on the "
    "remote host. See ADR-0025 for the version line."
)


@dataclass(frozen=True)
class Section:
    title: str
    start: int
    end: int
    body_start: int


class ChangelogError(ValueError):
    pass


def normalize_title(raw_title: str) -> str:
    title = raw_title.strip()
    match = VERSION_WITH_DATE_RE.match(title)
    if match:
        title = match.group("version").strip()
    if title.startswith("[") and title.endswith("]"):
        title = title[1:-1].strip()
    return title


def normalize_version(version: str) -> str:
    return version.strip().removeprefix("v")


def parse_version(version: str) -> tuple[int, int, int]:
    normalized = normalize_version(version)
    parts = normalized.split(".")
    if len(parts) != 3:
        raise ChangelogError(f"invalid version: {version}")
    try:
        return tuple(int(part) for part in parts)  # type: ignore[return-value]
    except ValueError as exc:
        raise ChangelogError(f"invalid version: {version}") from exc


def parse_sections(text: str) -> list[Section]:
    matches = list(SECTION_RE.finditer(text))
    sections: list[Section] = []

    for index, match in enumerate(matches):
        title = normalize_title(match.group("bracketed") or match.group("plain") or "")
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        body_start = match.end()
        if body_start < len(text) and text[body_start : body_start + 1] == "\n":
            body_start += 1
        sections.append(Section(title=title, start=match.start(), end=end, body_start=body_start))

    return sections


def find_section(text: str, wanted_title: str) -> Section:
    for section in parse_sections(text):
        if section.title == wanted_title:
            return section
    raise ChangelogError(f"section not found: {wanted_title}")


def extract_section_body(text: str, wanted_title: str) -> str:
    section = find_section(text, wanted_title)
    body = text[section.body_start : section.end].strip("\n")
    if not body.strip():
        raise ChangelogError(f"section is empty: {wanted_title}")
    return body + "\n"


def prepare_release(text: str, version: str, release_date: str) -> str:
    unreleased = None
    existing_version = False

    for section in parse_sections(text):
        if section.title == "Unreleased":
            unreleased = section
        if section.title == version:
            existing_version = True

    if existing_version:
        raise ChangelogError(f"version already exists in changelog: {version}")
    if unreleased is None:
        raise ChangelogError("missing Unreleased section")

    unreleased_body = text[unreleased.body_start : unreleased.end].strip("\n")
    if not unreleased_body.strip():
        raise ChangelogError("Unreleased section is empty")

    prefix = text[: unreleased.start].rstrip("\n")
    suffix = text[unreleased.end :].strip("\n")

    rebuilt = f"## Unreleased\n\n## [{version}] - {release_date}\n\n{unreleased_body}"
    if suffix:
        rebuilt += f"\n\n{suffix}"

    if prefix:
        return f"{prefix}\n\n{rebuilt}\n"
    return rebuilt + "\n"


def read_protocol_version(source_path: Path = PROTOCOL_SOURCE_PATH) -> int:
    content = source_path.read_text(encoding="utf-8")
    match = re.search(r"pub const PROTOCOL_VERSION: u32 = (\d+);", content)
    if not match:
        raise ChangelogError(f"could not read PROTOCOL_VERSION from {source_path}")
    return int(match.group(1))


def normalize_announcement(value: Any, label: str) -> dict[str, str] | None:
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ChangelogError(f"{label} announcement must be an object")

    allowed_keys = {"id", "title", "body"}
    extra_keys = sorted(set(value) - allowed_keys)
    if extra_keys:
        raise ChangelogError(
            f"{label} announcement has unsupported field(s): {', '.join(extra_keys)}"
        )

    announcement: dict[str, str] = {}
    for key in ("id", "title", "body"):
        field_value = value.get(key)
        if not isinstance(field_value, str) or not field_value.strip():
            raise ChangelogError(f"{label} announcement is missing non-empty string field: {key}")
        announcement[key] = field_value.strip()

    if not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", announcement["id"]):
        raise ChangelogError(
            f"{label} announcement has invalid id; use lowercase letters, numbers, dots, underscores, or dashes"
        )

    return announcement


def infer_protocol_from_notes(notes: str) -> int | None:
    match = re.search(r"protocol(?: is now)? version (\d+)", notes, flags=re.IGNORECASE)
    if match is None:
        return None
    return int(match.group(1))


def normalize_assets(value: Any, label: str) -> dict[str, str]:
    if not isinstance(value, dict):
        raise ChangelogError(f"{label} must be an object")

    missing_targets = [target for target in ASSET_TARGETS if target not in value]
    if missing_targets:
        raise ChangelogError(f"{label} is missing asset URL for {', '.join(missing_targets)}")

    normalized_assets: dict[str, str] = {}
    for target in ASSET_TARGETS:
        url = value.get(target)
        if not isinstance(url, str) or not url.strip():
            raise ChangelogError(f"{label} is missing asset URL for {target}")
        normalized_assets[target] = url.strip()
    return normalized_assets


def normalize_release_metadata(value: Any, label: str, version: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ChangelogError(f"{label} must be an object")

    allowed_keys = {"notes", "announcement", "assets", "protocol"}
    extra_keys = sorted(set(value) - allowed_keys)
    if extra_keys:
        raise ChangelogError(f"{label} has unsupported field(s): {', '.join(extra_keys)}")

    notes = value.get("notes")
    if not isinstance(notes, str) or not notes.strip():
        raise ChangelogError(f"{label} is missing non-empty release notes")

    metadata: dict[str, Any] = {"notes": notes.strip()}
    protocol = value.get("protocol")
    if protocol is not None:
        if not isinstance(protocol, int):
            raise ChangelogError(f"{label}.protocol must be an integer")
        metadata["protocol"] = protocol
    else:
        inferred_protocol = infer_protocol_from_notes(notes)
        if inferred_protocol is not None:
            metadata["protocol"] = inferred_protocol
    if "assets" in value:
        metadata["assets"] = normalize_assets(value.get("assets"), f"{label}.assets")
    else:
        metadata["assets"] = default_release_assets(version)
    announcement = normalize_announcement(value.get("announcement"), label)
    if announcement is not None:
        metadata["announcement"] = announcement
    return metadata


def normalize_releases(value: Any) -> dict[str, dict[str, Any]]:
    if value is None:
        return {}
    if not isinstance(value, dict):
        raise ChangelogError("releases must be an object")

    releases: dict[str, dict[str, Any]] = {}
    for raw_version, raw_metadata in value.items():
        if not isinstance(raw_version, str) or not raw_version.strip():
            raise ChangelogError("releases contains an empty version key")
        version = normalize_version(raw_version)
        parse_version(version)
        releases[version] = normalize_release_metadata(raw_metadata, f"releases.{version}", version)

    return {
        version: releases[version]
        for version in sorted(releases, key=parse_version, reverse=True)
    }


def build_latest_json(
    version: str,
    notes: str,
    assets: dict[str, str],
    protocol: int | None = None,
    announcement: dict[str, str] | None = None,
    releases: dict[str, Any] | None = None,
) -> str:
    normalized_version = normalize_version(version)
    normalized_notes = notes.strip()
    if not normalized_notes:
        raise ChangelogError("release notes are empty")

    if protocol is None:
        protocol = read_protocol_version()

    ordered_assets = normalize_assets(assets, "assets")
    normalized_announcement = normalize_announcement(announcement, "root")
    archived_releases = normalize_releases(releases)
    current_metadata: dict[str, Any] = {
        "notes": normalized_notes,
        "protocol": protocol,
        "assets": ordered_assets,
    }
    if normalized_announcement is not None:
        current_metadata["announcement"] = normalized_announcement
    archived_releases[normalized_version] = current_metadata
    archived_releases = {
        release_version: archived_releases[release_version]
        for release_version in sorted(archived_releases, key=parse_version, reverse=True)
    }

    manifest: dict[str, Any] = {
        "version": normalized_version,
        "protocol": protocol,
        "notes": normalized_notes,
        "assets": ordered_assets,
    }
    if normalized_announcement is not None:
        manifest["announcement"] = normalized_announcement
    manifest["releases"] = archived_releases

    return json.dumps(manifest, indent=2) + "\n"


def default_release_assets(version: str, repo: str = DEFAULT_RELEASE_REPO) -> dict[str, str]:
    normalized_version = normalize_version(version)
    tag = f"v{normalized_version}"
    return {
        target: f"https://github.com/{repo}/releases/download/{tag}/{EXPECTED_ASSET_NAMES[target]}"
        for target in ASSET_TARGETS
    }


# ---------------------------------------------------------------------------
# Manifest ownership
# ---------------------------------------------------------------------------
#
# A release manifest is a promise: every URL in it is a binary this project
# published, and `flk update` downloads it without asking again. The repository
# shipped a manifest that broke that promise — 45 archived releases, 180 asset
# URLs, every one of them another project's release download — so the check that
# would have caught it is now part of writing the file rather than something a
# reader has to remember.


def foreign_release_entries(releases: Any, repo: str = DEFAULT_RELEASE_REPO) -> list[str]:
    """Archived versions whose assets are not this repository's release assets.

    Deliberately reads the raw manifest value instead of the normalized one:
    `normalize_release_metadata` manufactures this repository's URLs for an
    entry that has none, which would launder a foreign entry into looking local.
    An entry with no `assets` of its own is left alone — it already carries the
    local default wherever it is read.
    """
    if not isinstance(releases, dict):
        return []

    foreign: list[str] = []
    for raw_version, metadata in releases.items():
        if not isinstance(metadata, dict):
            continue
        assets = metadata.get("assets")
        if not isinstance(assets, dict):
            continue
        try:
            expected = default_release_assets(str(raw_version), repo)
        except ChangelogError:
            foreign.append(str(raw_version))
            continue
        if any(str(assets.get(target, "")).strip() != expected[target] for target in ASSET_TARGETS):
            foreign.append(str(raw_version))
    return sorted(foreign)


def ensure_manifest_assets_belong_to_repo(
    manifest: dict[str, Any], repo: str = DEFAULT_RELEASE_REPO, label: str = "manifest"
) -> None:
    """Refuse a manifest that advertises a version this repository did not publish."""
    version = manifest.get("version")
    if not isinstance(version, str) or not version.strip():
        raise ChangelogError(f"{label} is missing a string version")

    assets = manifest.get("assets")
    if not isinstance(assets, dict):
        raise ChangelogError(f"{label} is missing an assets object")

    expected = default_release_assets(version, repo)
    for target in ASSET_TARGETS:
        url = str(assets.get(target, "")).strip()
        if url != expected[target]:
            raise ChangelogError(
                f"{label} advertises {target} for v{normalize_version(version)} as {url or '<missing>'}, "
                f"which is not this repository's release asset ({expected[target]})"
            )

    foreign = foreign_release_entries(manifest.get("releases"), repo)
    if foreign:
        raise ChangelogError(
            f"{label} archives {len(foreign)} version(s) published by another repository: "
            f"{', '.join(foreign)}"
        )


def build_no_release_manifest(
    repo: str = DEFAULT_RELEASE_REPO, protocol: int | None = None
) -> str:
    """The manifest for a repository that has published no release yet."""
    return build_latest_json(
        NO_STABLE_RELEASE_VERSION,
        NO_STABLE_RELEASE_NOTES,
        default_release_assets(NO_STABLE_RELEASE_VERSION, repo),
        protocol=protocol,
        releases={},
    )


# ---------------------------------------------------------------------------
# Release plan (#509)
# ---------------------------------------------------------------------------


@dataclass
class ReleasePlan:
    """What landed since the last release, and what that calls for.

    A recommendation, never an instruction: the number stays a human's decision
    and `check-version` can be overridden. What did not exist before #509 is any
    of this — the recipe asked for a number and took whatever it was given.
    """

    rev_range: str
    since_label: str
    last_version: str | None
    is_first_release: bool
    commits: list[Commit] = field(default_factory=list)
    groups: dict[str, list[str]] = field(default_factory=dict)
    unclassified: list[str] = field(default_factory=list)
    breaking: list[str] = field(default_factory=list)
    bump: str | None = None
    recommended_version: str | None = None

    @property
    def commit_count(self) -> int:
        return len(self.commits)

    @property
    def bump_reason(self) -> str | None:
        if self.bump is None:
            return None
        if self.bump == "major":
            return "a BREAKING CHANGE is in the range"
        if self.bump == "minor":
            return "a feat is in the range"
        return "changes that are neither a feat nor a break are in the range"


def run_git(args: list[str], cwd: Path | None = None) -> str | None:
    """git's answer, or None when it has none — as opposed to a failure."""
    result = subprocess.run(
        ["git", *args], capture_output=True, text=True, check=False, cwd=cwd
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def latest_stable_tag(cwd: Path | None = None) -> str | None:
    """The most recent `v*` tag, or None when this repository has never released.

    `git describe` fails rather than answering when no tag matches, which is the
    state every repository is in before its first release. That is an answer,
    not an error: this project's own first release is in that state.
    """
    return run_git(["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"], cwd)


def root_commit(cwd: Path | None = None) -> str | None:
    return run_git(["rev-list", "--max-parents=0", "HEAD"], cwd)


def bump_version(version: str, bump: str) -> str:
    major, minor, patch = parse_version(version)
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ChangelogError(f"unknown bump: {bump}")


def resolve_release_base(
    since: str | None = None, cwd: Path | None = None
) -> tuple[str, str, str | None, bool]:
    """The commit range a release would cover, and the version before it.

    Returns `(rev_range, since_label, last_version, is_first_release)`. With no
    `v*` tag the range is the whole history and `is_first_release` is True, which
    is the defined answer to "since the last tag" for a repository that has never
    tagged a release — a state this repository is in.
    """
    if since:
        # Exclusive, like the `tag..HEAD` range below: "since" a commit means
        # after it, so the ref itself is not part of what the release would cover.
        return f"{since}..HEAD", f"since {since}", None, False

    tag = latest_stable_tag(cwd)
    if tag:
        return (
            f"{tag}..HEAD",
            f"since {tag}",
            normalize_version(tag),
            False,
        )

    first = root_commit(cwd)
    if first is None:
        raise ChangelogError(
            "could not read git history; run this inside the flock repository"
        )
    # `HEAD`, not the root commit: `git log <root>` is one commit, and
    # `<root>..HEAD` drops the root. The whole history is `HEAD`.
    return (
        "HEAD",
        f"the whole history — no v* tag yet, first commit {first[:12]}",
        None,
        True,
    )


def build_release_plan(since: str | None = None, cwd: Path | None = None) -> ReleasePlan:
    rev_range, since_label, last_version, is_first_release = resolve_release_base(since, cwd)
    try:
        commits = git_commits(rev_range, cwd)
    except subprocess.CalledProcessError as exc:
        # A ref that does not resolve is the caller's typo, not a stack trace.
        stderr = exc.stderr.strip() if isinstance(exc.stderr, str) else ""
        raise ChangelogError(
            f"could not read git log for {rev_range}: {stderr.splitlines()[0] if stderr else exc}"
        ) from exc
    plan = ReleasePlan(
        rev_range=rev_range,
        since_label=since_label,
        last_version=last_version,
        is_first_release=is_first_release,
        commits=commits,
    )

    for classified in classify_commits(commits):
        subject = classified.commit.subject
        if classified.kind is None:
            plan.unclassified.append(subject)
            continue
        plan.groups.setdefault(classified.kind, []).append(subject)
        if classified.breaking:
            plan.breaking.append(subject)

    plan.bump = recommended_bump(commits)
    if last_version is not None and plan.bump is not None:
        plan.recommended_version = bump_version(last_version, plan.bump)
    return plan


def ensure_version_is_releasable(plan: ReleasePlan, version: str, overrides: set[str]) -> None:
    """Refuse a version the plan says is wrong, unless the operator overrode it.

    Two separate questions, so two overrides. Holding a release back is a normal
    thing to want and only needs `--allow-below-recommended`; shipping a number
    that is not greater than the last release can only be the first release or a
    mistake, and needs `--allow-not-greater`.
    """
    requested = parse_version(version)
    normalized = normalize_version(version)

    if plan.last_version is not None and "allow-not-greater" not in overrides:
        if requested <= parse_version(plan.last_version):
            raise ChangelogError(
                f"v{normalized} is not greater than the last release v{plan.last_version}; "
                f"pass --allow-not-greater if this is deliberate"
            )

    if plan.recommended_version is not None and "allow-below-recommended" not in overrides:
        if requested < parse_version(plan.recommended_version):
            raise ChangelogError(
                f"v{normalized} is below the recommended v{plan.recommended_version} "
                f"({plan.bump}: {plan.bump_reason}); pass --allow-below-recommended to hold "
                f"the release back"
            )


def manifest_from_release_payload(
    payload: dict[str, Any], version: str, protocol: int | None = None
) -> dict[str, Any]:
    normalized_version = normalize_version(version)
    tag_name = str(payload.get("tagName") or "")
    if normalize_version(tag_name) != normalized_version:
        raise ChangelogError(
            f"GitHub release tag mismatch: expected v{normalized_version}, got {tag_name or '<missing>'}"
        )
    if payload.get("isDraft"):
        raise ChangelogError(f"GitHub release v{normalized_version} is still a draft")
    if payload.get("isPrerelease"):
        raise ChangelogError(f"GitHub release v{normalized_version} is a prerelease")

    notes = str(payload.get("body") or "").strip()
    if not notes:
        raise ChangelogError(f"GitHub release v{normalized_version} has empty release notes")

    assets_list = payload.get("assets")
    if not isinstance(assets_list, list):
        raise ChangelogError("GitHub release response is missing assets")

    release_assets: dict[str, Any] = {}
    for asset in assets_list:
        if isinstance(asset, dict):
            name = asset.get("name")
            if isinstance(name, str) and name not in release_assets:
                release_assets[name] = asset

    manifest_assets: dict[str, str] = {}
    for target, asset_name in EXPECTED_ASSET_NAMES.items():
        asset = release_assets.get(asset_name)
        if not isinstance(asset, dict):
            raise ChangelogError(f"GitHub release v{normalized_version} is missing asset {asset_name}")
        url = str(asset.get("url") or "").strip()
        if not url:
            raise ChangelogError(f"GitHub release asset {asset_name} is missing a download URL")
        manifest_assets[target] = url

    return {
        "version": normalized_version,
        "protocol": protocol if protocol is not None else read_protocol_version(),
        "notes": notes,
        "assets": manifest_assets,
    }


def canonicalize_manifest(manifest: dict[str, Any], label: str) -> dict[str, Any]:
    version = manifest.get("version")
    if not isinstance(version, str) or not version.strip():
        raise ChangelogError(f"{label} is missing a string version")

    notes = manifest.get("notes")
    if not isinstance(notes, str) or not notes.strip():
        raise ChangelogError(f"{label} is missing non-empty release notes")

    protocol = manifest.get("protocol")
    if not isinstance(protocol, int):
        raise ChangelogError(f"{label} is missing an integer protocol")

    assets = manifest.get("assets")
    if not isinstance(assets, dict):
        raise ChangelogError(f"{label} is missing an assets object")

    normalized_assets = normalize_assets(assets, f"{label} assets")

    return {
        "version": normalize_version(version),
        "protocol": protocol,
        "notes": notes.strip(),
        "assets": normalized_assets,
    }


def ensure_manifest_matches_expected(
    manifest: dict[str, Any],
    expected_manifest: dict[str, Any],
    label: str,
) -> dict[str, Any]:
    canonical_manifest = canonicalize_manifest(manifest, label)
    canonical_expected = canonicalize_manifest(expected_manifest, "expected release manifest")
    if canonical_manifest != canonical_expected:
        raise ChangelogError(
            f"{label} does not match the published GitHub release manifest for v{canonical_expected['version']}"
        )
    return canonical_manifest


def ensure_current_release_assets_are_mirrored(manifest: dict[str, Any], label: str) -> None:
    canonical = canonicalize_manifest(manifest, label)
    releases = normalize_releases(manifest.get("releases"))
    metadata = releases.get(canonical["version"])
    if metadata is None:
        raise ChangelogError(f"{label} is missing releases.{canonical['version']}")
    if metadata.get("assets") != canonical["assets"]:
        raise ChangelogError(
            f"{label} releases.{canonical['version']}.assets must match top-level assets"
        )


def load_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise ChangelogError(f"file not found: {path}") from exc


def load_json(path: Path) -> dict[str, Any]:
    try:
        content = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise ChangelogError(f"file not found: {path}") from exc

    try:
        data = json.loads(content)
    except json.JSONDecodeError as exc:
        raise ChangelogError(f"invalid JSON in {path}: {exc}") from exc

    if not isinstance(data, dict):
        raise ChangelogError(f"expected JSON object in {path}")
    return data


def archived_releases_from_current_manifest(manifest: dict[str, Any]) -> dict[str, dict[str, Any]]:
    releases = normalize_releases(manifest.get("releases"))
    version = manifest.get("version")
    notes = manifest.get("notes")
    if isinstance(version, str) and version.strip() and isinstance(notes, str) and notes.strip():
        normalized_version = normalize_version(version)
        metadata: dict[str, Any] = {"notes": notes.strip()}
        protocol = manifest.get("protocol")
        if isinstance(protocol, int):
            metadata["protocol"] = protocol
        assets = manifest.get("assets")
        if isinstance(assets, dict):
            metadata["assets"] = normalize_assets(assets, "current root assets")
        else:
            metadata["assets"] = default_release_assets(normalized_version)
        announcement = normalize_announcement(manifest.get("announcement"), "current root")
        if announcement is not None:
            metadata["announcement"] = announcement
        releases[normalized_version] = metadata

    return {
        release_version: releases[release_version]
        for release_version in sorted(releases, key=parse_version, reverse=True)
    }


def load_product_announcement(path: Path) -> dict[str, str] | None:
    try:
        content = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise ChangelogError(f"product announcement file not found: {path}") from exc

    try:
        data = json.loads(content)
    except json.JSONDecodeError as exc:
        raise ChangelogError(f"invalid JSON in {path}: {exc}") from exc

    if data is None:
        return None
    return normalize_announcement(data, f"announcement in {path}")


def write_text(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")


def fetch_release_payload(version: str, repo: str) -> dict[str, Any]:
    normalized_version = normalize_version(version)
    command = [
        "gh",
        "release",
        "view",
        f"v{normalized_version}",
        "--repo",
        repo,
        "--json",
        "tagName,isDraft,isPrerelease,body,assets",
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        stderr = result.stderr.strip() or result.stdout.strip() or "unknown gh error"
        raise ChangelogError(f"failed to read GitHub release v{normalized_version}: {stderr}")

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ChangelogError(f"invalid JSON from gh release view: {exc}") from exc

    if not isinstance(payload, dict):
        raise ChangelogError("unexpected GitHub release payload shape")
    return payload


def fetch_remote_json(url: str, label: str) -> dict[str, Any]:
    command = [
        "curl",
        "-fsSL",
        "--retry",
        "3",
        "--connect-timeout",
        "10",
        "--max-time",
        "20",
        url,
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        stderr = result.stderr.strip() or result.stdout.strip() or "unknown curl error"
        raise ChangelogError(f"failed to fetch {label}: {stderr}")

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ChangelogError(f"invalid JSON from {label}: {exc}") from exc

    if not isinstance(payload, dict):
        raise ChangelogError(f"expected JSON object from {label}")
    return payload


def verify_asset_urls_resolve(assets: dict[str, str], label: str) -> None:
    for target in ASSET_TARGETS:
        url = assets[target]
        command = [
            "curl",
            "-fsSIL",
            "--retry",
            "3",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            url,
        ]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            stderr = result.stderr.strip() or result.stdout.strip() or "unknown curl error"
            raise ChangelogError(f"failed to verify {label} asset {target}: {stderr}")


def ensure_manifest_is_outdated(current_manifest: dict[str, Any], version: str) -> None:
    current_version = current_manifest.get("version")
    if not isinstance(current_version, str):
        raise ChangelogError("website/latest.json is missing a string version")

    if parse_version(current_version) >= parse_version(version):
        raise ChangelogError(
            f"website/latest.json is already at v{normalize_version(current_version)}; expected something older than v{normalize_version(version)}"
        )


def git_status_lines(path: Path) -> list[str]:
    result = subprocess.run(
        ["git", "status", "--short", "--", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return []
    return [line for line in result.stdout.splitlines() if line.strip()]


def cmd_prepare(args: argparse.Namespace) -> int:
    path = Path(args.path)
    original = load_text(path)
    updated = prepare_release(original, normalize_version(args.version), args.date)
    write_text(path, updated)
    return 0


def cmd_extract(args: argparse.Namespace) -> int:
    path = Path(args.path)
    body = extract_section_body(load_text(path), normalize_version(args.version))
    if args.output:
        write_text(Path(args.output), body)
    else:
        sys.stdout.write(body)
    return 0


def cmd_sync_latest_json(args: argparse.Namespace) -> int:
    manifest_path = Path(args.output)
    version = normalize_version(args.version)

    current_manifest = load_json(manifest_path)
    ensure_manifest_is_outdated(current_manifest, version)

    release_payload = fetch_release_payload(version, args.repo)
    new_manifest = manifest_from_release_payload(release_payload, version, args.protocol)
    announcement_path = Path(args.announcement)
    announcement = load_product_announcement(announcement_path)

    # The archive is carried forward, not rebuilt: a release is additive to its
    # own history. That is only safe while the archive holds this repository's
    # releases, so anything else is dropped here, by name, on the way past —
    # otherwise the next release re-publishes it and the file never heals.
    archived = archived_releases_from_current_manifest(current_manifest)
    foreign = foreign_release_entries(archived, args.repo)
    for stale in foreign:
        del archived[stale]

    output = build_latest_json(
        version,
        str(new_manifest["notes"]),
        dict(new_manifest["assets"]),
        protocol=int(new_manifest["protocol"]),
        announcement=announcement,
        releases=archived,
    )
    write_text(manifest_path, output)
    if announcement is not None:
        write_text(announcement_path, "null\n")

    print(f"updated {manifest_path} from GitHub release v{version}")
    if foreign:
        print(
            f"dropped {len(foreign)} archived version(s) not published by {args.repo}: "
            f"{', '.join(foreign)}"
        )
    # The file is written before this runs, so a manifest that still advertises a
    # foreign repository is now on disk and has to be reported as the failure it
    # is rather than discovered by a user running `flk update`.
    ensure_manifest_assets_belong_to_repo(load_json(manifest_path), args.repo, str(manifest_path))
    print(f"every advertised asset is a {args.repo} release asset")
    if announcement is not None:
        print(f"included product announcement from {announcement_path}")
        print(f"cleared {announcement_path}")
    status_lines = git_status_lines(manifest_path)
    print("files changed:")
    if status_lines:
        for line in status_lines:
            print(f"  {line}")
    else:
        print(f"  (no git status output for {manifest_path})")

    print("next:")
    print(f"  git diff -- {manifest_path}")
    print(f"  git add {manifest_path}")
    print(f'  git commit -m "docs: update website manifest for v{version}"')
    print("  git push")
    return 0


def cmd_neutralize_latest_json(args: argparse.Namespace) -> int:
    manifest_path = Path(args.output)
    write_text(manifest_path, build_no_release_manifest(args.repo, args.protocol))
    ensure_manifest_assets_belong_to_repo(load_json(manifest_path), args.repo, str(manifest_path))
    print(
        f"wrote {manifest_path}: no installable version is advertised "
        f"(v{NO_STABLE_RELEASE_VERSION} sentinel, {args.repo} asset URLs, "
        f"protocol {args.protocol if args.protocol is not None else read_protocol_version()})"
    )
    return 0


def print_release_plan(plan: ReleasePlan) -> None:
    print("release plan")
    if plan.last_version is None:
        print("  last release:  none — this is the first release from this repository")
    else:
        print(f"  last release:  v{plan.last_version}")
    print(f"  commits since: {plan.commit_count} ({plan.since_label})")
    if plan.bump is None:
        print("  recommended:   nothing to release — no conventional commits in the range")
    else:
        print(f"  recommended:   {plan.bump} — {plan.bump_reason}")
    if plan.recommended_version is not None:
        print(f"  next version:  {plan.recommended_version}")
    elif plan.bump is not None:
        print(
            "  next version:  yours to choose — there is no v* tag to bump from, and the "
            "version line is a product decision (ADR-0025)"
        )

    for label, subjects in (
        ("breaking", plan.breaking),
        *[(kind, plan.groups.get(kind, [])) for kind in sorted(plan.groups)],
        ("unclassified", plan.unclassified),
    ):
        if not subjects:
            continue
        print()
        print(f"  {label} ({len(subjects)})")
        for subject in subjects:
            print(f"    {subject}")

    if plan.unclassified:
        print()
        print(
            "  unclassified subjects are not conventional commits; CI rejects them and "
            "they contribute no bump"
        )


def cmd_plan(args: argparse.Namespace) -> int:
    plan = build_release_plan(args.since, Path(args.cwd) if args.cwd else None)
    print_release_plan(plan)
    return 0


def cmd_check_version(args: argparse.Namespace) -> int:
    version = normalize_version(args.version)
    parse_version(version)  # format, before anything reads git
    overrides = set(args.override or ())
    plan = build_release_plan(args.since, Path(args.cwd) if args.cwd else None)

    if plan.last_version is None:
        print(
            f"v{version}: first release from this repository, so there is no earlier "
            f"version to be greater than ({plan.commit_count} commits in the range)"
        )
    else:
        print(f"v{version}: last release v{plan.last_version}, plan recommends {plan.bump}")

    ensure_version_is_releasable(plan, version, overrides)
    for override in sorted(overrides):
        print(f"override accepted: --{override}")
    print(f"v{version} is releasable")
    return 0


def cmd_validate_product_announcement(args: argparse.Namespace) -> int:
    announcement = load_product_announcement(Path(args.path))
    if announcement is None:
        print(f"product announcement ({args.path}): none")
    else:
        print(
            f"product announcement ({args.path}): {announcement['id']} - {announcement['title']}"
        )
    return 0


def cmd_verify_release_state(args: argparse.Namespace) -> int:
    version = normalize_version(args.version)
    release_payload = fetch_release_payload(version, args.repo)
    expected_manifest = manifest_from_release_payload(release_payload, version, args.protocol)

    local_raw_manifest = load_json(Path(args.output))
    ensure_manifest_assets_belong_to_repo(local_raw_manifest, args.repo, str(args.output))
    local_manifest = ensure_manifest_matches_expected(
        local_raw_manifest,
        expected_manifest,
        str(args.output),
    )
    ensure_current_release_assets_are_mirrored(local_raw_manifest, str(args.output))
    print(f"GitHub release v{version}: OK")
    print(f"local manifest ({args.output}): OK")

    live_raw_manifest = fetch_remote_json(args.live_url, args.live_url)
    live_manifest = ensure_manifest_matches_expected(
        live_raw_manifest,
        expected_manifest,
        args.live_url,
    )
    ensure_current_release_assets_are_mirrored(live_raw_manifest, args.live_url)
    print(f"live manifest ({args.live_url}): OK")

    verify_asset_urls_resolve(dict(expected_manifest["assets"]), "release")
    print("release asset URLs: OK")

    if local_manifest != live_manifest:
        raise ChangelogError("local and live manifests disagree after individual verification")
    print("local and live manifests agree: OK")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Prepare and extract changelog release notes")
    subparsers = parser.add_subparsers(dest="command", required=True)

    prepare = subparsers.add_parser("prepare", help="Move Unreleased into a versioned section")
    prepare.add_argument("--path", default="CHANGELOG.md")
    prepare.add_argument("--version", required=True)
    prepare.add_argument("--date", default=str(date.today()))
    prepare.set_defaults(func=cmd_prepare)

    extract = subparsers.add_parser("extract", help="Extract a version section body")
    extract.add_argument("--path", default="CHANGELOG.md")
    extract.add_argument("--version", required=True)
    extract.add_argument("--output")
    extract.set_defaults(func=cmd_extract)

    sync_latest_json = subparsers.add_parser(
        "sync-latest-json",
        help="Update website/latest.json from a published GitHub release",
    )
    sync_latest_json.add_argument("--version", required=True)
    sync_latest_json.add_argument("--repo", default=DEFAULT_RELEASE_REPO)
    sync_latest_json.add_argument("--output", default=str(DEFAULT_LATEST_JSON_PATH))
    sync_latest_json.add_argument("--announcement", default=str(DEFAULT_PRODUCT_ANNOUNCEMENT_PATH))
    sync_latest_json.add_argument("--protocol", type=int)
    sync_latest_json.set_defaults(func=cmd_sync_latest_json)

    neutralize_latest_json = subparsers.add_parser(
        "neutralize-latest-json",
        help="Write a manifest that advertises no installable version",
    )
    neutralize_latest_json.add_argument("--repo", default=DEFAULT_RELEASE_REPO)
    neutralize_latest_json.add_argument("--output", default=str(DEFAULT_LATEST_JSON_PATH))
    neutralize_latest_json.add_argument("--protocol", type=int)
    neutralize_latest_json.set_defaults(func=cmd_neutralize_latest_json)

    plan = subparsers.add_parser(
        "plan",
        help="Report commits since the last release and the bump they call for",
    )
    plan.add_argument("--since", help="Diff from this ref instead of the last v* tag")
    plan.add_argument("--cwd", help="Read git history from this directory")
    plan.set_defaults(func=cmd_plan)

    check_version = subparsers.add_parser(
        "check-version",
        help="Check a release version against the last release and the recommended bump",
    )
    check_version.add_argument("--version", required=True)
    check_version.add_argument("--since")
    check_version.add_argument("--cwd")
    check_version.add_argument(
        "--allow-not-greater",
        dest="override",
        action="append_const",
        const="allow-not-greater",
        help="Accept a version that is not greater than the last release",
    )
    check_version.add_argument(
        "--allow-below-recommended",
        dest="override",
        action="append_const",
        const="allow-below-recommended",
        help="Accept a version below the bump the commit range recommends",
    )
    check_version.set_defaults(func=cmd_check_version)

    validate_product_announcement = subparsers.add_parser(
        "validate-product-announcement",
        help="Validate docs/next product announcement JSON",
    )
    validate_product_announcement.add_argument(
        "--path", default=str(DEFAULT_PRODUCT_ANNOUNCEMENT_PATH)
    )
    validate_product_announcement.set_defaults(func=cmd_validate_product_announcement)

    verify_release_state = subparsers.add_parser(
        "verify-release-state",
        help="Verify GitHub release, local manifest, live manifest, and asset URLs all match",
    )
    verify_release_state.add_argument("--version", required=True)
    verify_release_state.add_argument("--repo", default=DEFAULT_RELEASE_REPO)
    verify_release_state.add_argument("--output", default=str(DEFAULT_LATEST_JSON_PATH))
    verify_release_state.add_argument("--live-url", default=DEFAULT_LIVE_MANIFEST_URL)
    verify_release_state.add_argument("--protocol", type=int)
    verify_release_state.set_defaults(func=cmd_verify_release_state)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    try:
        return args.func(args)
    except ChangelogError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
