---
number: 22
title: "chore(dev): promote no-fake-impl + no-commented-code to hard gates"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-10T09:56:14Z
closed: 2026-06-10T09:56:21Z
merged: 2026-06-10T09:56:21Z
base: feat/sidebar-row-gap
head: gate-promotions
url: https://github.com/gerchowl/herdr/pull/22
---

# chore(dev): promote no-fake-impl + no-commented-code to hard gates

Promotes the last two warn-only agent-drift gates to blocking, after an unbiased review + an upstream matcher fix cleared them.

- **no-fake-impl** — now clean. herdr has zero real stubs; the 36 prior 'hits' were all the Kitty graphics protocol `*_PLACEHOLDER` vocabulary (false positives), fixed in [gerchowl/guardrails#6](https://github.com/gerchowl/guardrails/pull/6) which narrowed `placeholder` to the stub sense (`placeholder impl` / a bare `// placeholder` comment).
- **no-commented-code** — 5 doc-comment prose/JSON-example false positives annotated with `guardrails-ok` (encode.rs, integration/mod.rs ×2, render_ansi.rs, remote.rs).
- Drops the now-unused `scripts/guardrails-nudge` wrapper.
- Bumps guardrails to pick up the matcher fix, the new `perf-budget` gate, and the onboarding surfacing (`guardrails info`, richer devShell banner, template README/.envrc, starter deny.toml).

`prek run --all-files` fully green; the commit itself passed all gates.
