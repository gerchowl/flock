---
number: 129
title: "herdr-web: CLI-friendly on-screen key bar (Esc/Tab/Ctrl/arrows/pipe/prefix)"
kind: issue
state: CLOSED
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:29:13Z
closed: 2026-06-15T14:25:37Z
url: https://github.com/gerchowl/herdr/issues/129
---

# herdr-web: CLI-friendly on-screen key bar (Esc/Tab/Ctrl/arrows/pipe/prefix)

On a phone the web view relies on the iOS soft keyboard (`pkgs/herdr-web/static/index.html:131` taps `term.focus()`), which has no Esc, Tab, Ctrl, Alt, arrows, `|`, `/`, `-`, or a way to send the herdr prefix chord — so most of herdr (and any CLI/agent) is unreachable from a phone.

**Ask:** a tap-bar of CLI keys that send the right bytes over the existing WS (the frontend already forwards keystrokes via `term.onData` → `ws.send`):
- Esc `\x1b`, Tab `\t`, Enter, arrows `\x1b[A/B/C/D`
- a **sticky Ctrl** modifier (so Ctrl-C `\x03`, Ctrl-R, etc.), maybe sticky Alt
- common punctuation phones bury: `|` `/` `-` `~` `\``
- a one-tap **herdr prefix** button

Pure `index.html` change. This is the unlock for keyboard-driven herdr features (new worktree `prefix+shift+g`, navigate, kill) on a phone — see the parity note in #109.

Relates to #109.
