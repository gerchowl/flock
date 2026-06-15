---
number: 77
title: "stale pane geometry after restart/switch + scrollback doesn't reflow (we ship libghostty-vt — use its reflow)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T08:19:38Z
closed: 2026-06-12T09:05:38Z
url: https://github.com/gerchowl/herdr/issues/77
---

# stale pane geometry after restart/switch + scrollback doesn't reflow (we ship libghostty-vt — use its reflow)

## Bug (live, post-fd0dd50)

After restart / server switches — especially when scrolling up — panes render hard-wrapped at a stale NARROW width (screenshot: content wrapped to ~25 cols in a full-width pane; fresh and historical lines both affected at first, history stays broken).

Two distinct layers:

1. **Resize propagation miss**: after attach/handoff/switch the PTY/VT keeps a stale geometry — fresh output wraps narrow until something forces a resize. Suspects: the attach-time geometry broadcast (does every pane's PTY get SIGWINCH + VT resize, or only the focused workspace?), the handoff import path, and the new slot resume (#76 full-redraw repaints the FRAME but does the server re-assert pane geometry?). The reproduction correlates with restart/switch.
2. **Scrollback reflow**: scrolled-up history stays wrapped at the old width even after live content recovers. We embed libghostty-vt — ghostty's core SUPPORTS reflow-on-resize (the celebrated implementation). Investigate whether our resize call goes through the reflowing path (screen/pagelist resize w/ reflow) or a naive grid resize, and what the zig API exposes in our vendored version.

## Acceptance
- After any attach/handoff/switch, every pane renders at the true geometry without manual nudging (e2e: attach at width A, reattach at width B, assert fresh output wraps at B for ALL panes incl. unfocused).
- Scrollback reflows on width change if the vendored libghostty-vt exposes it (separate commit; if the API isn't exposed in our pin, document + file the upstream-bump follow-up instead of hacking it).

## References
src/terminal/runtime.rs (VT resize), attach/geometry plumbing (Hello cols/rows → update_terminal_state), PR #76 resume path, zig/libghostty-vt bindings.

---

## Comments

### gerchowl — 2026-06-12T09:05:38Z

Shipped in PR #82: slot-flip now re-asserts geometry (the real bug — warm slots kept dial-time width); reflow was ALREADY wired (libghostty-vt's only resize entry point reflows scrollback, wraparound-aware) — regression-tested, no pin bump needed.

