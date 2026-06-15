---
number: 31
title: "feat(nix): bake fork build identity into the version string"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T19:59:06Z
closed: 2026-06-10T19:59:22Z
merged: 2026-06-10T19:59:22Z
base: feat/sidebar-row-gap
head: fork-build-identity
url: https://github.com/gerchowl/herdr/pull/31
---

# feat(nix): bake fork build identity into the version string

Fork release-identity (the lightweight "own versioning" decision): every nix build now self-identifies as **`0.6.8-fork.<shortRev>`** via upstream's existing `HERDR_BUILD_CHANNEL`/`HERDR_BUILD_ID` hooks (`build_info.rs` renders them; the package just never set them).

- `herdr status` distinguishes builds (today: two proto-13 fork builds both said "0.6.8")
- federation `peers summary` already carries `version` → the servers band shows which machine is behind, fleet-wide, for free
- the nix pin remains the release mechanism — no semver cadence, no release branches

Verified: `nix derivation show` carries the env; eval clean on both the package and overlay callsites.
