---
number: 23
title: "feat(scroll): Shift+PageUp/Down/Home/End + Shift+wheel scroll herdr scrollback"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T12:56:47Z
closed: 2026-06-10T13:26:27Z
merged: 2026-06-10T13:26:27Z
base: feat/sidebar-row-gap
head: feat/scrollback-keys
url: https://github.com/gerchowl/herdr/pull/23
---

# feat(scroll): Shift+PageUp/Down/Home/End + Shift+wheel scroll herdr scrollback

## What
Fixes the **"scroll gets stuck"** feeling in agent chats / full-screen apps, with a deterministic keyboard + Shift-wheel scroll path.

## Why it felt stuck (not the mouse)
herdr decides per wheel-notch (`pane.rs:wheel_routing`): if the focused app enables **mouse reporting** or **alternate-scroll** (Claude's TUI runs on the alt screen), herdr forwards the wheel to the app and **pins its own scrollback to the bottom** (`scroll_reset()`). So scrolling becomes the app's job — when it's at a boundary or ignores the event, nothing moves → reads as stuck. Switching agents forces a fresh render pinned to bottom → the "unsticks / flushes to bottom" symptom. An MX Master's free-spin just floods more events; a trackpad does the same. Herdr-side behavior, not hardware.

## Fix — app-independent scroll
- **Shift+PageUp / PageDown** page herdr's own scrollback in the focused pane; **Shift+Home / End** jump to top-of-history / live bottom. Bare keys still pass to the app. Intercepted in the shared `prepare_terminal_key_forward` chokepoint (App + headless loops).
- **Shift+wheel** always host-scrolls, even under mouse capture — a mouse escape hatch matching the keys.

Terminal-standard (gnome-terminal/kitty/iTerm). Complements the **pre-existing** bare-PageUp scrollback that already works in plain-shell panes — this is the missing half for alt-screen / mouse-reporting panes, exactly where it stuck.

## Tests
- Shift+PageUp scrolls + pages further + Shift+End returns to bottom
- Shift+Home → top (offset == max)
- bare PageUp forwards to a mouse-reporting app while Shift+PageUp still scrolls
- Shift+wheel scrolls under mouse capture (DECSET 1000)

Full suite **1901 passed / 0 failed**; guardrails gates (fmt, clippy, no-fake-impl, no-commented-code, secrets) all pass.

## Caveat
A few host terminals bind Shift+PageUp to their own scrollback and may not forward it in alt-screen — if so, rebind to a prefix chord.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
