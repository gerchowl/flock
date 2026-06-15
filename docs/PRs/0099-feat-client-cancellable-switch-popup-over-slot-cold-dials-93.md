---
number: 99
title: "feat(client): cancellable switch popup over slot cold dials (#93)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T16:34:30Z
closed: 2026-06-12T17:01:23Z
merged: 2026-06-12T17:01:22Z
base: master
head: switch-popup
url: https://github.com/gerchowl/herdr/pull/99
---

# feat(client): cancellable switch popup over slot cold dials (#93)

Implements #93 (server-switch popup with cancel-and-return) for the **slots-enabled** path. Warm flips are sub-ms (no popup needed) and the legs path (`[slots] disabled`) keeps today's exit-and-relaunch UX untouched — every touchpoint guards on `slot_manager` existing (the #76 discipline).

## What changed

### Promote cold dials into in-process async switches
Today the `SwitchServer` arm's `Ok(None)` (cold/unknown) from `flip_to` falls through to the launcher's record-and-exit path. With slots on, that fallthrough is gone: instead the loop arms a `PendingSwitch` and spawns a `spawn_switch_dial` that

- dials the slot's local socket directly when one is already live (home, the active leg's launcher-owned ssh bridge);
- builds an **optimistic client-side `SshStdioBridge` non-interactively** for a bridge-less ssh peer (new `pub(crate) fn start_switch_bridge_noninteractive` in `src/remote.rs` — uses the PATH `herdr` if it matches version; no install prompts, no live-handoff approval — a missing/incompatible remote binary surfaces as a dial failure).

The bridge is returned to the loop inside the success event and stored on the slot's `SlotConnection.bridge`, so the existing `SshStdioBridge::Drop` tears it down with the slot.

### Generation counter, not a flag
`PendingSwitch { gen, target, previous_display, target_display, started_at, outcome_beat }`; warm-sweep dials tracked in `HashMap<String, u64>` (replacing the prior `HashSet`); every `SlotWarmed { gen, key, stream, bridge }` / `SlotDialFailed { gen, key }` carries the gen it was spawned with. Apply-time dispatcher `classify_dial_event` returns `Pending` / `Sweep` / `Stale`. A stale-on-arrival success drops its stream (server sees disconnect) and never flips. Cancel/re-switch bumps gen.

### Shared success path
The warm-flip arm and cold-dial success arm both call a new `apply_slot_flip`: retire the old reader, bind a new one, flip `active_slot_key` before any further events apply, swap `write_stream`, re-assert geometry (#77), request a full redraw. Popup teardown happens inside the success arm BEFORE further events apply (PopupGuard discipline). After success a **150ms esc-grace** swallows a lone Esc chunk so a late muscle-memory press doesn't land in the new session.

### Popup painting
Client-side raw-ANSI box, centered via `state.reported_size`, ~50x4: line1 `switching to <target>…  Ns`, line2 `[esc] cancel · returns to <previous>`. Tone schedule:
- neutral 0–3s,
- yellow at 3s,
- subtitle `host not responding — [esc] returns to <previous>` at 10s,
- `retry window ending soon — [esc] returns to <previous>` at ~25s.

No background dim. Repaints on the existing 100ms Timer arm, throttled to `POPUP_REPAINT_INTERVAL` (~220ms / ~4-5x sec). Failure beat: `switch to <target> failed: <err>` for ~2s, then clear + full redraw. Cancel beat: `cancelled ✓` for ~600ms.

### Esc detection
In `StdinInput`: a chunk equal to **exactly `[0x1b]`** while `pending_switch.is_some()` is cancel (bump gen, beat, stay on previous slot — `active_slot_key` unchanged). Any longer chunk starting with `0x1b` (arrow keys, F-keys, CSI sequences, alt-combos) passes through untouched. Esc is **never** intercepted when `pending_switch` is `None`.

### focus_workspace
The slots path has no `ClientMessage` for focusing a workspace today (legs/switch-file mechanism owns that). The slots cold-switch path drops `focus_workspace` with a doc-comment; the legacy legs path keeps focus support, and the gap closes when the protocol grows a `FocusWorkspace`-like message (#75 vicinity).

## Tests
- `classify_dial_event` disposition table — pending-match, sweep-match, stale-success-after-cancel, stale-on-outcome-beat, wrong-key with matching gen, no-pending-no-sweep.
- `is_bare_esc_chunk` table — `[0x1b]` alone is cancel; `ESC [ A` (arrow up), `ESC O P` (F1), `ESC a` (alt-a), empty, and `"a"` all pass through.
- `popup_lines` / `popup_tone_ansi` — neutral / yellow / unresponsive / retry-ending / outcome-beat-override.
- `cancel_bumps_gen_and_leaves_active_slot_unchanged` — Esc-cancel bumps gen; a late dial success with the original gen dispatches Stale.
- `rapid_switch_cancel_switch_only_second_pending_matches` — A → cancel → A: second dial's success is `Pending`, first dial's late success stays `Stale`.
- `apply_slot_flip_replaces_write_stream_against_socketpair` — the shared flip helper rebinds against a real UnixStream pair, raises the old reader-quit, advances `active_slot_key`, and writes a Resize re-assert visible on the new peer.

All 2156 unit tests pass; `--test client_mode`, `--test live_handoff`, `--test peer_federation` green; fmt + clippy (`-D warnings`) clean.

## Test plan
- [ ] Manual: with `[slots] enabled` and a configured but unreachable peer, switch via sidebar — popup shows, elapsed counts, Esc returns to previous, redraw is clean.
- [ ] Manual: rapid A → Esc → A while the first dial is still in flight — the first dial's late success is observably dropped (no flip) and the second lands.
- [ ] Manual: slots disabled — switch still uses the legacy exit-and-relaunch (no behavior change).

---

## Comments

### gerchowl — 2026-06-12T17:01:20Z

Post-impl review (fresh agent): **approve with nits** — all hazard paths verified sound (gen discipline, beat-state cancels, slots-disabled purity). Fixed before merge: split-Esc debounce (30ms hold — phantom-cancel under load closed), stale-bridge drop moved to a detached thread (loop-stall closed). Accepted as-is: ConnectionLost popup residue (TerminalGuard covers), full-region erase already replaced-by-need, try_init wording (it closes the silent-abort; the notice-rail refinement rides #75). Known test gap, honest: no loop-level split-Esc delivery test (the joining is inline; classifier-level coverage exists).

