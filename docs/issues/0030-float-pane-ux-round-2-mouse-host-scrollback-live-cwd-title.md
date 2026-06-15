---
number: 30
title: "float pane UX round 2: mouse, host scrollback, live cwd title"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-10T17:07:05Z
closed: 2026-06-11T11:53:19Z
url: https://github.com/gerchowl/herdr/issues/30
---

# float pane UX round 2: mouse, host scrollback, live cwd title

Follow-up to #25 / PR #28 (known P1 limitations, noted in the PR body).

- Mouse events aren't float-aware: clicks/scroll inside the float rect route to the layout pane underneath.
- Shift+PageUp host-scrollback isn't float-aware.
- The float border title shows the spawn cwd, not the live foreground cwd.
- Consider: copy-mode parity inside the float.

All scoped to the float overlay path (src/app/float.rs, src/ui/float.rs, input seams) — no layout/persistence surface.

---

## Comments

### gerchowl — 2026-06-11T11:53:18Z

Shipped in PR #49.

