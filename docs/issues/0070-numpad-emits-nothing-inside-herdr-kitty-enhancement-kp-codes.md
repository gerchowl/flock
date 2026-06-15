---
number: 70
title: "numpad emits nothing inside herdr (kitty enhancement: KP_* codes dropped)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T20:52:42Z
closed: 2026-06-11T21:24:07Z
url: https://github.com/gerchowl/herdr/issues/70
---

# numpad emits nothing inside herdr (kitty enhancement: KP_* codes dropped)

## Bug (live)

Inside herdr, the **numpad emits nothing** — numpad digits/operators never reach the pane's PTY. Plain top-row digits work.

## Likely mechanism
The client pushes kitty keyboard-enhancement flags on the host terminal (ime_compatible_keyboard_enhancement_flags, src/main.rs ~698). Under the kitty protocol, Alacritty reports numpad keys as DISTINCT CSI-u codepoints (57399..57425: KP_0..KP_9, KP_ENTER, operators) instead of plain digit chars. Suspects, in order:
1. The client's raw-input parser / key forwarder doesn't map KP_* functional codes → drops them instead of falling back to their char equivalents.
2. The server-side PTY encoder (kitty CSI-u encode for panes) doesn't translate KP codes for non-kitty-aware pane apps.

## Fix
Map keypad functional codepoints to their char equivalents wherever an app/pane hasn't negotiated kitty keyboard itself (digits, enter, + - * / . =); pass through natively when the pane HAS negotiated kitty (it can tell keypad apart legitimately). Tests: synthesized CSI-u KP_5 press reaches the PTY as "5" (legacy pane) and as the KP code (kitty pane).

## References
src/raw_input.rs (CSI-u parse), src/input.rs (enhancement flags), pane PTY encode path; kitty keyboard protocol spec functional-key table.

---

## Comments

### gerchowl — 2026-06-11T21:24:07Z

Shipped in PR #71 — KP codepoints (57399..57416) had no kitty_codepoint_to_keycode arm and died as Unsupported. Now: char-equivalents for legacy panes + native CSI-u for kitty-negotiated panes; keybinds see the equivalents. Numpad-nav distinction on kitty panes flagged as possible follow-up (pre-existing).

