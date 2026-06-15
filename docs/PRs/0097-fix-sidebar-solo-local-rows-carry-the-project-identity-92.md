---
number: 97
title: "fix(sidebar): solo local rows carry the project identity (#92)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-12T16:08:24Z
closed: 2026-06-12T16:08:53Z
merged: 2026-06-12T16:08:53Z
base: master
head: solo-identity
url: https://github.com/gerchowl/herdr/pull/97
---

# fix(sidebar): solo local rows carry the project identity (#92)

## Summary
- Solo local workspaces (one local member, no remote folds, no group) used to render as bare member grammar (`mba22:keyboard-shorcuts #17`) with no project identity anywhere on the row. They now adopt the solo-remote form #81 already produces: `<icon> <owner/repo> · <server>:<branch> [#PR]` — identity first, locator second, one line, no synthetic group.
- A new `grammar::solo_local_label` is the single formatter; it mirrors the shape `remote_entry_label` returns for lone remote checkouts. The sidebar render picks it for unindented non-leader rows. Leaders (≥2 local members) and indented member rows are untouched, so a second member (worktree add or remote fold) graduates to the existing leader+members form automatically.
- Unresolved identity falls back to the workspace display label alone — never bare `<server>:<branch>`, which would read like a member of an absent group.

Closes #92.

## Test plan
- [x] `cargo test --bin herdr -- ui::grammar:: ui::sidebar::tests:: --test-threads=2` — two new sidebar buffer tests (`solo_local_row_renders_project_identity_then_server_branch`, `solo_local_row_unresolved_identity_uses_display_label_only`) and two new grammar unit tests.
- [x] `cargo test --test peer_federation -- --test-threads=2` — full suite green; the federation label assertions and indented-member needle still match (groups remain byte-identical).
- [x] `cargo test --bin herdr -- --test-threads=2` — 2143 passed; one updated flat-row assertion in `src/ui.rs` (the row now carries the project identity ahead of the icon-anchor, so the test now anchors on the icon position rather than the bare `<server>:one` tail that gets truncated on a long `dir:<tmp>` project key).
- [x] `cargo fmt` + `cargo clippy --all-targets -- -D warnings` clean.
