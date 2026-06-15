---
number: 111
title: "feat(config): in-app config editor + config.local.toml overlay (#108)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-13T18:34:31Z
closed: 2026-06-13T18:35:28Z
merged: 2026-06-13T18:35:28Z
base: master
head: config-editor
url: https://github.com/gerchowl/herdr/pull/111
---

# feat(config): in-app config editor + config.local.toml overlay (#108)

## Summary

Implements #108 (v1a spike): edit the live herdr config from inside the running app or via the CLI, with validate+rollback on close and a new overlay layer (`config.local.toml`) so nix/HM users with a read-only `config.toml` symlink can still tweak settings.

- **Overlay loader** (`src/config/io.rs`): `load_live_config` concatenates `\$XDG_CONFIG_HOME/herdr/config.local.toml` after the base text before parse. Malformed overlay -> diagnostic, base config stays loaded.
- **Write-target resolver** (`src/app/config_io.rs::resolve_write_target`): live config if writable, overlay otherwise (creates parent + header comment when missing).
- **`edit_config` action**: new `keys.edit_config` binding (default empty), App-level `launch_config_editor` copies the target to a temp, hosts `\${EDITOR:-vi} <tmp> && cp <tmp> <target>` in an overlay pane (mirrors `edit_scrollback`). On PaneDied -> `apply_config_from_disk(true)`. On Failed apply we restore the pre-edit backup, re-apply, and toast the diagnostics.
- **`herdr config edit` CLI**: resolves the same write target, opens `\$EDITOR` directly, prints a hint to run `herdr server reload-config`.
- **Docs**: `website/.../configuration.mdx` gains a section on the editor + the overlay precedence rules.

### Overlay precedence rules (documented + tested)

The overlay is meant to ADD scalars / sections the base does not declare:

- Can introduce a brand-new `[section]` block.
- Can extend a base section with NEW scalar keys it does not set.
- Cannot override a scalar the base already sets in the same `[section]` -- toml 0.8 rejects duplicate keys across re-declared tables. Remove from base to override.
- `[[peers]]` arrays APPEND -- overlay cannot remove peers.
- Malformed overlay -> diagnostic, base config stays loaded.

## Test plan

- [x] \`cargo test --bin herdr -- --test-threads=2\` -- 2191 pass (one headless test is documented-flaky, passes on isolated rerun).
- [x] New tests:
  - \`overlay_introduces_new_section_when_base_omits_it\`
  - \`malformed_overlay_keeps_base_and_surfaces_diagnostic\`
  - \`write_target_uses_overlay_when_base_is_symlink\`
  - \`write_target_uses_base_when_base_is_real_file\`
  - \`launch_config_editor_registers_post_exit_hook_and_temp_files\`
  - \`config_edit_rolls_back_when_edit_produces_invalid_toml\`
- [x] Smoke: \`HERDR_CONFIG_PATH=/tmp/foo EDITOR=true herdr config edit\` writes the overlay header and prints reload hint.
- [x] \`cargo fmt\` + clippy -D warnings clean.
- [ ] Manual: open herdr, bind \`keys.edit_config = "prefix+shift+e"\`, hit it, edit & save, observe reload toast. Sanity-check the rollback by saving a syntax error.
