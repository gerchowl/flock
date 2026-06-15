---
number: 20
title: "fix(sidebar): stop status row flickering between spinner text and state label"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-09T22:45:46Z
closed: 2026-06-09T22:59:47Z
merged: 2026-06-09T22:59:47Z
base: feat/sidebar-row-gap
head: fix-status-flicker
url: https://github.com/gerchowl/herdr/pull/20
---

# fix(sidebar): stop status row flickering between spinner text and state label

## Symptom

The sidebar status row flickers rapidly between the live Claude spinner text (e.g. "Cogitating…") and the bare state label while an agent is actively working — "flickering between ours and the original."

## Root cause

The status row renders `<agent> · <live_activity|label> [· <custom_status>]`, showing the scraped spinner activity **only while `state == Working`** and falling back to the state label otherwise.

The detector republishes a `StateChanged` event whenever the spinner text changes (`activity_changed = detection.activity != last_activity`). A spinner line caught mid-redraw — or in the gap between two verbs — scrapes as `None`. That `None` was written straight into `terminal.live_activity` (`actions.rs`: `terminal.live_activity = activity.clone()`), clearing it for a frame even though `visible_working`/`state` stayed `Working`. So the row dropped to the state label and bounced back on the next good scrape, every few frames.

Unlike the effective-state arbitration (which already has a 1200 ms working-hold and `visible_working_overrides_hook` damping), the `live_activity` write had **no** stickiness.

## Fix

`TerminalState::update_live_activity(activity, detected_state)` holds the last scraped activity through transient `None` misses while the agent is still working, clearing only once it genuinely leaves the working state. Also reset `live_activity` on agent respawn so stale text can't leak across sessions.

The handler runs in `handle_app_event` — the shared client/headless chokepoint — so one change covers both render loops.

## Tests

`live_activity_survives_transient_scrape_miss_while_working` covers the round-trip (hold on transient None while Working, replace on fresh scrape, clear on Idle). Full suite green (1888).
