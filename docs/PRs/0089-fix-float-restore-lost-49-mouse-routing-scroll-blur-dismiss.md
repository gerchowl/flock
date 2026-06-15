---
number: 89
title: "fix(float): restore lost #49 (mouse routing/scroll) + blur dismiss (#88)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T12:28:07Z
closed: 2026-06-12T12:28:11Z
merged: 2026-06-12T12:28:11Z
base: master
head: float-blur
url: https://github.com/gerchowl/herdr/pull/89
---

# fix(float): restore lost #49 (mouse routing/scroll) + blur dismiss (#88)

Two commits: (1) cherry-pick of `289d383` — PR #49's float mouse routing / host scrollback / live title was **missing from master's lineage** (lineage audit: only this commit was dropped; b68e426/1fbd90f/8eebc0d/94f49f7 all present) — the user's 'float not scrollable' was this; (2) **blur dismiss** per #88: first click outside hides the float (consumed), second acts normally; Esc stays with the PTY. 2139/0, clippy clean.
