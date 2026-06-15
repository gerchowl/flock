---
number: 141
title: "Self-detect missing/outdated agent integration and nudge in-band"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-14T13:40:42Z
closed: 
url: https://github.com/gerchowl/herdr/issues/141
---

# Self-detect missing/outdated agent integration and nudge in-band

Follow-up to #136 (P1). herdr has no in-band signal when the active agent's integration is missing/outdated — the panel just shows nothing; `herdr integration status` is the only way to find out.

## Scope
- [ ] On session/server start, check the active agent's integration status (already computed by installed_integration_statuses) and surface a one-line hint when missing/outdated.
- [ ] Persistent header badge for steady-state (not-installed / outdated / current=silent); transition-only or snooze-gated toast (no per-session nag).
- [ ] Suppression: snooze + dismiss + explicit mute persisted in herdr state. Never auto-write settings.json (first-run consent at most).
- [ ] Remote: check on the remote, surface locally, prefixed with the host.
- [ ] integration status: add --json + meaningful exit codes (0/1/2).

## Pitfalls
- Nag-blindness unless toast is strictly transition/snooze-gated -> fall back to badge-only.

Refs #136. Builds on the manifest data from #140.
