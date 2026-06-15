---
number: 12
title: "feat(agents): project-scoped attention — repo family, agent unit"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T20:09:36Z
closed: 2026-06-06T20:09:54Z
merged: 2026-06-06T20:09:54Z
base: feat/sidebar-row-gap
head: fix/scoped-agent-cycling
url: https://github.com/gerchowl/herdr/pull/12
---

# feat(agents): project-scoped attention — repo family, agent unit

a|q = all agents; s|w = agents within the same project (main + registered worktrees, via repo_group_key). Agent-unit cycling (shells skipped). Renames focus_attention_workspace→focus_attention_project. 1860/1860 at low contention; family-spanning + cross-project-isolation + shell-skip + single-agent-noop tests.
