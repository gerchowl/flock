---
number: 82
title: "fix(terminal): re-assert pane geometry on attach/switch; scrollback reflow (#77)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T08:53:41Z
closed: 2026-06-12T09:05:30Z
merged: 2026-06-12T09:05:30Z
base: feat/sidebar-row-gap
head: geometry-reflow
url: https://github.com/gerchowl/herdr/pull/82
---

# fix(terminal): re-assert pane geometry on attach/switch; scrollback reflow (#77)

Fixes #77.

## Root cause, per layer

**Layer 1 — stale geometry on server switch (the real bug).**
A warm connection slot (#76) only learns this client's terminal size at *dial time* (the Hello cols/rows passed into the throttled warm-all sweep). Subsequent `Resize` events reach only the *then-active* write stream, so a paused warm slot's server keeps the stale dial-time width. On an in-process switch, the flip resumed the slot and asked for a full redraw — which repaints the **frame** — but never re-sent geometry, so every pane's PTY+VT on the newly-active server kept the stale narrow width and fresh output wrapped at it until a manual nudge. (Attach and handoff were already fine: `ClientConnected` runs `resize_shared_runtime_to_effective_size`, which resizes the active workspace *and* every background tab/workspace via `resize_background_tab_panes_to_terminal_area`.)

**Fix:** carry the last reported cell-pixel size in `ClientState` alongside cols/rows, and on a successful flip send a fresh `ClientMessage::Resize` with the client's current geometry to the just-activated slot. The server's `Resize` handler then resizes every pane's PTY+VT to the true size.

**Layer 2 — scrollback reflow: no code change needed.**
The vendored libghostty-vt exposes exactly one resize entry point to C/Rust — `ghostty_terminal_resize` — and it **already routes through the reflowing path**: `Terminal.resize` → `Screen.resize`/`PageList.resize` with `reflow` gated on the `wraparound` mode (which defaults **on**). The internal `resizeWithoutReflow` is never reached from the C or Rust layer, so there is no naive grid resize to swap out. The "history stays wrapped at the old width" symptom was a *downstream effect of layer 1*: the server never received the new width on switch, so `resize()` — and therefore reflow — never ran for those panes. With layer 1 re-asserting geometry, scrollback reflow runs automatically.

## What the vendored vt actually exposes for reflow
- C API: a single `ghostty_terminal_resize(t, cols, rows, cell_w, cell_h)` (`vendor/libghostty-vt/src/terminal/c/terminal.zig`), which calls `Terminal.resize` → `Screen.resize` (full cursor-pin-tracking, hyperlink-reflow, prompt-redraw implementation) → reflowing `PageList.resize`, including scrollback. `reflow = self.modes.get(.wraparound)`, default true.
- Rust binding: `Terminal::resize` in `src/ghostty/bindings.rs` / `src/ghostty/mod.rs` wraps it directly. No follow-up upstream bump is required.

## Commits
1. `fix(terminal): re-assert pane geometry to the slot activated on server switch (#77)` — client geometry carry + flip-time `Resize` + channel-runtime e2e.
2. `test(terminal): lock scrollback reflow-on-resize through the vendored vt (#77)` — regression test pinning reflow at the binding boundary.

## Tests (devShell, `--test-threads=4`)
- New layer-1 e2e `resume_reasserts_geometry_so_panes_render_at_new_width` (`client_mode`): dial at width A → pause (warm) → grow to width B while paused → resume + re-assert → next frame renders at width B.
- New layer-2 `resize_reflows_soft_wrapped_scrollback_to_new_width` (`ghostty::tests`): soft-wrapped scrollback collapses to fewer rows and reads back as one unwrapped logical line after a widen.
- Suites: `client_mode` 19/19 · `live_handoff` 16/16 · `peer_federation` 7/7 · `server_headless` 16/16 · ghostty+client unit 87/87. rustfmt + clippy `-D warnings` clean.
