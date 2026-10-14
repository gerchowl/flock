# GitHub Actions cache policy

The accepted fix is tracked in [#1474](https://github.com/gerchowl/flock/issues/1474).
The branch roles follow [ADR-0025, amendment](../adr/0025-version-identity-and-release-automation.md#amendment-2026-10-07-dev-integrates-main-holds-releases-versions-are-plain):
`dev` integrates changes, and `main` contains releases. Only `dev` populates
build caches. PRs, previews on other refs, and release tags can restore but
cannot save Rust caches. Release tags do not need their own persistent caches
because the next release has a different ref and cannot reuse them.

Zig build caches are disabled in CI, preview, release, and manual artifact
builds. The compiler installation still uses setup-zig's tool download path.
A per-run suffix does not prevent prefix restoration, but continually saving
new global/local Zig caches consumes the shared quota. Its `cache-key` input
adds a component but does not remove the run-ID suffix. Artifact jobs explicitly
remove their local Zig caches before building already.

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
Zig opt-out, Nix inputs, and cleanup permissions, and executes the cleanup
command against a fake CLI to verify scope and failure propagation. These tests
run in the existing `just script-tests` discovery suite.

Use `gh run view <id> --json jobs` to compare job and step durations, and inspect
run logs for cache hits. Match restored Rust keys to `refs/heads/dev` using
`gh cache list --ref refs/heads/dev`. PR runs cannot seed a new Nix cache because
saving is restricted to dev. Warm Nix timings and steady-state usage therefore
require post-merge dev runs. Use
`gh api repos/gerchowl/flock/actions/cache/usage` to inspect total bytes and count.
