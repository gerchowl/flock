---
number: 35
title: "chore(dev): adopt gerchowl/guardrails — flake input, hard gates, tracing facade"
kind: pr
state: OPEN
author: gerchowl
labels: []
created: 2026-06-10T21:15:03Z
closed: 
merged: 
base: master
head: chore/adopt-guardrails
url: https://github.com/gerchowl/herdr/pull/35
---

# chore(dev): adopt gerchowl/guardrails — flake input, hard gates, tracing facade

## Summary

Adopts [gerchowl/guardrails](https://github.com/gerchowl/guardrails) as the source of truth for herdr's code-quality governance, replacing the partial, fragmented setup that had landed on `guardrails-devenv` and `gate-promotions`.

- **`flake.nix`** — adds `guardrails` as a flake input, wires the toolbelt into the devShell, and auto-installs the prek hook via `shellHook`.
- **`.pre-commit-config.yaml`** — curated gate set. All three agent-drift gates run as **hard gates** (no nudges):
  - `no-fake-impl` (`todo!`/`unimplemented!`/`FIXME`)
  - `no-debug-leftovers` (`dbg!`/`print!`/`eprint!`)
  - `no-commented-code`
  Plus `gitleaks`, `rustfmt`, `clippy -D warnings`, `cargo-deny`. Per-gate `exclude` patterns for `vendor/`, Kitty-protocol "placeholder" terminology, CLI/UX output surfaces (`cli/`, `update.rs`, `remote.rs`, `server/headless.rs`, `client/mod.rs`, `integration/mod.rs`), and build/test scaffolding.
- **`deny.toml`** — cargo-deny config for herdr's dep graph. Permissive license allow-list covering the current tree; `RUSTSEC-2025-0141` (yanked bincode 2.0.1, informational) explicitly ignored with rationale.
- **`src/logging.rs`** — extends the tracing facade with hot-path latency instrumentation: `Stopwatch` + per-subsystem slow-path threshold helpers (`render_frame_observed`, `loop_iter_observed`, `*_observed`). Below threshold logs at `TRACE`; above logs at `WARN` with context (`pane_count`, `full_redraw`, `cause`, etc.). Steady state is silent.
- **`src/**`, `tests/**`** — scrub debug-print leftovers and route through the tracing facade so `no-debug-leftovers` passes.

## Supersedes

This PR is the consolidated, finished state of the guardrails adoption. After merge, the guardrails-only commits on these branches become redundant:

- `origin/guardrails-devenv` — the 3 chore commits at the tip (`70cc6c3`, `83d878a`, `b98eb7e`). Branch's remaining content is merged feature work that lands via individual feature-branch PRs.
- `origin/gate-promotions` — the `6a659da` promotion commit.

Both branches are slated for deletion once the rest of their feature content has landed.

## Verification

All gates passed at commit time (`nix develop --command git commit`):

```
no fake/placeholder implementations......................................Passed
no debug-print leftovers (use the tracing facade)........................Passed
no commented-out code....................................................Passed
secrets scan.............................................................Passed
rustfmt..................................................................Passed
clippy (-D warnings).....................................................Passed
cargo-deny (licenses + advisories).................(no files to check)Skipped
```

(cargo-deny skipped because this commit doesn't change `Cargo.toml`/`Cargo.lock`. It will fire on subsequent commits that touch the dep graph.)

## Test plan

- [ ] CI: \`nix flake check\` and the standard test matrix
- [ ] \`nix develop\` enters cleanly and prek hook is installed
- [ ] \`prek run --all-files\` passes on a clean checkout
- [ ] \`cargo deny check\` passes
- [ ] Smoke-test the new logging slow-path: induce a slow render (e.g. resize storm) and confirm WARN-level \`render.frame\` event appears with \`outcome=slow\`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
