---
number: 38
title: "client: auto-retry attach during live-handoff window instead of bailing to shell"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T07:17:49Z
closed: 2026-06-11T11:53:23Z
url: https://github.com/gerchowl/herdr/issues/38
---

# client: auto-retry attach during live-handoff window instead of bailing to shell

## Symptom (live, during a real handoff)

```
❯ herdr
herdr: server shut down: live update in progress; reconnect after handoff completes
```

The client exits to the shell mid-handoff. The user has no signal for when the handoff finished and has to retry blind — even though the window is typically sub-second.

## Fix

When the attach is refused with the live-update notice, the client should retry with backoff (e.g. 200ms intervals up to ~30s) showing a one-line spinner ('handoff in progress, reconnecting…'), then attach normally. Timeout → today's message. Same treatment in the launcher attach-loop (src/main.rs) so a SwitchServer relaunch that races a handoff also waits.

## Notes
- The refusal string originates server-side during `server live-handoff`; grep "live update in progress".
- COORDINATE: #36 (hub-and-spoke) is concurrently reworking the attach-loop/SwitchServer path — land this after #36 to avoid conflicts on src/main.rs.

---

## Comments

### gerchowl — 2026-06-11T11:53:22Z

Shipped in PR #52.

