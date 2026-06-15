---
number: 10
title: "chore(dist): latest branch channel for Nix consumers"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-06T19:17:12Z
closed: 2026-06-06T19:17:26Z
merged: 2026-06-06T19:17:26Z
base: feat/sidebar-row-gap
head: chore/latest-channel
url: https://github.com/gerchowl/herdr/pull/10
---

# chore(dist): latest branch channel for Nix consumers

Nix-only distribution per discussion: `latest` branch auto-fast-forwards after the flake package builds green in CI. Consumers pin latest (newest-green) / integration branch (bleeding edge) / rev (immutable). Doc + workflow only.
