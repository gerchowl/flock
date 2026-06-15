---
number: 71
title: "fix(input): map kitty keypad functional codes — numpad works again (#70)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T21:23:27Z
closed: 2026-06-11T21:24:02Z
merged: 2026-06-11T21:24:02Z
base: feat/sidebar-row-gap
head: numpad-fix
url: https://github.com/gerchowl/herdr/pull/71
---

# fix(input): map kitty keypad functional codes — numpad works again (#70)

Fixes #70 — the numpad emitted nothing inside herdr (top-row digits worked).

## Where they died
Once the client pushes kitty keyboard-enhancement flags on the host terminal, Alacritty/Ghostty report numpad keys as distinct CSI-u functional codepoints (KP_0..KP_9 = 57399.., KP_DECIMAL/DIVIDE/MULTIPLY/SUBTRACT/ADD = 57409..57413, KP_EQUAL = 57415, KP_SEPARATOR = 57416). In `src/input/parse.rs`, `kitty_codepoint_to_keycode` had no arm for these, so its catch-all returned `None`, `parse_kitty_key_sequence` failed, and `extract_one_event` turned the whole sequence into `RawInputEvent::Unsupported` — the key was dropped before ever reaching a pane PTY. (Navigation keypad codes 57417..57427 and KP_ENTER 57414 were already mapped, which is why arrows/Enter on the numpad were unaffected.)

## Fix
- Parse: map keypad codepoints to their char/key equivalents so keybind matching and legacy panes behave exactly like the top-row key. The native keypad codepoint is preserved on a new `TerminalKey::keypad_codepoint` field.
- Encode: for a pane that has itself negotiated kitty keyboard, re-emit the native keypad CSI-u code so apps that legitimately distinguish the keypad still can. Legacy/non-kitty panes get the collapsed char/key.

## Mapping table

| codepoint | key | equivalent (legacy pane) | kitty pane |
|---|---|---|---|
| 57399..57408 | KP_0..KP_9 | `0`..`9` | native `\e[5739x;..u` |
| 57409 | KP_DECIMAL | `.` | native |
| 57410 | KP_DIVIDE | `/` | native |
| 57411 | KP_MULTIPLY | `*` | native |
| 57412 | KP_SUBTRACT | `-` | native |
| 57413 | KP_ADD | `+` | native |
| 57414 | KP_ENTER | Enter (`\r`) | native |
| 57415 | KP_EQUAL | `=` | native |
| 57416 | KP_SEPARATOR | `,` | native |
| 57417..57427 | KP nav (Left..Begin) | cursor/Home/End/etc. (already mapped) | native code recorded |

## Tests
- parse: keypad digits/operators → correct `Char`, native `keypad_codepoint` recorded; KP_ENTER → Enter; nav keys keep native code.
- encode: legacy pane KP_5 → `5`, KP_ENTER → `\r`, operators → their chars; kitty pane KP_5 → `\e[57404;1u` (and `:1u` event-type when requested).
- PTY-level via `GhosttyPaneTerminal::encode_terminal_key`: legacy pane KP_5 → `5`, KP_ENTER → `\r`; kitty-negotiated pane → native `\e[57404;1u`.
- keybind: synthesized numpad-2 CSI-u triggers an indexed `'2'` binding identically to top-row 2.
- Added 9 keypad rows to `keyboard_protocol_corpus.tsv` (exercised by both the parse and raw-input fixture suites).

Full suite green (2084 passed); fmt + clippy (`-D warnings`) clean.
