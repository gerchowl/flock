---
number: 45
title: "session-comms patterns from dompt: full-state LWW + late-join seed, absolute commands, holder token, wire-shape contract tests"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-11T08:40:57Z
closed: 
url: https://github.com/gerchowl/herdr/issues/45
---

# session-comms patterns from dompt: full-state LWW + late-join seed, absolute commands, holder token, wire-shape contract tests

Notes from dompt (nerd-machines/dompt), where we just finished an audit-driven rework of the presenter↔phone↔core comms (nerd-machines/dompt#170, ADR-0005/0008 there). herdr's cross-machine session comms (proto-based) and dompt's hub protocol solve neighboring problems; these are the semantics-level patterns that did the heavy lifting for us and are encoding-agnostic — they apply on top of protobuf unchanged:

1. **LWW full-state snapshots + late-join seed, not deltas.** Every broadcastable state (presentation, settings, transport cursor, talk clock) is published as the FULL document, last-writer-wins, with a `*_get` seed command for late joiners. Deltas are where cross-machine session sync goes to die: a missed delta = silent divergence, and you end up building resync anyway. Full-state made reconnect/resume free.
2. **Absolute, idempotent commands — never toggles.** `set_fullscreen(bool)`, `set_running(bool)`, `present`/`stop_present`. Two peers issuing "pause" near-simultaneously must converge on paused, not double-toggle back. Our worst field bug (a fullscreen blip) was exactly a toggle under duplicate delivery; it became structurally impossible once commands were absolute, and idempotence turned into a plain unit test on the pure transition function.
3. **Single-holder token for "who drives".** One mutable token, any authenticated peer may take it, holder-only writes, token auto-releases on disconnect. Kills write-races without locks or CRDTs for the control plane.
4. **Contract tests pinning exact wire shapes.** Proto gives you schema compat at the field level, but not "this payload means this". We pin the literal wire shape of every cross-boundary message in unit/e2e tests on both ends. With proto you still want these for the SEMANTIC contract (units, value ranges, which fields are echoed back).
5. **(Counter-lesson, for symmetry)** dompt skipped a protocol version field because the hub serves every peer its own client bundle — no independent release cycles, no skew. herdr's multi-machine sessions are the opposite case: your proto versioning is the right call there, and it's what dompt will copy the day a peer ships separately (parked as nerd-machines/dompt#139 remark).
