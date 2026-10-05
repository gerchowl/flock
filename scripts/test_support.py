"""Shared fixtures for the maintenance script tests.

A fixture repository rather than this one: the release plan reads git history, so
a test that read the real repository's tags would assert on whatever tags the
checkout happens to have. Everything here is created in a temporary directory
with an empty git configuration, so nothing depends on the developer's own
identity, hooks, signing keys, or `init.defaultBranch`.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

# `.invalid` is reserved by RFC 2606 and can never resolve, which is the point:
# a fixture identity must not be a name that could belong to anyone.
FIXTURE_NAME = "Flock Script Tests"
FIXTURE_EMAIL = "flock-script-tests@example.invalid"

GIT_ENV = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_SYSTEM": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
    "HOME": "/nonexistent",
    "XDG_CONFIG_HOME": "/nonexistent",
}


def run_git(repo: Path, *args: str) -> str:
    # stderr is captured rather than inherited so that a test which asserts a git
    # refusal does not print the refusal into `just check`'s output; it rides on
    # the exception instead, for the test that wants to read it.
    result = subprocess.run(
        ["git", *args],
        cwd=repo,
        env=GIT_ENV,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise subprocess.CalledProcessError(
            result.returncode, ["git", *args], result.stdout, result.stderr
        )
    return result.stdout.strip()


def make_repo(root: Path) -> Path:
    """An initialized repository with one empty commit."""
    repo = root / "repo"
    repo.mkdir(parents=True)
    run_git(repo.parent, "init", "--quiet", "--initial-branch=main", str(repo))
    # Set on the repository rather than per-invocation so `git commit` in a test
    # reads like a normal commit.
    run_git(repo, "config", "user.name", FIXTURE_NAME)
    run_git(repo, "config", "user.email", FIXTURE_EMAIL)
    run_git(repo, "config", "commit.gpgsign", "false")
    commit(repo, "chore: first commit")
    return repo


def commit(repo: Path, subject: str, body: str = "") -> str:
    message = subject if not body else f"{subject}\n\n{body}\n"
    run_git(repo, "commit", "--quiet", "--allow-empty", "--message", message)
    return run_git(repo, "rev-parse", "HEAD")


def tag(repo: Path, name: str) -> str:
    run_git(repo, "tag", name)
    return name