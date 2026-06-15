---
number: 24
title: "feat(hud): stable hostname, header divider fix, prompt-expand keybind + in-place expand"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T14:46:03Z
closed: 2026-06-10T15:46:54Z
merged: 2026-06-10T15:46:54Z
base: feat/sidebar-row-gap
head: hud-polish
url: https://github.com/gerchowl/herdr/pull/24
---

# feat(hud): stable hostname, header divider fix, prompt-expand keybind + in-place expand

Four pane-header/HUD fixes (a fifth — a lighter main-pane grey — is being handled separately pending a color pick).

1. **Hostname → LocalHostName.** The status-line hostname used `sysinfo::host_name()`, which on corp/campus DHCP (ETH `staff-net-*.intern.ethz.ch`) is an unstable name. `short_host_name()` now prefers the stable macOS `LocalHostName` (cached, session-stable), backing both the status line and peer identity.
2. **Divider ↔ last-prompt collision.** The header hairline divider rendered on the same row as the last prompt line — the prompt section now reserves *both* the context row above and the divider row below (`prompt_rows = height - 2`).
3. **Prompt-expand keybind.** New configurable `keys.toggle_prompt_expand` — the keyboard twin of clicking the pane header to toggle the full last-prompt view (focused pane).
4. **Expand replaces in place.** The expanded prompt now *replaces* the collapsed header at the same anchor, in header colors, extending downward — instead of floating below the still-visible minimized header in bright text.

Tests: `toggle_focused_prompt_expand` round-trip; build/clippy/fmt clean; 119 header/nav/prompt tests pass. The divider + in-place-expand are visual — worth an eyeball once deployed.
