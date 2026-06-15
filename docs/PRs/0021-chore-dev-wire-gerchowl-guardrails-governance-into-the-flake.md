---
number: 21
title: "chore(dev): wire gerchowl/guardrails governance into the flake devShell"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T05:30:10Z
closed: 2026-06-10T08:19:22Z
merged: 2026-06-10T08:19:22Z
base: feat/sidebar-row-gap
head: guardrails-devenv
url: https://github.com/gerchowl/herdr/pull/21
---

# chore(dev): wire gerchowl/guardrails governance into the flake devShell

## What

Wires **`gerchowl/guardrails`** (shareable code-quality / governance gates + toolbelt, as a Nix flake) into herdr's dev environment so the discipline is the same everywhere and survives agent drift.

## How

- **`flake.nix`** — adds `guardrails` as an input (`inputs.nixpkgs follows` ours) and builds `devShells.default` via `guardrails.lib.${system}.mkDevShell`. herdr's toolchain (`zig_0_15`, rust, cmake, ninja, pkg-config, just, nextest) rides in `extra`; `SDKROOT` comes from the darwin stdenv for free; the `libghostty-vt` build tuning (`LIBGHOSTTY_VT_OPTIMIZE`/`SIMD`) is restored in the shell hook.
- **`.envrc`** — direnv `use flake`, so the toolbelt + `prek install` self-wire on tree entry. Also clears any stale `core.hooksPath` left from the old `.githooks` setup.
- **`.pre-commit-config.yaml`** — the exposed governance surface, run by `prek`:
  - agent-drift gates `no-fake-impl` / `no-debug-leftovers` / `no-commented-code` → **warn-only** for now (via `scripts/guardrails-nudge`: report, never block). Promote one by pointing its `entry` at `guardrails-<name>`. Per-line escape: `guardrails-ok`.
  - **hard gates**: gitleaks, `rustfmt`, `clippy -D warnings`, `cargo-deny`.
  - **commit-msg**: conventional-commits, still calling `scripts/conventional_commits.py`.
- **`deny.toml`** — license allow-list (incl. herdr's own `AGPL-3.0-or-later`) + one audited advisory ignore (`RUSTSEC-2025-0141`, bincode-unmaintained, informational).
- **Replaces `.githooks`** (`just lint` pre-commit + commit-msg) with prek — `clippy`+`rustfmt` already cover `just lint`; `install-hooks` rewired to `prek install -t pre-commit -t commit-msg`.

## Rollout note

The three agent-drift gates are warn-only because the tree currently has ~15 files with `println!/dbg!` outside `main`/`bin`/`tests` (mostly legitimate CLI output in `src/cli/*`). They report on touch but don't block; a follow-up can promote them after a cleanup pass (or annotate the intentional CLI-output sites with `guardrails-ok`).

## Verification

- devShell composes: toolbelt + herdr toolchain (`cargo 1.95`, `zig 0.15.2`) + env (`SDKROOT` set) all present.
- `cargo deny check` → `advisories ok, bans ok, licenses ok, sources ok`.
- `prek run --all-files` → all green (gates Passed/warn-only; gitleaks, rustfmt, clippy, cargo-deny pass).
- conventional-commit hook rejects a non-conventional subject (exit 1) and accepts a valid one (exit 0).

## Worktree caveat

guardrails' auto-`prek install` shellHook guards on `[ -d .git ]`, which is false in linked worktrees (`.git` is a file there), so the hooks auto-wire in the main checkout but need a one-time `just install-hooks` inside worktrees (the commit-time bootstrap still re-enters the devShell regardless).
