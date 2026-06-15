---
number: 33
title: "git-first sidebar: projects as sections, main checkout as primary row, members as tab-like entries; misc for non-git"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-10T20:51:56Z
closed: 2026-06-11T13:08:49Z
url: https://github.com/gerchowl/herdr/issues/33
---

# git-first sidebar: projects as sections, main checkout as primary row, members as tab-like entries; misc for non-git

## Motivation

The sidebar's organizing principle is still "flat workspace list, with worktree-space groups as a special case". After #25 (workspace-as-unit, tabs de-emphasized) the natural end-state is **git-first organization**:

- **Git projects are the first-class sections.** Every workspace belonging to the same repo (main checkout + linked worktrees + plain same-repo workspaces) lives under one project header.
- **Non-git workspaces collect under a `misc` section** — lifted out of the way rather than interleaved.
- **The main checkout IS the project's primary row** — not a synthetic group header above it; it's "treated the same as a workspace" (selectable, has agent state, is the parent).
- **The project's other members (worktrees/sibling workspaces) render as tab-like subentries** under that primary row — the visual idiom tabs used to own, repurposed for what the fork actually multiplexes: checkouts of one repo. With `tab_mode = "workspace"` (#26) members ARE the new tabs semantically; the sidebar should say so visually.

## What already exists

- `WorktreeSpaceMembership.key` drives today's grouping (sidebar.rs `collapsible_space_keys`, `workspace_parent_group_state`); collapse-all + traffic-light counts (#17) and auto-collapse already operate on these groups.
- `repo_group_key()` (workspace.rs) falls back to live git metadata for plain workspaces — the hook for grouping non-worktree same-repo workspaces.
- `project_key` (normalized origin URL, machine-independent) folds federation rows into project groups — **note the duality**: membership key ≠ project_key (git common dir vs origin URL); a git-first sidebar should converge on one notion (project_key is the better candidate: it also matches remote rows).
- Owner-qualified labels (#27) already render `org|person/repo`.

## Scope sketch

- P0: section the workspace list by project key (git projects first, `misc` last); main checkout (non-linked member) is the section's primary row; members indent under it as tab-like entries; collapse/auto-collapse/attention roll up unchanged.
- P1: ordering within and across sections (attention-priority? recency?); federation remote members fold into the same project sections (they already fold by project_key — verify the merged view).
- P2: converge membership-key vs project_key so local + remote + plain workspaces all group by one identity.

## Pitfalls

- The membership-key/project_key duality above — grouping by two different keys produces split sections for the same repo.
- Sidebar render is an upstream-active surface (fork tracks daily) — keep the sectioning additive, reuse the existing group machinery rather than rewriting the list walk.
- `misc` must not swallow workspaces whose git metadata simply hasn't resolved yet (cached_git_space is async — need a "pending" treatment, not a flash of misc→project).
- Interacts with #29 (group-local switching): "switch tab" within a project = move between the project's members — the two issues should land coherently.

## References
#25 (workspace-as-unit spike, closed), #26/#28 (landed), #29 (group-local keys), #27 (owner labels), src/ui/sidebar.rs, src/workspace.rs `repo_group_key`, src/workspace/git/discovery.rs `project_key`.

---

## Comments

### gerchowl — 2026-06-11T08:01:51Z

## Design completion (user): tab bar = session-member switcher + two-level highlight

1. **Sidebar highlights two levels**: the current SESSION (project/space primary row) and the current WORKSPACE (member) both carry the standard highlight fill — one 'where am I' idiom across server → session → workspace (matching the current-server highlight from #36/PR #39).

2. **The top tab bar renders the current session's workspaces as tabs**, labeled `<ID> <name>` (e.g. `1 main · 2 keyboard-shorcuts · 3 calm-forest`). In tab_mode=workspace the strip was semantically idle (single-tab workspaces) — this gives it the member-switcher job. It structurally resolves #29: prefix+1..9 'switch tab' switches session members BECAUSE that's what the strip shows — no navigate.rs dispatch rerouting (the fork-strategist's veto), the content under the existing keys changes instead.

Sequencing: after #41 (sidebar chrome rework) lands to avoid colliding on sidebar.rs; this issue then covers sections-by-project + primary rows + member tab-strip + two-level highlight as one coherent restructure.

### gerchowl — 2026-06-11T09:43:32Z

## Tab-strip coloring (user)

Same scheme as everywhere (#42 SSoT + the #43 focus language):
- **Selected member tab = accent** — focus is always accent (pane borders, header divider, current-server row, tab strip: one rule).
- **Unselected tabs = the member's state color as TEXT/outline tint only** (the #42 join head = worst state), DIMMED — no background fills or chips, deliberately non-distracting: state whispers, focus speaks.

### gerchowl — 2026-06-11T13:08:49Z

Shipped in PR #56 (4 staged commits + the review fix): project sections (project_key-merged, membership-canonical), primary rows with group joins, member tab-strip <ID> <name> in workspace mode (resolving #29 via state-method branching + a pre-match hook — dispatch match byte-identical with upstream), two-level highlight, trailing positional misc with pending-flash protection. Post-impl review: approve; the one real nit (pending-flag on stale-cwd path) fixed in-branch.

