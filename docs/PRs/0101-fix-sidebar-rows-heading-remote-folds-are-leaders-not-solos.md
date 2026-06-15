---
number: 101
title: "fix(sidebar): rows heading remote folds are leaders, not solos"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T22:20:18Z
closed: 2026-06-12T22:20:23Z
merged: 2026-06-12T22:20:23Z
base: master
head: solo-vs-remote-children
url: https://github.com/gerchowl/herdr/pull/101
---

# fix(sidebar): rows heading remote folds are leaders, not solos

The dompt screenshot bug: solo-form was picked by LOCAL grouping alone, so a lone local checkout with remote children kept its `· mba22:…` suffix while heading them. Law 1 (leader vs solo = does anything indent under it) now counts remote folds. Regression test included. 2176/0 · federation 7/7 · clippy clean.
