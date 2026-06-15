---
number: 50
title: "feat(pane): promotable header fields — set-field/clear-field (#40)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T09:44:17Z
closed: 2026-06-11T09:45:02Z
merged: 2026-06-11T09:45:02Z
base: feat/sidebar-row-gap
head: header-fields
url: https://github.com/gerchowl/herdr/pull/50
---

# feat(pane): promotable header fields — set-field/clear-field (#40)

Implements #40 (body + scope-additions comment): sessions can promote session-specific facts — dev containers, long-task progress, ports, model names — into their own pane's header as compact `key value` chips, visible everywhere panes are chosen.

## API
- New JSON-RPC methods `pane.set_header_field` / `pane.clear_header_field` (additive, no version bump). Pane resolved exactly like the other `pane.report_*` calls: env-baked `HERDR_PANE_ID` claim, verified/healed by socket-peer process ancestry.
- Handlers validate synchronously (key ≤ 16 chars, value ≤ 48 chars, max 6 fields/pane — over-cap rejected with `invalid_header_field` / `too_many_header_fields`), then the mutation rides `update_terminal_state` via an internal event — the shared chokepoint both event loops consume. Zero new `request_*` fields.

## State
- `TerminalState.header_fields`: ordered `key → (value, expires_at)` (new `src/terminal/header_fields.rs`). Updating an existing key keeps its slot and never hits the cap.
- **Not persisted**: snapshots never contain fields (ephemeral by design — a restored session's containers are unknown). Contract test included.
- TTL expiry piggybacks the existing agent-metadata scheduled tick: `next_agent_metadata_expiry` folds in field deadlines, `expire_agent_metadata_at` sweeps expired chips — both loops share that path. Reads also filter expired chips, so renders never show a stale chip before the tick fires.

## Render / nav surfaces (scope comment)
- Pane header context line: chips after branch/PR, muted key + text value; project/branch segments win width, chips fit leftover in insertion order with middle-truncated values (`fit_header_field_chips`, unit-tested).
- Fields ride `PaneDetail` (same plumbing as `custom_status`) into: sidebar agent panel rows, navigator pane lists (also searchable: querying `73%` finds the pane), and mobile pane lists. Per-surface value budgets: header (48) > agent panel (24) > nav/mobile (16).
- Sidebar diff confined to the agent-panel entry struct/builder/row render.

## CLI + docs (scope comment)
- `herdr pane set-field <key> <value> [--ttl <secs>] [--pane <pane_id>]` and `herdr pane clear-field <key> [--pane <pane_id>]`; pane defaults to the calling pane (`$HERDR_PANE_ID`, ancestry-healed), so hooks/wrappers need no bookkeeping.
- Documented in `herdr pane --help`, website CLI reference (with podman-wrapper/build-script examples), and the socket API docs (new "Promoted header fields" section + raw-methods table).

## Tests
9 new: set/clear RPC round-trip through `update_terminal_state`; cap + length rejections; TTL arms the shared deadline and the shared sweep drops the chip; headless scheduled-tasks expiry (both-loops guarantee); header chip fit/truncation; PaneDetail carries fields; navigator meta + search; mobile detail; snapshot exclusion contract.

Full suite: **1962 passed, 0 failed** (`cargo test --bin herdr`); `cargo fmt` + `clippy --all-targets` clean.
