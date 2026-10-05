#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

ALLOWED_TYPES = {
    "feat",
    "fix",
    "perf",
    "docs",
    "ci",
    "test",
    "refactor",
    "chore",
    "release",
}
# The `!` after the scope and the `BREAKING CHANGE:` footer are the two
# conventional-commit ways of saying "this breaks the contract", and both mean
# the same thing to a version bump.
SUBJECT_RE = re.compile(r"^(?P<kind>[a-z]+)(?:\([^)]+\))?(?P<breaking>!)?:\s+\S")
BREAKING_FOOTER_RE = re.compile(r"(?m)^BREAKING[ -]CHANGE:\s")

# `git log` field and record separators. Both are control characters git's
# `%x` emits and neither can appear in a commit message produced by an editor,
# so a message body spanning many lines stays one record.
LOG_FIELD_SEPARATOR = "\x1f"
LOG_RECORD_SEPARATOR = "\x1e"
LOG_FORMAT = LOG_FIELD_SEPARATOR.join(["%H", "%s", "%b"]) + LOG_RECORD_SEPARATOR

BUMP_ORDER = ("major", "minor", "patch")
# `feat` is the only type that promises a feature; everything else that ships in
# the binary is a patch. `release:` commits are exempt in `recommended_bump`,
# because they are the release rather than part of what it covers.
TYPE_BUMPS = {"feat": "minor"}
DEFAULT_BUMP = "patch"


@dataclass(frozen=True)
class Commit:
    sha: str
    subject: str
    body: str


@dataclass(frozen=True)
class ClassifiedCommit:
    commit: Commit
    kind: str | None
    breaking: bool


def git_subjects(rev_range: str) -> list[str]:
    output = subprocess.check_output(
        ["git", "log", "--pretty=format:%s", rev_range], text=True
    ).strip()
    return [line.strip() for line in output.splitlines() if line.strip()]


def git_commits(rev_range: str, cwd: Path | None = None) -> list[Commit]:
    """Commits in `rev_range`, oldest first, with their bodies.

    Bodies are read because a `BREAKING CHANGE:` footer is only visible in the
    body — a bump recommendation that missed it would be a major change shipped
    as a minor one.
    """
    output = subprocess.check_output(
        # `--reverse` because a release reads forwards: `git log` hands them back
        # newest first, which would print the plan's groups in reverse order.
        # stderr is captured rather than inherited so that git's own explanation of
        # a bad range reaches the caller as a message instead of the terminal.
        ["git", "log", "--reverse", f"--pretty=format:{LOG_FORMAT}", rev_range],
        text=True,
        cwd=cwd,
        stderr=subprocess.PIPE,
    )

    commits: list[Commit] = []
    for record in output.split(LOG_RECORD_SEPARATOR):
        record = record.strip("\n")
        if not record.strip():
            continue
        fields = record.split(LOG_FIELD_SEPARATOR)
        if len(fields) != 3:
            raise ValueError(f"unexpected git log record: {record!r}")
        sha, subject, body = fields
        commits.append(Commit(sha=sha.strip(), subject=subject, body=body))
    return commits


def valid_subject(subject: str) -> bool:
    match = SUBJECT_RE.match(subject)
    return bool(match and match.group("kind") in ALLOWED_TYPES)


def classify_commit(subject: str, body: str = "") -> tuple[str | None, bool]:
    """The conventional type of a subject, and whether it breaks the contract.

    Either half is `None`/`False` for a subject this repository would reject:
    CI refuses a non-conventional subject, so a plan that refused to read the
    history would report nothing at all on a branch that has not been through
    CI yet. The commits are still listed; they just land in the unclassified
    bucket and contribute no bump.
    """
    match = SUBJECT_RE.match(subject)
    if match is None:
        return None, False
    kind = match.group("kind")
    if kind not in ALLOWED_TYPES:
        return None, False
    breaking = bool(match.group("breaking")) or bool(BREAKING_FOOTER_RE.search(body))
    return kind, breaking


def classify_commits(commits: list[Commit]) -> list[ClassifiedCommit]:
    classified: list[ClassifiedCommit] = []
    for commit in commits:
        kind, breaking = classify_commit(commit.subject, commit.body)
        classified.append(ClassifiedCommit(commit=commit, kind=kind, breaking=breaking))
    return classified


def highest_bump(bumps: list[str]) -> str | None:
    ranked = [bump for bump in BUMP_ORDER if bump in bumps]
    return ranked[0] if ranked else None


def recommended_bump(commits: list[Commit]) -> str | None:
    """The bump the commits in a range call for, or None when there are none.

    The strongest signal wins, in the order `BREAKING CHANGE` / `!` > `feat` >
    everything else, which is conventional-commits' own rule rather than a
    flock one. A `release:` commit contributes nothing: it is the release.
    """
    bumps: list[str] = []
    for classified in classify_commits(commits):
        if classified.kind is None or classified.kind == "release":
            continue
        if classified.breaking:
            bumps.append("major")
        else:
            bumps.append(TYPE_BUMPS.get(classified.kind, DEFAULT_BUMP))
    return highest_bump(bumps)


def commit_message_subject(path: Path) -> str | None:
    for line in path.read_text(encoding="utf-8").splitlines():
        subject = line.strip()
        if subject and not subject.startswith("#"):
            return subject
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate conventional commit subjects")
    parser.add_argument("subjects", nargs="*")
    parser.add_argument("--range", dest="rev_range")
    parser.add_argument("--message-file")
    args = parser.parse_args()

    subjects = list(args.subjects)
    if args.rev_range:
        subjects.extend(git_subjects(args.rev_range))
    if args.message_file:
        subject = commit_message_subject(Path(args.message_file))
        if subject:
            subjects.append(subject)

    invalid = [subject for subject in subjects if not valid_subject(subject)]
    if invalid:
        print("invalid commit subject(s):")
        for subject in invalid:
            print(f"  {subject}")
        print(
            "commit subjects must use conventional commits because preview notes are generated from them."
        )
        print("example: fix(update): install selected channel")
        print("expected: type(optional-scope): subject")
        print("allowed types: " + ", ".join(sorted(ALLOWED_TYPES)))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())