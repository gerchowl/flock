---
number: 93
title: "spike: server-switch popup with cancel-and-return (stall-proof switching)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T14:56:42Z
closed: 2026-06-12T17:01:46Z
url: https://github.com/gerchowl/herdr/issues/93
---

# spike: server-switch popup with cancel-and-return (stall-proof switching)

## Motivation / why

Server switches today show #67's frozen last frame + a status line; a stalled dial (unreachable host, hung ssh) leaves the user staring at a frozen screen with no agency — the only exits are the #52 retry window expiring or killing the client (now safe post-#72, but still a dead end). A switch POPUP with elapsed time and a working CANCEL that returns to the previous server turns every switch into a recoverable action. The user hit exactly this twice today (the proto-16 hang, the "totally broke herdr" switch).

## Decision / proposed approach

Overlay popup on switch initiation: `switching to sage… 3s  [esc] cancel`. Two architecture paths, increasingly capable:

1. **Slots path (`[slots] enabled`, #76)**: the old server's live frame stays underneath (no freeze at all); the dial runs async; cancel = abort the dialing slot (demote to cold) and simply stop — the active slot never changed. Stall = elapsed keeps counting; cancel always works. Hypothesis: nearly free.
2. **Legacy legs path (#67)**: the old client exits before the dial, so "the old view" is a frozen frame held by the launcher chain. Cancel = user-triggered instance of the EXISTING failure path (launcher abandons leg N, relaunches leg N-1 with a notice). Requires: the dialing leg to poll for a cancel keypress while connecting (stdin is held — the #72 status-line precedent paints over the held screen, so reading a key there is plausible), and `decide_next_leg` to treat cancel like failure-with-notice.

Client-side overlay machinery is NEW (the client paints only server frames today); precedent: #72's retry status line paints directly over the held screen — the popup generalizes that into a small client-drawn box.

## What already exists
- #67 frozen-frame hold + failure→previous-leg relaunch + Hello.notice (src/main.rs decide_next_leg, src/client/mod.rs)
- #72 HeldRestoreGuard + visible retry status painting over the held screen (the overlay precedent)
- #76 SlotManager/SlotRegistry: async dial, demote, in-process flip (src/client/slots.rs)
- #52 retry window (timeouts unchanged)

## Scope
P0: popup overlay (target + elapsed) on both paths; cancel via Esc (+ q) under slots = abort dial, stay put; cancel under legs = abandon leg, relaunch previous with "switch cancelled" notice.
P1: click-cancel; show the retry-window state inside the popup (replacing the bare status line).
P2: progress detail (dial/handshake/first-frame phases).

## Pitfalls
- stdin during a legs dial: who reads the Esc? The launcher between legs vs the dialing client pre-handshake — must not eat bytes destined for the next session (#72's pre-handshake OSC capture already walks this line).
- Slots cancel racing dial success: the flip must check a cancel flag before swapping (the #76 reader-rebind discipline applies).
- Double-cancel / cancel-after-success: idempotency.
- The popup must never outlive its switch (leaked overlay over a live session).
- Esc conflicts: only while a switch is in flight — never steals Esc otherwise.

## Acceptance
- [ ] Switch to an unreachable host: popup shows, elapsed counts, Esc returns to the previous server view (both paths), notice confirms cancellation.
- [ ] Successful switch: popup vanishes on first frame; no Esc theft after.
- [ ] Cancel race with success: deterministic (either lands, never half-states).
- [ ] e2e for the slots-cancel and legs-cancel paths.

## References
#67/#72/#76/#52, src/main.rs, src/client/mod.rs, src/client/slots.rs.

---

## Comments

### gerchowl — 2026-06-12T14:59:41Z

## Consolidated review (3 fresh agents: architect / TUI-UX / cancellation-correctness)

### Unanimous
- **Slots is the home of the feature.** The "two paths" framing was an illusion: warm flips are sub-ms (no popup needed) and a COLD switch under slots currently falls through to the legs path — the stalled-switch case IS the cold dial. Scope: promote cold dials into async slot dials with the popup; the popup covers every slow switch by construction.
- **Generation counter, not a flag** (architect + concurrency independently): cancel→re-switch races make a boolean unsound (a stale SlotWarmed carrying a real handshaked stream must be dropped-with-Detach by gen mismatch, never flipped).
- **Popup renders client-side** over the active slot's still-live frame (the old view never died under slots); lifetime = a PopupGuard (Drop → request_full_redraw, #72's guard pattern), torn down INSIDE the flip match arm.
- **Esc gated strictly on pending_switch.is_some()** — no theft otherwise.

### UX spec (adopted)
No background dim (live frame stays legible). Three lines: `switching to sage… 3s` / `[esc] cancel · returns to mba22` — the return-destination subtitle is the feature. Tone shifts: neutral 0–3s, yellow 3–10s, "host not responding" 10s+, "retry window ending" ~25s (keyed to #52's ~30s). Esc-flush on success + ~150ms `landed ✓` beat (the muscle-memory race). P1: absorb #72's reconnect wait into the same visual language with honest labels (`[esc] disconnect` — no "previous" exists there).

### The one fork (architect vs concurrency)
- Architect: **legs Esc = won't-fix** (ssh inherits stdin at spawn; the #72 OSC-capture precedent doesn't generalize to arbitrary keys); legs gets only a richer status line until slots is default.
- Concurrency: provides a SOUND legs-cancel mechanism if wanted — launcher-owned raw-mode Esc watcher + `child.kill()` + **mandatory `child.wait()`** (Child::drop does not reap — zombie risk), cancel rides decide_next_leg's existing failure rail as "switch cancelled".

### Pitfalls added by review
gen-stamped dial events; stale-stream Detach-on-drop wrapper; popup teardown ordered before any Frame applies post-flip; double-notice suppression on stale failures; never `continue` past the guard scope.

### gerchowl — 2026-06-12T17:01:46Z

Spike complete, shipped in PR #99 (+ review-round fixes). Decision trail: 3-agent design review (architect/UX/concurrency) → slots-only with gen-counted cold dials promoted out of the legs fallthrough; popup = client-drawn over the live frame; cancel = gen bump; tones at 3/10/25s; esc debounce 30ms; bridge bootstrap client-side, detached teardown. Follow-ups: focus_workspace over slots rides #75; #72 reconnect-wait absorption = P1 there too.

