# ADR 0025 — Version identity: this project restarts its version line, and the deterministic `just release` path stays

- Status: Proposed
- Date: 2026-10-05
- Issues: #506 (the user-visible defect), #507 (this record), #509 (the plan command)
- Decision owner: operator.

**This ADR is a proposal.** Version identity is the operator's decision and
nothing below has been acted on beyond the manifest neutralization in §3, which
is #506's bug fix and does not depend on this being accepted.

## Context

Three facts, none of which is a matter of taste.

**The repository is standalone.** `gh api repos/gerchowl/flock` reports
`fork: false`, with no `parent` and no `source`. There is no upstream to track,
no way to compare a version against another project's, and nothing left to
encode in the repository about where this tree came from.

**The release data still described the upstream project.** `website/latest.json`
carried **45 archived releases and 180 asset URLs**, every one of them
`github.com/ogulcancelik/herdr`, at `protocol: 12`, while this tree speaks
`PROTOCOL_VERSION = 25` (`src/protocol/wire.rs`). A stable-channel `flk update`
fetches that manifest and installs the URL in it, and `src/update.rs` asks no
question of the URL. So the channel did not merely advertise a stale version; it
would have downloaded another project's binary and then compared its protocol 12
against its own 25 to decide between live handoff and stopping the old server.

**This project has never released.** The only tag in the repository is
`preview-2026-06-18-c7630b57fa40`. There is no `v*` tag, so `0.6.8` in
`Cargo.toml` is not this project's version at all — it is the number upstream
happens to be at, inherited with the tree.

## Decision (proposed)

### 1. The version line restarts at `1.0.0`

Not `0.6.9`, and not `0.6.8-fork.1`.

`0.6.9` is a version string that upstream is entitled to publish next. A user
who sees `0.6.9` cannot tell from the string which product it names, which is
the whole thing that has to be true of a version number.

`0.6.8-fork.1` is better as a *runtime* marker than as a *manifest* version, and
the difference is not a preference — it is arithmetic and parsing:

- `Version::parse` (`src/update.rs:74`) splits on `.` and requires exactly three
  numeric parts. `0.6.8-fork.1` splits into four, one of them `8-fork`, and does
  not parse: `flk update` would fail with `invalid version in update manifest`,
  and `checked_in_website_manifest_matches_update_schema` would panic on
  `Version::parse(...).unwrap()`. Putting a marker in the manifest's version
  requires changing the updater first.
- SemVer orders `0.6.8-fork.1` **below** `0.6.8`, so every build made from this
  tree — whose `CARGO_PKG_VERSION` is `0.6.8` — would consider itself newer than
  the first release and would never offer it as an update.

The marker is not lost by dropping it from the version. `src/build_info.rs`
already reports `0.6.8-fork.e3f3f6f` at runtime, and that is where a user
identifies a build they are looking at. What the marker cannot do is name a
*release*, because the release's identity has to survive being copied into a
download URL, a Homebrew formula, a Nix pin and a bug report.

`1.0.0` is a string that cannot belong to the 0.6.x line, so a version in the
wild is unambiguous. Its cost is that it reads as a maturity claim. That claim
is defensible on its own terms — the tree is past the rename, past the protocol
reaches 25, past 1300 commits, and this is the release that makes it installable
— but it is the operator's call and not a fact this ADR can establish.

**The first release bumps `Cargo.toml` from `0.6.8`.** `just release-prepare`
writes the version it is given, so the number lands as a normal release step;
nothing else in the tree has to change.

### 2. Keep the deterministic `just release` path; do not adopt release-plz or cargo-release

Recorded with reasons so that it is not re-proposed without new evidence:

1. **Two channels, not one.** Stable and preview have separate manifests, and
   preview has a scheduled workflow. release-plz models neither; adopting it
   means losing the preview channel or running two competing release paths.
2. **Two changelogs.** Release notes come from a curated `CHANGELOG.md` staged
   into `docs/next/`, gated by `just release-docs-check`. A generated changelog
   fights the curated one.
3. **The gated full check.** `release-prepare` runs `just check` before it will
   produce a commit. That property is worth more than the automation, and it is
   not release-plz's model.
4. **The version line is a product decision** — §1 — and a tool that derives the
   number cannot be told "start again at 1.0.0" in a way that survives a
   re-proposal.

What is worth stealing is the one thing this ADR takes from it: computing the
bump rather than asserting it (§4).

### 3. A manifest may only advertise binaries this repository published

The release tooling now refuses, by name, to write or verify a manifest whose
assets are not `DEFAULT_RELEASE_REPO`'s release download URLs — for the current
version and for every archived entry. `sync-latest-json` additionally **drops**
foreign archived entries on the way past rather than carrying them forward, and
prints what it dropped. Carrying the archive forward is correct for a project's
own history and is exactly how another project's history got in; it is not the
behaviour to keep while the file can hold someone else's.

Until the first release is cut, the stable manifest is a sentinel: version
`0.0.0`, this repository's asset URL shape, and a note saying no release exists.
`0.0.0` is not a version-line decision — it is below every build of this binary,
so `flk update` answers "already up to date" and remote bootstrap answers "the
release manifest does not include flock `<your version>`". Both are loud and
neither downloads anything. The sentinel is replaced by the first release
whatever number §1 settles on.

The runtime half of #506 — `src/update.rs` refusing a manifest whose assets are
not this project's — is **not** in this ADR and not in this change. It is a
product-behaviour change in `src/`, and it is still open.

### 4. The bump is derived and checked; the number stays a human's

`just release-plan` reports the commits since the last `v*` tag grouped by
conventional type and recommends a bump: a `!` or `BREAKING CHANGE` footer →
major, a `feat` → minor, anything else that ships → patch. With no `v*` tag the
range is the whole history and the answer is "first release; the version line is
yours" rather than a failure.

`just release-prepare <version>` refuses a version that is not greater than the
last release, or that is below the recommended bump. Both checks are
overridable — `--allow-not-greater`, `--allow-below-recommended` — because
holding a release back is a normal thing for a human to want, and a tool that
cannot be argued with gets `--no-verify`'d and then stops checking anything.

### 5. The crate is not published to crates.io

`Cargo.toml` names the package `flock-ai` because the plain `flock` name is
taken, and carries no `publish` metadata. Publishing is out of scope for this
record and is **not** decided here: if it is ever wanted it needs its own
decision about the name, the `publish` metadata, and what a `cargo install` of
this tree is supposed to mean for a product that ships as a tarball, a Homebrew
formula and a Nix flake.

## What this record does not do

- It does not set the version. `Cargo.toml` still says `0.6.8` and the first
  release bumps it.
- It does not cut a release, and it does not tag. Cutting the first release is
  the operator's, and it needs the docs review `just release-docs-check` already
  enforces — root `README.md` and `CHANGELOG.md` currently differ from
  `docs/next/`.
- It does not change the two-channel design, or the preview workflow's schedule.
- It does not add the runtime asset-origin check to `src/update.rs`.

Refs #506, #507, #509.