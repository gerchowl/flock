---
number: 86
title: "Federation v2 + fork feature train → master"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T09:49:44Z
closed: 2026-06-12T09:49:52Z
merged: 2026-06-12T09:49:52Z
base: master
head: feat/sidebar-row-gap
url: https://github.com/gerchowl/herdr/pull/86
---

# Federation v2 + fork feature train → master

Graduates the long-lived integration branch to master per the new workflow (worktrees + branches against master from here on). Carries the entire fork arc: workspace-as-unit, float pane, federation (hub-spoke → origin gossip → connection slots, proto 18), sidebar restyle v3, state-language SSoT, guardrails gates, sccache devShell. Tracking: #68 (closed).
