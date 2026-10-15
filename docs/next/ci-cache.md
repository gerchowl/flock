# GitHub Actions cache policy

The accepted fix is tracked in [#1474](https://github.com/gerchowl/flock/issues/1474).
The branch roles follow [ADR-0025, amendment](../adr/0025-version-identity-and-release-automation.md#amendment-2026-10-07-dev-integrates-main-holds-releases-versions-are-plain):
`dev` integrates changes, and `main` contains releases. Only `dev` populates
build caches. PRs, previews on other refs, and release tags can restore but
cannot save Rust caches. Release tags do not need their own persistent caches
because the next release has a different ref and cannot reuse them.

setup-zig's automatic build cache is disabled in every workflow. CI lint and
build instead restore the same `.zig-cache` directory through the SHA-pinned
`actions/cache/restore` action and save it through `actions/cache/save` only
from dev, after successful compilation. Keys include the compiler version,
OS, architecture, job, vendored Zig sources, and build script, with no run ID.
Prefix restoration can reuse previous generations, including the existing dev
setup-zig caches. An exact stable-key hit does not save again. The compiler
installation still uses setup-zig's separate stable download cache.

The choice is measured: latest dev Linux lint took 113 seconds with Zig cached.
The first PR run (38635993968) took 192 seconds with Zig disabled even though it
restored the exact same dev Rust key. Its lint step alone rose from 62 to 169
seconds. Linux build also rose from 201 to 279 seconds with an exact Rust hit. Retaining Zig reuse with stable keys avoids that cold-build cost while
removing per-run saves. setup-zig's `cache-key` input only adds a component and
cannot remove its run-ID suffix, hence the separate restore/save actions.
Artifact jobs continue without Zig build caching because they remove the local
cache before compiling. The preview check also leaves automatic caching off.

The Nix job restores an Actions-backed store cache after installing Nix.
The key covers the runner OS, flake lock, flake definition, and Nix expressions.
Source-only changes reuse dependencies while Nix still checks new derivations.
Only dev saves and purges older generations of the `nix-v1-<OS>-` prefix.
Exact key hits do not save again. The GC target is 2 GiB before compression.
Nix cannot collect live roots, so this is a collection target, not an absolute
archive size guarantee. Inspect save logs and repository usage if live roots
grow. Existing Zig and PR entries are not deleted retroactively. Those entries and
old Rust generations remain subject to GitHub's age/quota eviction.

Inputs were checked against the pinned
[cache-nix-action v7.0.2 README](https://github.com/nix-community/cache-nix-action/blob/7df957e333c1e5da7721f60227dbba6d06080569/README.md#inputs).
It requires Nix 2.24+ and SQLite 3.37+, supplied by the installer and runner.
Cache failures do not replace or bypass the actual build/check commands.

Closed-PR cleanup runs from the trusted base workflow via `pull_request_target`.
It checks out no code and grants only `actions: write`. The PR number is passed
through an environment variable to scope deletion to `refs/pull/<number>/merge`.
An empty cache set succeeds, while API/authentication failures fail the job.

## Verification

`python3 -m unittest scripts.test_actions_cache` checks every cache writer,
Zig opt-out and stable CI keys, Nix inputs, and cleanup permissions, and executes the cleanup
command against a fake CLI to verify scope and failure propagation. These tests
run in the existing `just script-tests` discovery suite.

Use `gh run view <id> --json jobs` to compare job and step durations, and inspect
run logs for cache hits. Match restored Rust keys to `refs/heads/dev` using
`gh cache list --ref refs/heads/dev`. PR runs cannot seed a new Nix cache because
saving is restricted to dev. Warm Nix timings and steady-state usage therefore
require post-merge dev runs. Use
`gh api repos/gerchowl/flock/actions/cache/usage` to inspect total bytes and count.
