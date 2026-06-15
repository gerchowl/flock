---
number: 88
title: "float pane: hide on blur (click outside); scrollback doesn't render inside the float"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T12:18:43Z
closed: 2026-06-12T12:28:14Z
url: https://github.com/gerchowl/herdr/issues/88
---

# float pane: hide on blur (click outside); scrollback doesn't render inside the float

## Live feedback (post-#49)

1. **Blur-hide**: clicking OUTSIDE the visible float currently falls through to the layout panes (#49 route-through). Better: the first click outside HIDES the float (zellij-style blur dismiss) — the click is consumed, a second click acts normally. Esc stays with the float's PTY (apps need it — vim/fzf in a float would break); prefix+f remains the keyboard hide.
2. **Scroll**: the float isn't scrollable — wheel routes to the float runtime (#49) but host-scrollback (wheel fallback + shift+PageUp targeting from #49) doesn't visibly scroll the float's content. Suspect: the float render path doesn't render the runtime's scrollback offset the way pane render does (or the scrolled-back state isn't consulted). Compare render_float vs the pane render's scrolled-back handling (pane_is_scrolled_back / scrollbar plumbing).

## References
PR #28 (float core), PR #49 (#30 round 2: mouse routing, scrollback targeting, live title), src/ui/float.rs, src/app/input/mouse.rs handle_float_mouse.

---

## Comments

### gerchowl — 2026-06-12T12:28:13Z

Shipped: blur dismiss + the root cause of 'not scrollable' was PR #49's commit missing from master's lineage — restored via cherry-pick. Esc deliberately stays with the float's PTY (vim/fzf need it); prefix+f and blur are the dismissals.

