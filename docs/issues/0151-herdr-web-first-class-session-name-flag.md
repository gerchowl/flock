---
number: 151
title: "herdr web: first-class `--session <name>` flag"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-15T00:34:15Z
closed: 2026-06-15T02:21:29Z
url: https://github.com/gerchowl/herdr/issues/151
---

# herdr web: first-class `--session <name>` flag

Follow-up to #131 (P3 ergonomics).

## Motivation
The bridge attaches via default-launch + a `-- <herdr args>` passthrough. An explicit `--session <name>` makes the persistent-attach intent legible (launchd unit says `--session main`, not `-- --session main`) and documents that the phone shares a specific named session.

## Acceptance
- [ ] `herdr web --session <name>` forwards to the spawned client
- [ ] Documented in `herdr web --help`

Refs #131.

---

## Comments

### gerchowl — 2026-06-15T02:21:29Z

Done in #154 (merged): first-class `--session <name>` forwarded as the global flag.

