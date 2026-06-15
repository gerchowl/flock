---
number: 65
title: "connection slots: multi-connection client — pre-load/switch/pause instead of exit-and-relaunch legs"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T19:59:24Z
closed: 2026-06-12T00:17:44Z
url: https://github.com/gerchowl/herdr/issues/65
---

# connection slots: multi-connection client — pre-load/switch/pause instead of exit-and-relaunch legs

## User-designed architecture (follows #63)

Replace the exit-and-relaunch leg model with **connection slots inside one client process**: the painter holds N server connections; switching servers flips which slot feeds the paint/input loop. The client never releases the terminal — the blip, the stdin-handover dance, and the terminal push/pop choreography disappear *by construction*.

```
client (one process, owns the tty forever)
 ├─ slot[active]: sage   — frames painted, input forwarded
 ├─ slot[warm]:   mba22  — connection held, frames PAUSED (home: always warm)
 └─ slot[cold]:   anvil  — no connection; lazy dial on first switch
```

- **Switch** = pre-dial target in the background (current view keeps painting) → on Welcome+first frame, flip the active pointer + full-redraw (exists as redraw-on-attach). Instant when warm.
- **Pause** = new protocol message (frames unsubscribe/resume) so a paused server stops streaming instead of backpressuring; ssh keepalive holds the transport. Resume = full redraw.
- **Slot policy**: active + home always; LRU cap for the rest (e.g. 3 total, configurable). Cold-drop on cap.
- **Failure** = the dial fails while the old slot still paints → top-right notice (`switch to X failed: …`), zero teardown. #63 part 3 becomes trivial here.
- **Fleet snapshot (#39)**: no more env-var leg handoff — the client holds the hub's live peer state and hands it to whichever slot it attaches; pass-through semantics keep working. Theme (#52): captured once by the client, sent on every slot's Hello.
- **Protocol**: one additive pause/resume message (bump if bincode strictness demands — single-owner fleet, lockstep deploys are routine).

## Relationship to #63
#63's part 1 (spaces remote-row click bug) and part 3 (failure notice) stay valid as shipped. Part 2's pragmatic fix (held alt-screen splash or similar) is the interim; this issue supersedes it as the proper mechanism.

## Pitfalls
- Both event loops trap doesn't apply client-side, but the CLIENT event loop becomes a select over N framed streams + tty input — needs careful per-slot read-state isolation (a slow warm slot must never stall the active paint path).
- Server-side per-client sessions persist while paused: verify server client-session cost is negligible and reaped on real disconnect.
- live-handoff (#38/#52 retry window) now happens per-slot; a paused slot surviving a server handoff should reconnect lazily on resume rather than retry-spinning in the background.
- ssh transport death of a warm slot → demote to cold silently; only surface if the user switches to it.

---

## Comments

### gerchowl — 2026-06-11T20:04:28Z

## Refinement (user): warm-all, not LRU

ALL fleet servers hold parallel warm slots by default: eager background dial at client start (config peers + home), frames paused until active. Personal-fleet cost is negligible (one idle ssh transport + one paused server session per slot). Down servers: demote to cold, gentle backoff redial, ghost row as today; switching to one surfaces the failure notice. The LRU cap stays only as a large-fleet guard (`[slots] max`, default well above a personal fleet). Follow-on once slots exist: paused connections can carry lightweight live status, retiring the ssh summary poller AND snapshot staleness (#66 data goes live).

### gerchowl — 2026-06-11T20:12:07Z

## Economics + unification (user reasoning, locked)

1. **Cap is vestigial** — sanity bound only (`[slots] max`, generous default). Policy = warm-all for the fleet.
2. **Server-side cost ≈ zero**: servers are always-on daemons regardless; a warm client is one accepted connection + a paused render session inside the existing process — no spawned processes piling up.
3. **The wire already exists — UNIFY it**: the hub holds persistent ssh transports for the 15s summary poller today. Slots absorb them: ONE connection per peer carries status while paused (replacing the poller) and frames while active (replacing the relaunch legs) and the carried snapshot (replacing the env handoff). Three subsystems → one connection. Net new infrastructure: none.
4. **Attention-driven warming** for non-warm slots: when status gossip reports an agent on server X entering blocked/wants-input, pre-dial X — it's the likeliest next leap. Keyed to the existing focus_attention ranking (blocked > done-unseen). Warm-all fleets get it for free; it's the policy for cold/down/big-fleet tails.

### gerchowl — 2026-06-12T00:17:43Z

Stage 1 shipped in PR #76 (review-hardened: slot-tagged events so warm death demotes instead of killing the session; apply-time stale-frame dropping; explicit bridge-socket plumbing). Proto 18, [slots] enabled=false default — flip on to dogfood. Stage 2 (live status over paused slots, poller retirement, full cold-fleet bridging) = #75.

