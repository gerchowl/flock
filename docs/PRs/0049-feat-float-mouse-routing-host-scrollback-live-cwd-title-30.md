---
number: 49
title: "feat(float): mouse routing, host scrollback, live cwd title (#30)"
kind: pr
state: OPEN
author: gerchowl
labels: []
created: 2026-06-11T09:32:59Z
closed: 
merged: 
base: feat/sidebar-row-gap
head: float-ux2
url: https://github.com/gerchowl/herdr/pull/49
---

# feat(float): mouse routing, host scrollback, live cwd title (#30)

Implements #30 (float pane UX round 2), follow-up to #28.

## Mouse events are float-aware
- While the active workspace's float is visible, mouse events whose coordinates fall inside the overlay rect route to the float's runtime: clicks/drags/motion are SGR-encoded against the overlay's **inner** rect, wheel honors the float app's wheel routing (mouse-report / alternate-scroll) and otherwise falls back to the float's host scrollback. Shift+wheel keeps the same deterministic host-scrollback escape hatch panes have.
- Border hits are swallowed so nothing leaks to the panes underneath; events **outside** the overlay still reach the layout panes unchanged (route-through only, no dismiss-on-outside-click).
- Hooked at both entry points: the full mouse path (`App::handle_mouse`, ahead of the URL-click/double-click pre-hooks) and the capture-off path (`AppState::handle_pane_mouse_only`). Selection/drag gestures anchored outside the float keep the normal path so dragging across the overlay can't hijack them.
- The overlay rect is derived at routing time from `state.view.terminal_area` via the same `float_overlay_rect`/`float_overlay_inner_rect` functions the renderer uses — no second geometry source to drift.
- Mouse-capture negotiation follows the input owner: a float app that enables mouse reporting now triggers host mouse capture (`focused_pane_requests_mouse_capture_from` → new `AppState::terminal_input_runtime_from`, also reused by `App::terminal_input_runtime`).

## Shift+PageUp host scrollback works in the float
- The float early-hook in `prepare_terminal_key_forward` now handles Shift+PageUp/PageDown/Home/End before encoding keys into the float's PTY.
- The scrollback helpers (`scroll_focused_pane_page` / `scroll_focused_pane_edge`) resolve the terminal-input owner — visible float first, focused layout pane otherwise — so the same path serves both. Hidden floats yield the keys back to the focused pane.

## Live cwd title
- The float border title now shows the PTY's live foreground cwd (`rt.foreground_cwd()` falling back to `rt.cwd()`, the same machinery `Tab::cwd_for_pane`/`foreground_cwd_for_pane` use), with the spawn cwd as the final fallback. Title formatting extracted into a pure, tested function.

## Cleanups
- Extracted runtime-level mouse forwarding helpers (`forward_runtime_mouse_button`/`_motion`/`_wheel`) shared by the pane and float paths.
- Shared the `app_with_visible_float` channel-runtime test fixture between the terminal and mouse test modules.
- Docs: extended the `toggle_float` paragraph in `configuration.mdx`.

Deliberately out of scope (issue's "consider"): copy-mode parity inside the float.

## Tests
- `mouse_inside_float_rect_routes_to_float_and_outside_to_panes` — byte-level routing for click/wheel in, click out.
- `float_border_clicks_are_swallowed_and_never_reach_the_pane_underneath`
- `wheel_inside_float_scrolls_the_float_host_scrollback`
- `shift_scrollback_keys_target_the_float_host_scrollback_while_visible` — visible → float pages/jumps; hidden → focused pane pages.
- `title_prefers_live_foreground_cwd_over_spawn_cwd`, `title_falls_back_to_spawn_cwd_then_to_a_static_label`, `title_truncation_keeps_the_path_tail`

Full suite: 1953 passed, 0 failed. fmt + clippy (`--all-targets`) clean.
