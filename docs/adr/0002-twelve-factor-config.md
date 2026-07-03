# ADR 0002 — Twelve-factor configuration: four layers, one write target, one live source

- Status: Accepted
- Date: 2026-07-02
- Issues: #112 (overlay deep-merge — closed), #108 (natural editor v1a — closed
  in PR #111), follow-ups TBD (write-path unification, startup overlay parity,
  generic env layer, `[web]` fold, settings-pane derip)
- Decision owner: human; advised by parallel review of the settings-pane
  write-path thread and the config subsystem code paths (config/io.rs,
  app/config_io.rs, web/mod.rs, main.rs::DEFAULT_CONFIG).

## Context

flock's configuration story accreted a layer at a time and now trips over
itself in six load-bearing places (all present at main @ ae0cc1c):

1. **Cold-start vs live-reload asymmetry.** `Config::load` (config/io.rs:51),
   used at process start, reads *only* `config.toml`. `load_live_config`
   (config/io.rs:147) is the only path that deep-merges `config.local.toml`
   (#112). A field set in the overlay works after `reload_config` but not at
   cold start. `configured_node_name` (app/api/peers.rs:310) already worked
   around this by calling `load_live_config` from a non-reload code path —
   that workaround is the symptom, not the fix.

2. **Settings-pane writes silently fail on centrally-managed hosts.**
   `App::update_config_file` (app/config_io.rs:100) writes `config_path()`
   directly. On nix/Home-Manager hosts that path is a read-only symlink into
   `/nix/store`; the write fails, a toast shows for 5 s, the toggle reverts.
   The `flock config edit` path (same file) correctly routes through
   `resolve_write_target` (app/config_io.rs:17), which redirects to the
   overlay in exactly this case. The settings pane never learned; neither did
   `flock config reset-keys` (cli.rs:263).

3. **Env vars are scattered per subsystem.** `FLOCK_HOST_NAME` shadows
   `Config.name`; `FLOCK_DISABLE_SOUND` shadows `ui.sound.enabled`; six
   `FLOCK_WEB_*` vars parametrise `flock web` directly, bypassing `Config`.
   There is no single answer to "what env vars does flock read" and no
   discipline for how env combines with the file layer.

4. **`flock web` is a separate config universe.** `WebConfig` (web/mod.rs:56)
   is CLI flags falling back to `FLOCK_WEB_*`, never reading `Config`. A
   `[web]` section in config.toml is an "unknown section" warning today.

5. **The config surface drifts from the loader.**
   `KNOWN_TOP_LEVEL_CONFIG_KEYS` (config/io.rs:7) is missing `slots` and
   `name`; `[slots]` is not wired into the live-reload path at all (silently
   dropped on reload, honored at cold start — the asymmetry crossed with the
   drift). `DEFAULT_CONFIG` (main.rs:74-370) is a ~300-line hand-maintained
   TOML shadow of the serde schema.

6. **The settings pane is mostly ceremony.** ~15 of ~162 `Config` fields are
   wired, and each one costs six edits across model, `AppState` mirror,
   getter, input handler, `save_*`, and render branch. The pane reads the
   mirrors, not the live `Config` (#108's documented pain).

The natural editor (PR #111) and the overlay (#112) already established the
answer's shape: file + overlay is the single source of truth, edited through
the same validated, reloaded TOML. This ADR extends that discipline to all
load paths and all write paths, and makes the settings pane a curated shim
over the same TOML rather than a parallel writer.

## Decision

**Four named layers, applied identically at cold start and live reload —
later wins:**

    Rust `Default`  <  FLOCK_<UPPER_SNAKE>  <  config.toml  <  config.local.toml

1. **`Config::load` becomes a thin wrapper around `load_live_config`.** One
   code path, one precedence, one diagnostics contract. The
   `configured_node_name` workaround collapses into a plain field read.

2. **The env layer is generic:** `FLOCK_<UPPER_SNAKE_PATH>` overrides any
   scalar field (`FLOCK_UI_IDLE_ENABLE` → `[ui.idle] enable`). Values parse
   as TOML scalars; type errors surface as ordinary diagnostics. A small
   blocklist (`keys`, `peers`, `theme.custom`, `sound.custom` paths) stays
   file-only. Env sits BELOW the file: env is a per-invocation poke, the file
   is the source of truth. `FLOCK_DISABLE_SOUND` / `FLOCK_HOST_NAME` become
   deprecated aliases for one release (note: this weakens them relative to a
   file-set value — the twelve-factor-correct behavior, called out in the
   release note). `FLOCK_CONFIG_PATH` and `FLOCK_ENV` are system-level,
   outside the stack, unchanged.

3. **All write paths go through `resolve_write_target`** — the settings pane
   and `config reset-keys` inherit the symlink→overlay redirection the
   $EDITOR path already has. On read-only bases, `reset-keys` *shadows* via
   an overlay `[keys]` with defaults (deep-merge wins) instead of failing.

4. **The settings pane is a curated shim over the same TOML.** Toggles write
   through `update_config_file` (now correctly routed) and the pane renders
   from the live `Config`; the `AppState` mirror fields are deleted (accessors
   stay as thin delegations). Contract: nothing exists in the pane that isn't
   the same TOML the $EDITOR path would produce.

5. **`flock web` folds into `[web]`** in `Config`; CLI flags override; the
   `FLOCK_WEB_*` env reads are subsumed by the generic layer (list-valued
   vars keep CSV parsing as a documented web-only exception).

6. **`KNOWN_TOP_LEVEL_CONFIG_KEYS` is drift-proofed** by a unit test that
   walks `Config::default()`'s serialized table and fails when a field is
   missing from the list or from the live-reload loader (kills the `[slots]`
   class of bug).

7. **`DEFAULT_CONFIG` becomes derived** from `toml::to_string_pretty(&Config::default())`
   plus a preamble; the prose moves to `docs/config-reference.md`. A
   round-trip test pins output == `Config::default()`.

8. **Tunables direction (reserved).** When the guardrails-tunables registry
   ships (the deferred no-hardcoded phase), the settings pane can iterate its
   Config-tier entries and render generic fields — the natural evolution of
   the shim. Out of scope here; direction noted so the phases converge.

## Alternatives considered

- **schemars-driven form (#108-v2).** Correct endgame, ~10x the code; the
  tunables-registry direction reaches the same outcome for the subset
  operators actually tweak, with per-field validation for free. Deferred.
- **Env-only (12-factor purist).** `Config` has nested tables (`[[peers]]`,
  `[theme.custom]`, keybinds) that don't map to env; the file is the surface
  #108/#111 shipped and users rely on. Rejected.
- **Status quo.** The write-path bug is a visible regression on every HM/nix
  host (every toggle → toast → revert); the startup asymmetry has already
  forced one workaround. Rejected.
- **Delete the settings pane; live by `flock config edit`.** Discoverability
  is real value for first-run users. Make it honest instead. Rejected.

## Consequences

- Migration: deprecated env aliases warn once via the config-diagnostic
  channel; previously-inert `[web]` sections take effect (release note).
- Version skew: older flock reading newer config gets "unknown section"
  warnings and keeps running (existing keep-current-on-error contract);
  newer reading older is unchanged (`#[serde(default)]` everywhere).
- Docs: `docs/config-reference.md` carries the field prose; an "environment
  variables" section documents the convention, the system-level exceptions,
  and the aliases.
- The four-layer semantics get one table-driven test (default/env/base/
  overlay wins) plus the drift-proofing test; the settings-pane refactor is
  mechanical and lands one section per PR.

## Resolved questions (decided 2026-07-03, flipping to Accepted)

1. Env precedence: `env < file`, as implemented in phase (d). Env is a
   per-invocation poke; the file is the source of truth. Consciously accepted:
   file-set values now beat the deprecated FLOCK_HOST_NAME/FLOCK_DISABLE_SOUND
   aliases.
2. `reset-keys` on read-only bases: POINTER, not shadow — plus a fifth write
   concept that supersedes the question: the **fleet-source write target**.
   On managed hosts the base symlink's real source of truth is a file in the
   fleet repo (e.g. ~/dotfiles/herdr/config.toml). A configured
   `[advanced] fleet_config_source` path lets in-app edits land THERE as an
   ordinary dirty working-tree edit — the next `just apply` carries it to
   every host (the nix-ish flow: store immutable, repo owns edits, apply
   deploys). For immediate local effect the same edit dual-writes the overlay;
   the apply-built base then supersedes the overlay duplicate (reconciliation
   may prune overlay keys that equal the base). `reset-keys` without a
   configured source refuses with a pointer naming the source file.
3. `AppState` holds `Config` owned — settled by evidence in phase (f): no hot
   path clones AppState.

## References

Prior art: PR #111 (#108-v1a), #112. Load: config/io.rs. Write:
app/config_io.rs, cli.rs:263. Env: app/api/peers.rs:283, sound.rs:13,
web/mod.rs:644-675. Pane: ui/settings.rs, app/input/settings.rs,
app/state.rs:1179. Rollback: app/api.rs:504. Drift: main.rs:74-370.
