---
number: 57
title: "follow-ups from #56 review: member-strip overflow test + per-frame section-key memoization"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-11T13:08:51Z
closed: 
url: https://github.com/gerchowl/herdr/issues/57
---

# follow-ups from #56 review: member-strip overflow test + per-frame section-key memoization

Two non-blocking nits from PR #56's post-impl review:
1. **Member-strip overflow coverage**: the `…` ellipsis branches and can_scroll_left/right gating in render_member_strip are untested under forced overflow (many members, narrow width). Add a render test.
2. **Memoize `project_section_keys()` per frame**: sidebar + strip recompute it several times per render (O(workspaces²)-ish on the hot path). Correctness-neutral today; memoize once per frame if sidebars grow.
3. (cosmetic) single-member strip has no render test.
