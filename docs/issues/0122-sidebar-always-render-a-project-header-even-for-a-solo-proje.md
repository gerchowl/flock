---
number: 122
title: "sidebar: always render a project header, even for a solo project"
kind: issue
state: OPEN
author: gerchowl
labels: ["enhancement"]
created: 2026-06-14T12:13:30Z
closed: 
url: https://github.com/gerchowl/herdr/issues/122
---

# sidebar: always render a project header, even for a solo project

Today a project is only shown as a collapsible group with a header when it has **≥2 members**; a single checkout renders as one flat `owner/repo · server:branch` row (`solo_local_label`, `src/ui/grammar.rs:104`; collapsibility gated in `src/app/actions.rs:993` `collapsible_space_keys`).

**Ask:** always render the project as a header row with its member(s) underneath, even with a single checkout, so the organization model is uniform — one project or many, it always lists the same way.

Part of milestone: Fleet project view + cross-machine worktrees.
