---
number: 85
title: "space order changes when switching servers — sections need a fleet-stable order"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-12T09:49:31Z
closed: 2026-06-12T13:01:02Z
url: https://github.com/gerchowl/herdr/issues/85
---

# space order changes when switching servers — sections need a fleet-stable order

## Bug (live dogfood)

Switching servers reorders the spaces list: each server renders ITS OWN section order (its local storage order first, then folds), so sage's sidebar ≠ mba22's — disorienting on every leap.

## Fix
Sections sort by a fleet-stable key (project identity, e.g. owner/repo alphabetical; misc last) — identical on every server by construction. Manual/drag order is a VIEWER preference and belongs in the viewer profile (#79) once that lands (carried per-client, applied everywhere); until then the stable sort replaces per-server storage order for SECTIONS (members within a section keep local-first/server order per #62).

## References
#62 (section model), #79 (viewer profile), drag-reorder machinery.

---

## Comments

### gerchowl — 2026-06-12T13:01:01Z

Shipped: identity-sorted sections, drop-indicator boundary clamp, e2e modernization. Manual ordering returns with #79's viewer profile.

