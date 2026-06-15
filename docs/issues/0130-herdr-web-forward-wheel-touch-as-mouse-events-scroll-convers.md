---
number: 130
title: "herdr-web: forward wheel/touch as mouse events (scroll conversation + right-click parity)"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:29:14Z
closed: 2026-06-15T14:25:38Z
url: https://github.com/gerchowl/herdr/issues/130
---

# herdr-web: forward wheel/touch as mouse events (scroll conversation + right-click parity)

The web frontend forwards **only keystrokes** (`pkgs/herdr-web/static/index.html:112` `term.onData` → `ws.send`); it never forwards mouse/wheel/touch.

Two consequences:
1. **No scrolling the conversation.** xterm.js here isn't a scrollback buffer — herdr repaints a fixed TUI viewport as ANSI, so xterm's own buffer has nothing to scroll. Scrolling must drive **herdr's pane scrollback**, which means forwarding wheel/touch from the browser to herdr (as mouse-wheel events or herdr's scroll keybinding).
2. **No right-click menu** (new-worktree / branch-session / kill all hang off the sidebar right-click — `src/app/input/mouse.rs:1196`), and no drag text-selection.

**Ask:** capture wheel + touch gestures in xterm.js and forward them as the mouse events herdr understands (enable xterm mouse reporting / send the wheel escape sequences). On a phone: two-finger or edge drag → scroll; long-press → right-click. This is what brings the web view to **mouse-feature parity** with the TUI.

Relates to #109. Pairs with the on-screen key bar for full parity.
