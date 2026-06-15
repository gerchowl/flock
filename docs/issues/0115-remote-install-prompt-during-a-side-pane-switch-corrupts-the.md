---
number: 115
title: "remote-install prompt during a side-pane switch corrupts the terminal (must fail-with-notice, never prompt)"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T21:43:20Z
closed: 2026-06-13T21:56:24Z
url: https://github.com/gerchowl/herdr/issues/115
---

# remote-install prompt during a side-pane switch corrupts the terminal (must fail-with-notice, never prompt)

## Bug (live, dangerous): remote-install prompt during a side-pane switch corrupts/kills the terminal

Switching to a stale/mismatched remote (anvil-dev: PATH herdr is base 0.6.8, not the fork) from the side pane triggers herdr's remote-bootstrap install prompt ("want to install? y/n"). That interactive prompt, in the switch/bridge context, KILLS the terminal tab and leaves it unusable afterward (raw mode / alt-screen not restored -- the #69/#72 restore class, here on the remote-bootstrap path).

Two faults:
1. **A SWITCH should never block on an interactive install prompt.** prepare_remote_herdr / ensure_remote_server_ready prompts when the remote binary mismatches. In the federation side-pane switch path this is hostile: the switch should FAIL GRACEFULLY with the top-right notice (#67 failure path) -- e.g. "anvil-dev: herdr mismatch (base 0.6.8 vs fork). Run `just apply anvil`." -- not prompt, not block, not corrupt.
2. **Even if it prompts, the terminal MUST be restored on every exit** (the #72 HeldRestoreGuard discipline). The tab being "unusable afterwards" means an exit path skipped restore.

## Fix
- In the switch/bridge attach path, make the remote-readiness check NON-interactive: on mismatch/missing, return a ClientError that rides the switch-failure notice rail (#67), never an interactive prompt. The interactive `--remote` install prompt is fine for an EXPLICIT `herdr --remote <host>` from a shell, but not for a federation switch.
- Audit the remote-bootstrap exit paths for terminal restore (try_init/HeldRestoreGuard coverage, #69/#72) -- the prompt flow corrupting the tab is a restore gap.

## Acceptance
- Switching to a version-mismatched remote shows a top-right failure notice and stays on the current server; the terminal is never corrupted.
- An explicit `herdr --remote <host>` (shell) keeps its interactive install prompt, with terminal restore on exit.

## References
prepare_remote_herdr / ensure_remote_server_ready (src/remote.rs), the switch/SwitchServer path (#67 failure notice), #69/#72 (terminal restore / try_init), #102 (switch robustness).
