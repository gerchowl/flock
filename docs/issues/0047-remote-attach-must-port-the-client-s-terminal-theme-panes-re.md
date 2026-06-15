---
number: 47
title: "remote attach must port the client's terminal theme (panes render pitch-black on spokes)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T08:46:14Z
closed: 2026-06-11T11:53:33Z
url: https://github.com/gerchowl/herdr/issues/47
---

# remote attach must port the client's terminal theme (panes render pitch-black on spokes)

## Bug (live)

Switching to a spoke, the working pane renders a pitch-black background — the spoke's PTYs were spawned with the SPOKE server's host_terminal_theme (a headless server has no/default theme), and the attaching client's theme (alacritty bg, forwarded locally via the OSC-11 capture) never crosses the wire.

## Fix sketch
- Carry the client's captured TerminalTheme (fg/bg) in the attach handshake (Hello — additive next to the #39 fleet field; protocol just bumped to 14, assess whether another additive field needs 15 or rides 14 since 14 is hours old and fleet-deployed by us only — bump if strictness demands, we control all deployments).
- The receiving server: (a) uses it as host_terminal_theme for NEW panes, (b) re-broadcasts OSC 11/10 default-color updates to EXISTING pane PTYs (osc_set_default_color_sequence exists in terminal_theme.rs) so running agents repaint/re-detect.
- Multiple clients with different themes: last-attach-wins is acceptable v1 (single-user fleet).

## References
host_terminal_theme plumbing (terminal/runtime.rs), OSC capture (raw_input.rs \x1b]10/11), theme forwarding at spawn (pane.rs), #39 handshake.

---

## Comments

### gerchowl — 2026-06-11T11:53:32Z

Shipped in PR #52 (protocol 15).

