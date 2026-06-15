---
number: 157
title: "ci(latest-branch): trigger on master + build herdr-web"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T15:47:22Z
closed: 2026-06-15T15:47:30Z
merged: 2026-06-15T15:47:30Z
base: master
head: fix/latest-branch-trigger
url: https://github.com/gerchowl/herdr/pull/157
---

# ci(latest-branch): trigger on master + build herdr-web

The integration branch graduated to `master` (PR #86), but `latest-branch.yml` still triggered on `feat/sidebar-row-gap` — so the `latest` channel stopped auto-tracking master and went stale (stuck at an old master). Point the trigger at `master`, and build `.#herdr-web` alongside `.#default` so `latest` only advances when both build green.
