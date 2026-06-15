---
number: 83
title: "pane keystrokes stall while the pane is under load (UI stays responsive — input→PTY leg starves)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-12T09:37:15Z
closed: 
url: https://github.com/gerchowl/herdr/issues/83
---

# pane keystrokes stall while the pane is under load (UI stays responsive — input→PTY leg starves)

## Bug (live, post-c0c9d45)

When something is under load (busy agent/build in a pane), herdr panes stop accepting keystrokes. Crucially: the host terminal outside herdr is fine, AND herdr's own UI keeps responding (space/tab switching works) — so client→server input is healthy; the failure is the input→PANE path while that pane is busy.

## Hypotheses (in likelihood order)
1. **Input queued behind the output deluge**: a busy pane floods PTY output; if input forwarding to that pane's PTY shares a channel/lock/thread with its VT processing or frame rendering, keystrokes wait behind megabytes of parse work — perceived as "not accepting" (they'd land late). Check the per-pane runtime: is there one task draining both directions? Does input write contend with the output read/VT lock?
2. **PTY write blocking/dropping**: the pane process not reading stdin (busy) → PTY input buffer full → server's write blocks (starving something) or errors/drops. Check the write path's blocking mode + error handling.
3. **Echo starvation only**: input lands instantly but the echo frame is starved by output-frame coalescing under load — user can't tell input arrived. (Distinguish: type a command blind + Enter; does it execute?)

## Diagnostic split (user to confirm if observable)
Keystrokes EVENTUALLY appearing (late) → hypothesis 1/3. NEVER appearing → 2.

## Acceptance
- Typing into a pane whose process floods output (e.g. `yes` or a build) stays responsive: input lands within normal latency; no loss.
- Regression test: channel-runtime or e2e — flood a pane's PTY output while writing input; assert input bytes reach the PTY child promptly and in order.

## References
src/terminal/runtime.rs (per-pane runtime, VT processing), pane input encode/forward path, server frame scheduling/dirty coalescing.

---

## Comments

### gerchowl — 2026-06-12T09:41:20Z

## Reframe (user observation + live measurements)

Trigger is RAM PRESSURE, not CPU — and herdr loses first AMONG ALL processes while everything else stays interactive. Measurements kill the bloat hypothesis: herdr server RSS ≈ 35MB, clients 4-15MB (claude processes are ~500MB each — herdr is one of the SMALLEST things running).

Revised mechanism, two compounding parts:
1. **Multi-hop major-fault amplification**: a keystroke crosses client → socket → server → PTY → child → PTY → VT → frame → socket → client. Under pressure every idle-waiting hop has paged-out/compressed pages; one keystroke pays a SERIES of major-fault wakeups. A plain terminal is one hop and barely notices.
2. **macOS QoS/darwinbg**: the headless server is windowless and likely default/background QoS — exactly what the kernel targets for compression + timer throttling under pressure. The interactive pipeline is being treated as batch.

## Fix direction
- Assert QOS_CLASS_USER_INTERACTIVE (pthread_set_qos_class_self_np) on the server's event/input threads + the client's stdin/paint threads; consider PRIO_DARWIN_PROCESS / latency-sensitive task policy for the server process. Audit how the headless server is spawned (inherits what?).
- Keep the output-flood input-priority audit from the original body — orthogonal and still worth having.
- Testability: QoS assertions are queryable (qos_class_self) — unit-test they're applied; the flood-input regression stays; actual memory-pressure stalls are documented-manual.

