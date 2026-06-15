---
number: 136
title: "Agent integration should travel with herdr: self-detect/nudge + declarative install instead of per-host re-wiring"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-14T13:18:40Z
closed: 
url: https://github.com/gerchowl/herdr/issues/136
---

# Agent integration should travel with herdr: self-detect/nudge + declarative install instead of per-host re-wiring

## Motivation / why

herdr's agent integration (the hooks that feed the top panel: last prompt, idle/blocked/done state, the attention queue) currently only works on a host if **two independent things** are both present and agree:

1. the hook **script** `~/.claude/hooks/herdr-agent-state.sh` (installed by `herdr integration install claude`), and
2. the **hook entries** in `~/.claude/settings.json` pointing at it.

On a Nix-managed fleet this splits badly: on macOS `settings.json` is a **read-only /nix/store symlink**, so herdr's own settings.json patch can't run there — the entries have to be re-declared in the machine-config repo (g-fleet). On the Linux servers neither half is present (`herdr integration status` → `claude: not installed`), so the panel shows no agent state at all. The integration — herdr's own concern — is leaking into and being re-implemented per-host in an external config repo, and silently absent where nobody ran the install.

**Goal:** a host running herdr gets the full, correct integration *from herdr*, with zero/minimal external per-host wiring, and herdr tells you when it's missing instead of failing silently.

## Current state (what exists)

`herdr integration install <agent>` (`src/integration/mod.rs`):
- writes the bundled script asset (`src/integration/assets/claude/herdr-agent-state.sh`) to `~/.claude/hooks/`,
- **patches `~/.claude/settings.json`** (serde_json merge of the hook entries),
- stamps `CLAUDE_INTEGRATION_VERSION = 6` (marker `HERDR_INTEGRATION_VERSION=`); `herdr integration status` reports current / outdated / not installed off that marker.

Gaps:
- **Two writers / read-only conflict:** herdr wants to own `settings.json`; on Nix hosts it's read-only and owned by Nix. No graceful handling — it just can't patch there.
- **No runtime detection:** grep confirms herdr has *no* in-band nudge. Status is only surfaced by the explicit `herdr integration status` command, so a missing/outdated integration is invisible until the panel looks wrong.

## Proposed approach (spectrum — compose 1 + (2 or 3))

1. **Self-detect + nudge (centerpiece):** on session/server start, herdr checks the active agent's integration status (it already computes this) and surfaces a one-line in-band hint when missing/outdated — e.g. `agent-status not set up — run 'herdr integration install claude'`. Host-agnostic, no settings.json ownership fight, cheapest win.
2. **Idempotent declarative install:** make `herdr integration install` safe to run unconditionally and degrade gracefully on a read-only `settings.json` (own the script + version; skip/verify the settings patch). Consumers run it via activation. Nix keeps owning the settings entries.
3. **herdr ships a consumer module:** `homeModules.default` / a settings fragment + the statusline binary + config.toml defaults, so consumers `import` herdr's integration rather than re-deriving it. Truest "travels with herdr"; heaviest.

## Scope

**P0**
- [ ] Self-detect integration status on startup; surface a non-nagging one-line hint when missing/outdated (suppressible).
- [ ] Make `integration install` idempotent + safe on a read-only `settings.json` (no hard failure; clear message if it can't patch).

**P1**
- [ ] Define herdr's settings contract so an external config tool can declare the entries and herdr's `status` agrees (don't fight it).
- [ ] Ship/expose the statusline + config.toml defaults from herdr (decouple from the consumer repo's bin/ + HM).

**P2**
- [ ] `homeModules.default` (Nix) consumers import for full integration.
- [ ] Decide what the external machine-config repo (g-fleet) then *stops* carrying.

## Pitfalls
- Read-only vs writable `settings.json` differ by host class (Nix store symlink on Macs; writable shared file on servers used by an account-switcher) — a single install strategy must handle both.
- A nudge that fires every session becomes nag-blindness; needs suppression + only-when-actionable.
- Version drift: the script marker (v6) and any externally-declared entries must stay in lockstep across herdr upgrades — the very coupling this issue exists to fix.
- Don't break `herdr integration status`'s existing contract / other agents (codex/pi/opencode/…) that share the mechanism.

## Acceptance criteria
- [ ] On a host with herdr but no integration, herdr tells you (in-band) and one documented command (or import) fixes it.
- [ ] Works on both a read-only-settings.json (Nix) host and a writable-settings.json host, without a per-host hand-wired settings fragment.
- [ ] A clear statement of what the external config repo no longer needs to carry.
- [ ] An ADR capturing the where/how decision + alternatives.

## References
- `src/integration/mod.rs`, `src/cli/integration.rs`, `src/integration/assets/claude/herdr-agent-state.sh`
- Consumer side (g-fleet): hook entries Nix-declared Mac-only in `modules/home/darwin-common.nix`; statusLine in `modules/home/common.nix`; config.toml band-aided onto NixOS hosts in `modules/herdr.nix`.
- Design spike — to be aligned via parallel agent review before implementation.

---

## Comments

### gerchowl — 2026-06-14T13:21:44Z

## Consolidated review (4 fresh-context experts: Nix packaging / TUI-UX / systems-integration / architecture-boundary)

### Strong agreement
- **herdr must NOT blindly author `~/.claude/settings.json`.** It's read-only (Nix store symlink) on one host class and shared-mutable (account-switcher) on the other — any "patch it" side-effect is wrong on at least one. Settings writes must be read-only-aware, atomic (flock + tmp + rename + .bak), and ideally separated from `install`.
- **herdr owns the script + publishes the hook entries as a verifiable fragment/manifest** (the declared contract), keyed by the version marker. The marker lives in the script; add a **content hash** so script-body drift within the same vN is detected without bumping the marker.
- **`integration status` needs a richer state model + machine-readability:** entries-present-regardless-of-author = installed/declarative (so a Nix-declared install reads green, not "not installed"); add exit codes (0/1/2) + `--json`.
- **Ship the statusline from herdr** (`herdr statusline` subcommand/binary) and **built-in styling defaults that user config overlays** — kill the consumer's hardcoded `~/dotfiles/bin/...` path.
- **Self-detect + nudge** is the cheap win: never auto-write settings.json (first-run consent prompt at most); a persistent header badge + a transition-only/snooze toast (no per-session nag). Remote: check on the remote, surface locally prefixed with the host.
- **Consumer (g-fleet) deletes**: hand-declared hook entries, the `~/dotfiles` statusline path, the config.toml band-aid, the macOS-vs-NixOS divergence. **Keeps**: channel pin, per-host opt-in, activation glue, restart-on-upgrade trigger.

### The real disagreement — the delivery seam
- **One reviewer:** herdr ships `homeModules.default` + `nixosModules.default` (+ a `lib.integration` data export) — ergonomic one-liner.
- **Two reviewers (stronger):** do NOT make a Nix module the contract — it over-couples herdr (which also serves brew/mise/non-Nix) to one toolchain and becomes a second surface that rots. Make the contract **DATA**: `herdr integration manifest --json` + the shipped statusline binary; consumers write thin ~20-line glue that consumes the manifest.
- Net: **data/manifest seam wins 3-to-1**; a Nix module, if any, should be a thin optional wrapper over the manifest, not the contract.

### Pitfalls surfaced (beyond the filed list)
- **Non-atomic settings.json write racing the account-switcher → corruption / half-merged creds.** Guard: read→merge→write-tmp→fsync→rename under flock, only in an explicit apply path, never on a symlink.
- **Nag-blindness** if the nudge isn't strictly transition/snooze-gated → fall back to badge-only.
- **Nix module becoming the de-facto contract** → changes get debated in Nix terms, the CLI/manifest seam rots.
- **Marker-integer drift**: script body changes silently within v6 → need a content hash.

### gerchowl — 2026-06-14T13:29:05Z

## Decision (accepted) — herdr publishes its integration as data; never blindly authors a foreign settings.json

(herdr tracks no `docs/adr/` — recording the decision here.)

**Decision**
1. herdr **owns the hook script** (bundled, versioned) — the one file `install` always writes.
2. herdr **publishes the hook entries as a manifest**: `herdr integration manifest [<target>] [--json]` emits the canonical entries + marker version + script path for the running binary. **This is the seam** — consumers (Nix/HM, Ansible, humans, postinstall) declare the entries from it; herdr need not write `settings.json` on declaratively-managed hosts.
3. **`integration install` becomes idempotent + read-only-aware**: when `settings.json` is unwritable (store symlink / EACCES) it does **not** fail — writes the script, prints the fragment to merge, returns a distinct status. When writable, merges atomically (tmp+rename), never truncate-in-place (can't corrupt a co-owned file).
4. **`integration status`**: entries-present-regardless-of-author = installed (Nix-declared reads green); add `--json` + meaningful exit codes.
5. **The contract is the CLI/manifest, not a Nix module** — a shipped `homeModules.default` would over-couple herdr (brew/mise/non-Nix users too) and become a second surface that rots. Optional thin wrapper over the manifest only, ideally a sidecar repo.

Marker stays in the script; a content hash guards silent script-body drift within a marker version.

**Alternatives rejected:** keep patching settings.json from install (wrong on read-only hosts; races co-owned files); ship a Nix module as the contract (3-of-4 reviewers against — coupling + rot).

**Delivery (multiple PRs):**
- **P0** (branch `feat/integration-manifest-seam`): `integration manifest [--json]` + read-only-aware idempotent `install` + status reads declarative installs as green. ← unblocks the consumer; in progress.
- **P1** (follow-up issue): self-detect + in-band nudge when the active agent's integration is missing/outdated.
- **P2** (follow-up issue): ship the status line as `herdr statusline` + built-in config.toml defaults the user overlays; content-hash drift detection.

