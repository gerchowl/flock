---
number: 29
title: "workspace mode: group-local tab-switching keys (prefix+1..9 / next_tab)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-10T17:07:04Z
closed: 2026-06-11T11:53:17Z
url: https://github.com/gerchowl/herdr/issues/29
---

# workspace mode: group-local tab-switching keys (prefix+1..9 / next_tab)

Follow-up to #25 (deliberately deferred — the review panel split on it).

**Motivation:** with `tab_mode = "workspace"`, muscle memory says prefix+2 = "second sibling of *this* space" (tmux window numbers are session-local for the same reason). Today the tab keys still act on tabs (mostly single-tab in this mode) and sibling navigation needs SwitchWorkspace/NextWorkspace.

**Contested:** multiplexer-architect review wants group-local rerouting; fork-strategist review vetoes touching the navigate.rs dispatch match (upstream's hottest conflict surface — 3 keybind features in 30 days, #224 churned cycling semantics).

**Proposed synthesis:** keep every dispatch arm byte-identical; branch inside the *called state method* (fork-owned actions.rs body): in workspace mode with a single-tab workspace, switch_tab(n)/next_tab cycle the space group's workspaces in sidebar visual order.

**Acceptance:** dispatch match untouched vs upstream; group-local cycling test pinned to sidebar visual order; tabs-mode behavior byte-identical.

---

## Comments

### gerchowl — 2026-06-11T08:01:52Z

Likely resolved structurally by #33's completion (see latest comment there): the tab bar becomes the session-member switcher in tab_mode=workspace, so prefix+1..9 switches members via unchanged dispatch — the strip's content changes, not the key routing. Keep open until #33 lands.

### gerchowl — 2026-06-11T11:53:16Z

Resolved structurally by #33's member tab-strip (in flight): prefix+1..9 switches the strip's session members in workspace mode via state-method branching — the navigate.rs dispatch stays untouched, satisfying both reviews.

