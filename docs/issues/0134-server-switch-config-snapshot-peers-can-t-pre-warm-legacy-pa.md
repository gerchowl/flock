---
number: 134
title: "Server switch: config/snapshot peers can't pre-warm + legacy path has no timeout-bounded fallback"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T13:07:43Z
closed: 2026-06-14T13:32:44Z
url: https://github.com/gerchowl/herdr/issues/134
---

# Server switch: config/snapshot peers can't pre-warm + legacy path has no timeout-bounded fallback

## Problem

Why switches are "sometimes fast, sometimes slow":

- **Fast** = the target slot is already warm; `flip_to` returns a live stream and the switch is instant in-process (`apply_slot_flip`, `src/client/mod.rs:2145`).
- **Slow** = cold dial (see #DEPENDS for the round-trip cost).

The background warm sweep (`src/client/mod.rs:2050-2087`) only warms slots that have a `slot_socket_path`, which in stage-1 is just Home + the active leg (`src/client/mod.rs:2709-2725`). **Config-peer and snapshot-peer slots are never pre-warmable**, so those switches always pay the cold-dial cost.

Separately, the **legacy non-slots path** (`slot_manager == None`) has no popup, no timer, and no cancel: the client exits, the launcher holds the alt-screen on the previous frame, and `run_remote` → `prepare_remote_herdr` blocks on SSH (`src/main.rs:378-453`, `src/remote.rs:219+`). If SSH hangs, `run_remote` never returns an error, so the `LegStep::FallBack` recovery (`src/main.rs:521-529`) never fires — the user is stranded on a frozen frame.

## Fix (options)

1. **Timeout-bounded fallback (smaller):** give the cold-dial worker an overall deadline and ensure a hung dial surfaces as a failure the popup/launcher can recover from; bound the bridge ssh too. (Largely enabled by #DEPENDS-1.)
2. **Pre-warm SSH peers (larger, stage-2):** extend the warm sweep so config/snapshot peers can be background-dialed like Home, making their switches instant too.

Start with (1); track (2) as follow-up if it grows.

## Acceptance

- A cold switch that cannot connect surfaces a failure and returns the user to the previous server within a bounded time on **both** the slots and legacy paths — never a permanently frozen screen.

Context: branch `server-switch-slow`.

---

## Comments

### gerchowl — 2026-06-14T13:07:58Z

Related: #133 is the round-trip cost ("#DEPENDS"), #132 is the ssh-timeout fix ("#DEPENDS-1") that enables the timeout-bounded fallback here.

### gerchowl — 2026-06-14T13:30:01Z

Option 1 (timeout-bounded fallback) landed in #138. Option 2 (pre-warm config/snapshot SSH peers, stage-2) split out into https://github.com/gerchowl/herdr/issues/139.

