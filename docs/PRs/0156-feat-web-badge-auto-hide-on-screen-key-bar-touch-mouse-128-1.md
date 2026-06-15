---
number: 156
title: "feat(web): badge auto-hide, on-screen key bar, touch→mouse (#128 #129 #130)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T14:25:23Z
closed: 2026-06-15T14:25:36Z
merged: 2026-06-15T14:25:36Z
base: master
head: feat/web-ui-parity
url: https://github.com/gerchowl/herdr/pull/156
---

# feat(web): badge auto-hide, on-screen key bar, touch→mouse (#128 #129 #130)

Closes #128, #129, #130. Frontend parity for the in-tree web page (`assets/web/index.html`); builds on `herdr web` (#131).

## #128 — status badge auto-hide
The badge fades out 2s after `connected` (stays visible for connecting/disconnected/error), so it no longer permanently covers herdr's top-right server/repo header.

## #129 — on-screen CLI key bar
A bottom tap-bar emitting the bytes a phone soft keyboard can't: Esc, Tab, **sticky Ctrl** (rewrites the next typed char → control code), arrows, `|` `/` `-` `~` `` ` ``, and a one-tap **herdr prefix** (ctrl+space → NUL). This is the unlock for keyboard-driven herdr (new worktree, navigate, kill) from a phone.

## #130 — mouse/touch
Desktop mouse + wheel **already forward** via xterm's native mouse handling — the herdr client runs `EnableMouseCapture` (`src/client/mod.rs:449`) over the same stream, and `term.onData` ships xterm's mouse reports. This adds **touch→mouse for phones**: vertical drag → wheel (SGR 1006, scrolls herdr's pane), long-press → right-click (sidebar menu). Guarded by `term.modes.mouseTrackingMode` so nothing is synthesized when mouse reporting is off (no stray escape sequences as keystrokes).

## Verification
Loaded the built binary's embedded page in a headless browser (Playwright): **no JS errors**, key bar renders, layout correct (terminal above the bar), badge auto-hide logic confirmed. ⚠️ **Touch gestures need on-device verification** — synthesized but not exercised on a real touchscreen here.

Refs #131.
