---
number: 140
title: "feat(integration): publish the integration contract as data (`integration manifest`)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-14T13:39:21Z
closed: 2026-06-15T00:25:23Z
merged: 2026-06-15T00:25:23Z
base: master
head: feat/integration-manifest-seam
url: https://github.com/gerchowl/herdr/pull/140
---

# feat(integration): publish the integration contract as data (`integration manifest`)

P0 of the #136 design (agent integration travels with herdr). Implements the **manifest seam** the consolidated review converged on.

## What
`herdr integration manifest <target> [--json]` emits the integration contract as data: the version, hook script path, settings path, and the **exact `settings.json` hooks fragment** herdr would install.

Consumers on a **read-only or externally co-owned `settings.json`** (Nix/Home-Manager store symlinks; or a file another tool co-owns) can now **declare the hook entries themselves** from the manifest, instead of relying on `install` to patch a file it can't (or shouldn't) write.

## How (anti-drift)
The canonical Claude hook entries now live in one `CLAUDE_HOOK_ENTRIES` table consumed by **both**:
- `install_claude` (writes them via `ensure_command_hook`), and
- `claude_hooks_fragment` (what `manifest` emits).

Test `manifest_hooks_match_installed_settings` asserts the manifest fragment is byte-identical to what `install` actually writes — so the two sources of truth cannot drift.

## Scope
- ✅ `integration manifest` for **claude** (+ `--json`, human summary); unsupported targets return a clear error.
- Verified: `cargo clippy --all-targets -D warnings` clean, `cargo fmt` clean, new tests pass, real-binary smoke test emits the correct fragment.

## Not in this PR (follow-ups on #136)
- Read-only-aware / idempotent `install` (don't fail when settings.json is unwritable; atomic merge when it is).
- Richer `integration status` (declarative install reads green) + `--json` + exit codes.
- Self-detect + in-band nudge (P1); ship `herdr statusline` + config defaults (P2); content-hash drift detection.
- Manifest for the other JSON-settings targets (copilot/qodercli/codex).

Refs #136.

---

## Comments

### gerchowl — 2026-06-14T13:42:42Z

## Second-round review (2 fresh-context agents: Rust-correctness + integration-contract)

**Both APPROVE.** The refactor is byte-for-byte correct (`CLAUDE_HOOK_ENTRIES` produces the same entries `install_claude` made; `claude_hooks_fragment` builds the identical JSON shape `ensure_command_hook` writes), CLI exit codes are consistent, no panics/unwraps in non-test code, clippy-clean.

**Applied from review (this PR):**
- anti-drift test strengthened: added a variant with a pre-existing user SessionStart hook (exercises the append branch + asserts the manifest entry is present verbatim and the user hook survives).
- unsupported-target test now asserts the label is interpolated (`kimi`).
- summary renderer surfaces a serialization failure loudly instead of silently dropping the fragment.

**Deferred to follow-ups (not blockers):**
- **manifest-aware status** — the script-vs-settings half-state. Filed as its own issue; the reviewers flagged it as the next thing to build (before/with read-only-aware install).
- host-specific `hookScript` path: fine for same-host eval, but a manifest generated on machine A and rendered for machine B gets a wrong path. Add a `--hook-path` override / 'evaluate on the target host' doc note — minor, follow-up.
- typed error when `$HOME`/`claude_dir()` can't resolve.

