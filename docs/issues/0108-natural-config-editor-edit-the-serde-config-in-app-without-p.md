---
number: 108
title: "natural config editor: edit the serde config in-app without per-field wiring"
kind: issue
state: CLOSED
author: gerchowl
labels: []
created: 2026-06-13T09:42:54Z
closed: 2026-06-13T18:35:33Z
url: https://github.com/gerchowl/herdr/issues/108
---

# natural config editor: edit the serde config in-app without per-field wiring

## Feature (user): a natural config editor -- edit the serde config WITHOUT per-field UI wiring

Every new knob today needs hand-wiring into herdr (config struct + state field + population + a UI surface). The user wants to stop wiring each one: a generic editor that reflects the config schema and round-trips through serde.

## Approaches (pick in the spike)
1. **v1 -- edit-the-source-with-validation (cheap, high value)**: a `herdr config edit` action (and a keybind) opens the live config.toml in $EDITOR inside a herdr FLOAT/pane; on save, parse via the existing serde Config + reload-config path, surfacing diagnostics inline (the reload report already exists). No per-field wiring -- ANY field serde knows about is editable immediately. Handles the HM-symlink case (the file is a read-only /nix symlink -> edit an overlay like ~/.config/herdr/config.local.toml merged at load, per the #94 overlay idea).
2. **v2 -- schema-driven form**: derive a JSON schema from the Rust Config (schemars), render a generic key/value form in a herdr overlay (sections -> fields -> typed inputs), write back TOML. New fields appear automatically from the derive. More work; the natural endgame.
3. **Hybrid**: v1 now (editor + validate + reload), schemars-annotate the Config so v2/the website docs/an external editor can all consume one schema.

## Pitfalls
- Read-only nix-managed config.toml: writes must target a user overlay, not the symlink (#94).
- Validation/rollback: a bad edit must not break the running server -- validate before apply, keep the prior config on parse failure (the reload path already reports diagnostics).
- Secrets/peers: don't expose generated [[peers]] as user-editable if they're nix-generated.

## References
config reload report (ConfigReloadReport), #94 (ssh-host/overlay server config), schemars, the float pane (#28) as the editor host.

---

## Comments

### gerchowl — 2026-06-13T17:54:51Z

## Spike verdict: DECISIVE-YES (v1a, ~400 LoC, no prerequisite migrations)

All seams exist: Config flat serde struct (#[serde(default)] everywhere); load_live_config -> LoadedConfig with per-section diagnostics that does NOT drop the running config on a bad parse; App::reload_config -> ConfigReloadReport (the validate+rollback pipeline -- no per-field wiring); the $EDITOR-in-pane + temp-file-cleanup precedent (navigate.rs edit_scrollback via spawn_pane_command); the float host (#28).

**v1a (skip schemars/v1b -- the user wants NO per-field forms; editing source TOML already gives 'any new serde field works immediately'):**
1. keys.edit_config binding + EditConfig action (mirror edit_scrollback/toggle_float).
2. App::edit_config_file(): copy config to a temp, open ${EDITOR} in a FLOAT via spawn_pane_command, cp back on close.
3. **Write target / overlay (the one new piece, ~20 lines)**: if the base config is a read-only symlink (nix/HM), write $XDG_CONFIG_HOME/herdr/config.local.toml; load_live_config concatenates the overlay AFTER the base text before parse (later [section] scalars win). Solves #94's read-only problem at tighter scope. Caveat documented: table re-declaration errors, [[peers]] appends.
4. Validate+rollback on the editor pane's PaneDied: apply_config_from_disk(true) -> if Failed, restore the pre-edit backup + toast diagnostics (existing channel).
5. CLI parity: herdr config edit.

Landing per the session goal (decisive on current stack).

### gerchowl — 2026-06-13T18:35:33Z

Shipped in PR #111 (v1a): edit_config action (float-hosted $EDITOR -> reload -> rollback-on-invalid), herdr config edit CLI, config.local.toml overlay. Honest correction to the spike: toml 0.8 rejects duplicate table keys, so the overlay ADDS sections/new-keys but cannot OVERRIDE a base-set scalar by concat -- documented. Proper override-merge is the follow-up below.

