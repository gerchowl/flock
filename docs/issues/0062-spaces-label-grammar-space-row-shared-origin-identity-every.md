---
number: 62
title: "spaces label grammar: space row = shared origin identity; every member = <server>:<branch>"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-11T18:40:17Z
closed: 2026-06-12T00:10:30Z
url: https://github.com/gerchowl/herdr/issues/62
---

# spaces label grammar: space row = shared origin identity; every member = <server>:<branch>

## User spec (live dogfood, follows #56)

```
<status> space = gh origin shared
  <status> <server>:<branch/workspace/tab/..>
```

The spaces list unifies to two levels:

1. **Space row = the project identity itself** — the gh-origin-shared key (project_key, `owner/repo` label per #27), NOT a particular checkout. Status = the group join (packed rects) across ALL members everywhere.
2. **Every member — local AND remote — renders uniformly as `<server>:<target>`** (`mba22:main`, `mba22:keyboard-shorcuts`, `sage:main`), status circle per member. This dissolves #56's asymmetry where local members render bare names + a separate branch line while remote rows render `host:target`: one label grammar for all concrete checkouts; the server qualifier is always present (the local server name included — "where is this checkout" is always answered).

## Consequences / decisions
- #56's "main checkout = primary row" recasts: the space row is the identity header; the local main checkout becomes a member row like any other (`mba22:main`). Selection: the space row focuses the local main checkout when present (preserves today's muscle memory), else the most recently active member; members select themselves.
- The two-line workspace row (name + branch line) collapses for members: the branch IS the label (`mba22:keyboard-shorcuts` for worktrees, `mba22:main` for the main checkout). ahead/behind + PR glyph append to the member line. (Supersedes the branch-glyph polish for member rows; the space row keeps no branch — it's not a checkout.)
- Member tab-strip (#56) labels follow the same grammar minus the local server prefix (strip is local-session scoped): `<ID> <branch/name>`.
- misc (non-git) workspaces: label stays `<server>:<workspace-name>`.
- Ordering within a space: local members first (main, then worktrees), then remote by server.

## Sequencing
After the in-flight band-polish PR merges (same file). This is the follow-up to #33/#56 — the label-grammar unification half.

## References
#33/#56 (sections/primary rows), #27 (owner labels), #51 (remote folding), screenshots in chat 2026-06-11.

---

## Comments

### gerchowl — 2026-06-11T18:40:34Z

## Addition (user): closing the main checkout must not break the session

Closing the `<server>:main` member (the main-checkout workspace) leaves the SPACE intact: its other members (worktree workspaces, remote rows) stay connected, grouped, and functional — the session persists. Consequences:

- The space row's identity must not die with the main checkout: grouping keys off the shared origin (project_key) / membership key, which worktree members carry themselves — verify nothing (sidebar grouping, space row selection fallback, kill_worktree main-root derivation, collapse keys, attention rollups) dereferences the main workspace as the group's anchor after it closes.
- Space-row selection fallback (from the body): with main gone → most recently active member.
- Today's behavior to audit: close_selected_workspace treats a NON-linked member as 'close the whole group' (parent close = group close, #17-era semantics) — under the new model closing main should close ONLY that workspace by default; the close-whole-space affordance moves to the SPACE row (context menu / close on the header).
- Restore: a persisted session whose main checkout was closed must restore the space from its members alone (no main present) without regressing grouping or the strip.

### gerchowl — 2026-06-11T18:42:58Z

## Addition (user): space symbols use the agents' icon style

The space row's status symbol (and member rows') adopts the AGENTS PANEL's icon language — the rich set, not just the SSoT colors (#54 consolidated colors but left circles on space/workspace rows):

- ✓ green checkmark — done
- the animated yellow spinner — working (reuse the agents panel's existing tick-driven animation machinery; same frames, same cadence)
- red escalation mark — blocked
- muted/hollow — no live agents

Driven by the row's join head (space row = group join head, member row = its own). One icon mapping shared by the agents panel, space rows, and member rows — the literal completion of #42 item 3 ('adopt the symbol/animations we have for the agents too. SSoT').

### gerchowl — 2026-06-11T18:43:15Z

Clarification (user): the agent-style icons REPLACE the circles — the icon IS the leading mark on space and member rows; `○`/`●` retire there entirely (the no-live-agents state renders as the muted/idle glyph of the agents' set, not a hollow circle). Circles remain only where they're a different signal (servers band reachability dot, seen/unseen if still needed — fold seen/unseen into the icon set's existing done-unseen treatment).

### gerchowl — 2026-06-11T18:44:32Z

## Addition (user): agents panel compacts to single rows

Each agent entry becomes ONE row:

`<status-symbol> <agent> <server> <proj> <workspace|branch>`

- The status TEXT ('idle', 'working', …) is removed — the symbol (✓/spinner/escalation, same set as everywhere after this issue) carries the state.
- Location grammar matches the spaces list: server-qualified, project, then workspace/branch — e.g. `✓ cc mba22 herdr keyboard-shorcuts`.
- The two-line agent entry (name line + '<agent> · <status>' line) retires; live-activity text and custom-status/header-field chips drop from the panel per this spec (they remain in the pane header, navigator, and member rows).
- Width pressure: truncate location right-to-left (branch first, then proj), middle-truncate per existing conventions.

This makes the whole sidebar one visual grammar: icon + identity + location, single-line everywhere except the two-line servers band.

### gerchowl — 2026-06-11T18:59:02Z

## Addition (user): quick-control key layering

- `ctrl+shift+1..9` = Nth MEMBER of the current space (switch_tab, bound)
- `ctrl+1..9` = Nth SPACE — requires a new `switch_space` indexed action under this issue's section model: jump to the Nth project section (focus its active member, else primary/local main). Currently bound to switch_workspace (global row N) as interim; repoint the binding when switch_space lands. Space sections should display their index for discoverability (like workspace row numbers today).

### gerchowl — 2026-06-11T20:00:12Z

## Addition (user, via the sage gap): REMOTE agents in the agents panel

The agents panel is local-only on every machine — the hub never showed peer agents either. Under this issue's single-row grammar the remote case is free: peer/origin summaries already carry per-workspace agent + status, exactly the fields of `<status-symbol> <agent> <server> <proj> <workspace|branch>`. Render remote agent rows from the same summaries that feed the spaces folding (config peers on the hub, carried snapshot + origin summary on spokes — see the origin-gossip issue), scope-respecting (all/current), selecting one = the same switch the workspace row would do.

### gerchowl — 2026-06-12T00:10:29Z

Shipped in PR #74 (6 staged commits, rebased over the #58-#73 trains): identity-header spaces, uniform <server>:<target> member grammar, agent icons replacing circles, single-row agents incl. remote, switch_space(N), close-main-keeps-the-space. Bonus real fix from the rebase: char-boundary truncation on remote leader labels (byte-slice panicked the whole spoke render).

