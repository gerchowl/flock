---
number: 60
title: "feat(dev): sccache in the devShell — worktrees inherit the compile cache"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T15:51:28Z
closed: 2026-06-11T15:51:34Z
merged: 2026-06-11T15:51:34Z
base: feat/sidebar-row-gap
head: devshell-sccache
url: https://github.com/gerchowl/herdr/pull/60
---

# feat(dev): sccache in the devShell — worktrees inherit the compile cache

Fresh worktrees no longer pay the ~10-min cold build: `sccache` as `RUSTC_WRAPPER` shares one compiler-level cache across all worktrees while each keeps its own `target/` — parallel agent builds stay parallel (the `CARGO_TARGET_DIR`-sharing alternative would serialize on cargo's build-dir lock). Validated live: stats show cache traffic on the first populating build. 20G cap, `~/.cache/herdr-sccache`, opt-out via `RUSTC_WRAPPER=""`.
