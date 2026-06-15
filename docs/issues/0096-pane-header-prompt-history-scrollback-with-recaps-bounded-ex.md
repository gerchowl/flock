---
number: 96
title: "pane header: prompt history scrollback with recaps (bounded expandable panel)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T15:15:45Z
closed: 2026-06-12T16:32:54Z
url: https://github.com/gerchowl/herdr/issues/96
---

# pane header: prompt history scrollback with recaps (bounded expandable panel)

## Feature (user): pane-header prompt HISTORY with recaps

Today the header keeps only the LAST prompt; expanding it renders in place with no visible end ("needs the blue bar to see its end"). Upgrade:

1. **History ring per pane terminal**: every `pane.report_prompt` APPENDS (timestamped) instead of overwriting; new optional `pane.report_recap <text>` appends a recap entry (wired from a Claude Stop hook later — the API just stores it). Cap: ~1000 LINES total per pane (drop oldest whole entries); not persisted in snapshots (ephemeral, like header fields #40).
2. **Collapsed (unchanged)**: latest prompt, middle-collapsed to prompt_float_lines.
3. **Expanded (prefix+shift+e / click) becomes a BOUNDED scrollable panel** over the pane: chronological, history above, the LATEST entry pinned at the bottom (if the latest is <3 lines it sits as the last line(s) with history directly above); wheel + PageUp/Down scroll; Esc/again-key closes. Panel has a visible border/edges — the "where does it end" fix. Recaps render visually distinct from prompts (muted/prefixed).
4. Entry chrome: relative timestamp + kind marker per entry, subtle.

## Constraints
- update_terminal_state chokepoint (both event loops — zero new request_* fields), same dual-loop discipline as #40.
- RPC additive; cap enforced at append; report_recap documented in pane --help + socket-api docs like #40 did.

## References
#24 (prompt expand), #40 (header fields precedent: RPC->state->render + caps + TTL discipline), prompt_float_lines, src/ui/panes.rs in-place expansion.

---

## Comments

### gerchowl — 2026-06-12T16:32:53Z

Shipped in PR #98: per-pane prompt+recap history (1000-line ring, ephemeral), pane.report_recap RPC + CLI, bordered scrollable panel pinned to the latest entry, wheel/PageUp/Esc, collapsed header byte-identical. Wire a Claude Stop hook at report-recap whenever you want recaps flowing.

