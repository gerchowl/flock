# Contributing to flock

Thanks for wanting to contribute.

Flock came from my own need for a fast, simple, effective workspace manager for coding agents. I care a lot about how it looks, feels, and works, so many design and technical decisions here are deliberate.

This guide exists so I can keep flock manageable as a solo project and keep it from drifting from what it is supposed to be.

## The One Rule

**You must understand your code.** If you cannot explain what your changes do, how they behave at the edges, and how they fit flock's existing design, your PR will be closed.

Using AI to write code is fine. Submitting code you do not understand is not.

## Flock is opinionated

Flock has a specific direction for how it should look, feel, and work.

That includes interaction patterns, layout behavior, mouse ergonomics, terminology, and how features fit the product as a whole.

If your idea changes or contradicts that direction, do not start with a PR. Start with a discussion.

If you have a suggestion, disagreement, feature request, or product-direction question, start a GitHub Discussion instead of an issue or PR.

## Issues and discussions

The issue tracker is the maintainer work queue.

Issues are only for reproducible bug reports and maintainer-created or maintainer-converted work items. If an issue is open, it should be real, scoped, and actionable.

Use GitHub Discussions for feature requests, ideas, questions, contribution proposals, design discussion, behavior changes, and product-direction checks.

Discussions are community input. Upvotes and comments help show demand, but they do not guarantee implementation, priority, maintainer attention, or PR approval. A maintainer may ignore a discussion, reject it, implement it directly, ask for more detail, or convert it into an accepted issue.

Issues that do not use the bug report template may be closed automatically.

## First-time contributors

We use an approval gate for new contributors.

Before opening your first PR, get maintainer approval on an accepted issue. If you want to propose new work, open a discussion describing what you want to change and why. If the work is accepted, a maintainer may convert the discussion into an issue or create a new issue for it.

If an accepted issue already exists, comment on that issue before starting work. A maintainer will comment `/approve @your-github-username` on the issue if your PR path is approved. That adds you to `.github/APPROVED_CONTRIBUTORS`.

Keep it short. Write in your own voice. A discussion, upvote, branch, or proposed implementation does not reserve the work and does not mean the PR path is approved.

This exists because AI makes it trivial to generate plausible-looking contributions that do not fit the app.

Agent note: if you are an AI agent preparing an issue or PR for this repository, check whether the human's GitHub username is listed in `.github/APPROVED_CONTRIBUTORS`. If they are not listed, warn them that opening a PR will be closed automatically. For feature requests, ideas, questions, and contribution proposals, guide them to GitHub Discussions. For bugs, use the bug report issue template and include a real reproduction. Do not bypass the issue template with the GitHub CLI or API. Keep the scope small, preserve flock's product direction, run the documented checks, and make sure the human can explain the change.

## What to put in a bug report

Bug reports should answer these questions clearly:

- what is the current behavior
- what is the expected behavior
- what is the shortest reproduction
- how does this affect you
- what Flock version, OS, and terminal are affected
- what shell and config are relevant, if any

If there is no reproduction yet, start a discussion instead.

If your proposal changes the visual language, interaction model, workflow, persistence, architecture, or product direction, start a discussion instead.

## Nix development environment

Use cargo-nextest 0.9.145 or newer. Earlier runners can inherit a sibling
test's capture pipe on macOS and falsely report leaked handles
([upstream fix](https://github.com/nextest-rs/nextest/pull/3553)). The repository
rejects older runners; the devShell provides 0.9.146. Leak detection and its
timeout stay enabled.

Run `nix develop` for the contributor toolchain. The devShell wraps sccache
with a persistent `$HOME/.cache/flock-sccache/tmp` directory and a separate
`$HOME/.cache/flock-sccache/flock.sock` server socket, shared across worktrees
for the same user. This implements the stable-directory/separate-endpoint design
accepted in [issue #1325](https://github.com/gerchowl/flock/issues/1325).

The pinned sccache 0.18.0 carries the startup fix approved in
[issue #1411](https://github.com/gerchowl/flock/issues/1411). Unix socket startup
takes an exclusive lock on `<socket>.lock`, probes for a live listener, and only
reclaims an owned stale socket after connection refusal or absence. A concurrent
starter finding a live listener succeeds without replacing it. Compilation does
not hold this lock, so compiler parallelism and the endpoint stay unchanged.
The lock file must be an owner-only regular file and persists across restarts
and crashes. Do not delete it while starters can run. Unsafe paths, permissions,
and inconclusive probes fail visibly without removing the endpoint. TCP and
abstract Unix sockets retain upstream behavior. A version assertion requires
re-evaluating the patch whenever the pinned sccache changes; drop the patch when
an upstream release fixes the unlink+bind race reported in
[mozilla/sccache#2906](https://github.com/mozilla/sccache/issues/2906).

Each dev shell picks up the patched binary on its next `nix develop`. A running
shared server keeps its old binary until it next restarts naturally. Nobody
restarts it deliberately for this rollout.

Artifact storage keeps sccache's existing configuration or platform default, including
`~/Library/Caches/Mozilla.sccache` on macOS, so existing caches remain warm.
The wrapper preserves an explicit `SCCACHE_DIR` and uses its `tmp` subdirectory
and `flock.sock` for daemon runtime state instead of the default runtime root.
[sccache 0.18 supports Unix sockets and explicit ports](https://github.com/mozilla/sccache/blob/v0.18.0/README.md#usage),
while [`SCCACHE_DIR` selects artifact storage](https://github.com/mozilla/sccache/blob/v0.18.0/docs/Configuration.md#disk-local).
There is no server-only temporary-directory setting, so the wrapper sets
`TMPDIR` for each sccache invocation, including compiler subprocesses. Other
shell commands retain their own `TMPDIR`. Persistent runtime storage avoids
scratch cleanup breaking later builds, at the cost of retaining daemon temporary
files. The separate socket avoids reusing a daemon on port 4226 that inherited
a disposable directory.

Explicit `SCCACHE_SERVER_UDS` or `SCCACHE_SERVER_PORT` overrides the default
socket. Ensure that endpoint's daemon was also started through the wrapper.
Keep runtime paths absolute, persistent, and writable, and do not delete them
while their daemon runs. Unix socket path limits apply (roughly 104 bytes on
macOS), so choose a shorter persistent `SCCACHE_DIR` or an explicit private
port for a long home path. An unset HOME requires an explicit `SCCACHE_DIR`.
Directory-creation failure prints the path and fails the build.
`RUSTC_WRAPPER=""` disables Rust caching, and custom wrappers remain intact.

Run `nix develop --command just test-sccache` to check deleted startup temp
directories, 20 rounds of four concurrent automatic starts, live-listener
preservation, stale sockets left by a killed private server, unsafe paths,
explicit and lazy restarts, and hits from a platform-default cache
warmed by an unwrapped daemon. The probe always supplies and asserts private
endpoints. Outside the devShell it exits before contacting any server with
`run inside nix develop (the wrapper is not active)`. The endpoint guard also
remains active under `python3 -O`. The probe isolates HOME and
XDG paths, so it exercises platform default cache selection without accessing
or stopping the host's shared cache server. Nix CI executes this check on Linux
and evaluates Darwin checks without building them. The Darwin probe can be
built locally with `nix build .#checks.aarch64-darwin.sccache --no-link`.

## Documentation for unreleased changes

The root `README.md`, root `CHANGELOG.md`, and website docs describe the latest released version of flock. Do not update root `README.md`, root `CHANGELOG.md`, or `website/src/content/docs/` for normal PRs.

If your PR changes user-facing behavior, mention the needed public-doc update in the PR. Update `docs/next/README.md` only when the root README needs to change for the next release. Update the full website-doc mirror under `docs/next/website/src/content/docs/` when website docs need to change for the next release.

For a user-facing PR, add `docs/next/changes/<issue-or-pr>.<kind>.md` with Markdown bullets and no headings. Use added, changed, deprecated, removed, fixed, security, or maintenance for the kind. Release preparation combines these fragments with existing Unreleased entries and deletes the consumed fragments.

If you are unsure whether docs are needed, mention it in the PR.

## Before submitting a PR

Install the repo hook once in your clone.

```bash
just install-hooks
```

The pre-commit hook runs `cargo fmt --check` before every commit.

Run the PR checks and make sure they pass.

```bash
just ci
```

`just ci` runs `cargo fmt --check` and `cargo nextest run`.

CI runs the full Linux tests plus macOS build and lint on ordinary PRs. Landing
batch PRs (`land/*`) and pushes to `dev`/`main` also run the full macOS suite.
Required aggregate checks remain active and fail on cancelled or missing legs.
A macOS-only test bug can therefore surface at batch time rather than on the
individual PR. See the [CI platform policy](docs/next/ci-platform-policy.md).

Do not open a PR that bypasses failing tests, formatting, or build errors.

## Test waits and flake policy

Follow [ADR-0037 §1](docs/adr/0037-test-wait-liveness-and-hang-ceilings.md#1-decision):
wait for the actual condition and fail when its producer dies, emits unexpected
output, or stays silent. Check liveness, reject unexpected stage transitions,
and use failure-only bounded receives. Do not fix flakes with sleeps, retries,
widened windows, or loosened thresholds. Examples:
[#1314](https://github.com/gerchowl/flock/pull/1314),
[#1334](https://github.com/gerchowl/flock/pull/1334),
[#1342](https://github.com/gerchowl/flock/pull/1342), and
[#1349](https://github.com/gerchowl/flock/pull/1349).

The [#1317](https://github.com/gerchowl/flock/issues/1317) nextest ceiling is a
backstop that names hung tests. It never makes a slow test pass. Both default
and CI profiles cover every binary: 420 seconds normally, 660 for mesh_delivery,
and 180 for the readiness binaries. Keep `terminate-after` in every timeout
override, including overrides that only change the slow-warning period.

Sizing uses at least 10× each binary's larger platform p99 from roughly 20 recent
dev CI runs, never below 180 seconds. See the [measurements and reproduction
command](docs/next/nextest-timings.md) and [ADR-0037 §2](docs/adr/0037-test-wait-liveness-and-hang-ceilings.md#2-sizing-and-limits).
Before resizing, investigate the producer and condition, recollect with
`scripts/nextest_timings.py`, and commit the evidence and regression checks.
Run `nix develop --command python3 scripts/probe_nextest_ceiling.py` to inject a
live silent producer and verify named termination in both profiles (about seven
minutes). Partial runs and unobserved binaries must remain explicit. A new binary inherits
the default ceiling and needs CI duration measurements before choosing an exception.

## Issue references in commits

If your PR relates to a GitHub issue, reference it in the commit body with `refs #<issue-number>`.

Example:

```text
fix: handle pane focus

refs #128
```

Do not use GitHub closing keywords like `fixes #128`, `closes #128`, or `resolves #128` in normal PR commits.

`Refs #N` is for the issue being fixed. When that commit lands on `dev`, automation
adds `pending-release` and closes the issue with an unreleased notice. Release
automation later records the published version.

For a tracking issue or epic, write `Part of #N`, never `Refs #N`. Both automation
paths exclude issues with a `tracking` or `epic` label, or a title ending in
`(tracking)`, case-insensitively. The shared rule lives in
[`scripts/release_issue_excluded.jq`](scripts/release_issue_excluded.jq).
Existing labels and closed state on trackers are left untouched, so this does
not reopen trackers closed by older automation. If evaluating the exclusion
rule fails, the step fails before editing, commenting on, or closing that issue.

## PR scope

Small bug fixes for accepted issues that clearly match the existing design are good candidates for PRs after approval.

Bigger changes to UI, behavior, interaction patterns, persistence, or architecture need discussion and maintainer approval first.

If a PR introduces a feature without prior alignment, or changes flock's feel without discussion, it will likely be closed.

## Questions?

Open a GitHub Discussion.

---

clank'd from [pi](https://github.com/badlogic/pi-mono/)
