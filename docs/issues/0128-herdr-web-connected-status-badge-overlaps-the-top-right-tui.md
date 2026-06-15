---
number: 128
title: "herdr-web: 'connected' status badge overlaps the top-right TUI (server/repo picker)"
kind: issue
state: CLOSED
author: gerchowl
labels: ["bug"]
created: 2026-06-14T12:29:12Z
closed: 2026-06-15T14:25:37Z
url: https://github.com/gerchowl/herdr/issues/128
---

# herdr-web: 'connected' status badge overlaps the top-right TUI (server/repo picker)

The web view's status badge is an HTML overlay pinned to the top-right *on top of* the terminal:

```css
#status { position: fixed; top: 8px; right: 12px; ... z-index: 10; }
```
(`pkgs/herdr-web/static/index.html:35-39`, in g-fleet)

herdr draws the server/repo header / sidebar in that same top-right region, so the badge covers it. It also never moves or hides after connecting — it sits there reading "connected" permanently.

**Fix options:** auto-hide/fade the badge a couple seconds after `connected` (keep it only for `connecting…`/`disconnected`/`error`), and/or move it to a corner herdr doesn't paint content in. Frontend-only (`index.html`).

Relates to the web MVP, #109.
