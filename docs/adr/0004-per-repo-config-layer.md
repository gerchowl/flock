# ADR 0004 — Per-repo configuration: a committed `.flk.toml` policy layer for repo facts

- Status: Proposed
- Date: 2026-07-03
- Issues: #121 (default-branch protection — the motivating first consumer),
  extends #100/#103/#112 (ADR-0002 config stack)
- Decision owner: human; advised by the worktree-kill safety thread and the
  ADR-0002 config subsystem.

## Context

ADR-0002 made configuration a clean four-layer stack, but every layer is
**user/fleet-global**:

    Rust `Default`  <  FLOCK_<UPPER_SNAKE>  <  config.toml  <  config.local.toml

That is the right home for *preferences* (keybinds, theme, UI) — they are the
operator's, and follow them across repos. But some configuration is a **repo
fact**, not a preference: it is true for every clone and every user of that
repo, and it should travel *with the repo*, committed, reviewed like code.

\#121 surfaced this sharply. The worktree-kill flow could offer to delete the
local `main` branch (the merge gate treated "main is reachable from origin" as
merge evidence). The fix is a **hardcoded, config-independent** guard: the
default branch (`main`/`master`/`origin/HEAD`) is never auto-deletable. But the
*natural extension* — "also never prune `develop` or `release/*` in this repo" —
is a repo policy with nowhere to live. Putting it in the user-global config is
wrong: a teammate who clones the repo would inherit none of it, and a laptop's
`config.local.toml` is the wrong scope for a fact about the repository.

Other configuration has the same shape: the worktree base directory/naming for
this repo, merge-gate strictness (require a merged PR vs. accept local merge
evidence), which remote is "upstream" for containment checks, repo-specific
agent/pivot setup, and per-repo onboarding commands. All are repo facts with no
committed home today.

## Decision

Add a **repo-scoped layer** to the ADR-0002 stack, for repo-domain keys only:

    …ADR-0002 global stack…  <  .flk.toml (committed)  <  .flk.local.toml (gitignored)

1. **Two kinds of config, kept distinct.**
   - **Repo policy** (facts; repo owns): protected branches, worktree base +
     naming, merge-gate strictness, upstream remote, repo agent setup. Home:
     committed `.flk.toml`, shared across every clone and user.
   - **User preference** (keybinds, theme, UI): stays global (ADR-0002). The
     repo file simply does not carry preference keys; if present they are
     ignored, so a repo can never dictate a user's keybinds.

2. **`.flk.toml` (committed) + `.flk.local.toml` (gitignored).** Mirrors the
   ADR-0002 base/overlay split at repo scope: the committed file is shared
   policy; the local overlay is a per-clone override (e.g. *this* machine's
   worktree base). Naming follows ADR-0003 (`flk`); `.toml` for tooling.

3. **Safety-critical protections are additive (set-union), never override.**
   `protected_branches` can only *add* to the hardcoded floor. No config layer
   — repo or user — can unprotect `main`/`master`/the detected default branch.
   The floor lives in code (`is_protected_branch`) and holds with zero config.

4. **Repo policy is read from the main checkout / git common dir, not the
   current worktree's checked-out copy.** Otherwise a feature branch could edit
   *its own* `.flk.toml` to unprotect `main` and then delete it. The
   authoritative policy is the default branch's / main worktree's file.

5. **Discovery:** walk up from cwd to the git toplevel to find `.flk.toml`;
   absent file → empty repo policy (the hardcoded floors still apply).

## First consumer (shipped alongside)

`[worktrees] protected_branches` (a `Vec<String>`) lands now, wired into the
existing global config as the bridge, consumed by the kill flow as an *extension*
of the hardcoded default-branch floor (#121). The repo-scoped `.flk.toml` is the
designed home this evolves into — the global key is the increment that ships the
capability before the new layer exists.

## Alternatives considered

- **Global-only (status quo).** Protected branches aren't a user preference;
  a fresh clone would inherit no protection, and `config.local.toml` is the
  wrong scope for a repo fact. Rejected as the *end state* (kept as the bridge).
- **Hardcode everything.** The `main`/`master` floor must be hardcoded, but
  `develop`/`release/*` vary per repo and can't be enumerated in code. Rejected.
- **A `[repo]` section inside the global `config.toml`.** Couples a repo fact to
  a user file; doesn't travel with the repo or get reviewed with it. Rejected.

## Consequences

- New discovery path (git-toplevel walk) and the worktree-authority rule (read
  from the common dir) — both must be covered by tests.
- `adr-matrix`: Proposed status trips no gate; on flip to Accepted this needs a
  `FEATURE-MATRIX.md` row citing `ADR-0004`.
- Version skew: older flock ignores `.flk.toml` (keeps current behavior + the
  hardcoded floors); the committed policy is `#[serde(default)]` throughout.
- Precedent in the fleet: guardrails already ships committed per-repo policy
  (`guardrails-allow.txt`, `perf-budgets.toml`); `.flk.toml` is the flock analog.

## References

Parent: ADR-0002 (config stack), ADR-0003 (flk rename). Code: worktree.rs
(`is_protected_branch`, `detect_default_branch`, `delete_local_branch`),
config/model.rs (`WorktreesConfig.protected_branches`), app/worktrees.rs
(`handle_worktree_kill_gate_finished`). Issue: #121.
