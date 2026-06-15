---
number: 11
title: "fix(agents): scoped-attention fallback cycles all workspace panes"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T19:46:43Z
closed: 2026-06-06T19:46:57Z
merged: 2026-06-06T19:46:57Z
base: feat/sidebar-row-gap
head: fix/scoped-attention-fallback
url: https://github.com/gerchowl/herdr/pull/11
---

# fix(agents): scoped-attention fallback cycles all workspace panes

User-reported: ctrl+shift+s no-ops. Sandbox repro with raw kitty CSI-u bytes proved dispatch fires (debug log: action=FocusAttentionWorkspace) but pane_details' agent-label filter left nothing to cycle. Now cycles all panes of the active workspace across tabs. +1 regression test (shell-only cycling), 1858/1858.
