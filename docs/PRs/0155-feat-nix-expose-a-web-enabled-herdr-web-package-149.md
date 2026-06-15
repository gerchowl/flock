---
number: 155
title: "feat(nix): expose a web-enabled herdr-web package (#149)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-15T13:18:45Z
closed: 2026-06-15T13:18:53Z
merged: 2026-06-15T13:18:53Z
base: master
head: feat/nix-web-package
url: https://github.com/gerchowl/herdr/pull/155
---

# feat(nix): expose a web-enabled herdr-web package (#149)

Prereq for #149 (g-fleet adopting `herdr web`). Adds a `withWeb` flag to `nix/package.nix` (builds the `web` cargo feature) and a `herdr-web` flake package output that uses it. The `default` package is unchanged/lean; only the new output pulls axum/rust-embed. Same `bin/herdr`, plus the `web` subcommand.

Eval-verified: `packages.<sys>.herdr-web.pname` = `herdr-web`, `default.pname` = `herdr`. (Full build runs in CI / on `just apply`.)

Refs #149, #131.
