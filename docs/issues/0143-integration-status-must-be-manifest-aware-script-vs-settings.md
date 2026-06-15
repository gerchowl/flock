---
number: 143
title: "integration status must be manifest-aware (script-vs-settings half-state)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-14T13:42:41Z
closed: 
url: https://github.com/gerchowl/herdr/issues/143
---

# integration status must be manifest-aware (script-vs-settings half-state)

Surfaced by the second-round review of #140. `integration status` reads the version marker from the **hook script file only** — it never inspects settings.json. So a consumer who declares the manifest's hook entries (e.g. Nix) but never runs `install` reaches a broken-but-green-looking state: settings.json is correct, the script is **absent**, the hooks fire and fail "file not found", yet `status` reports `NotInstalled` with no diagnostic.

## Scope
- [ ] `status` also checks whether settings.json contains the manifest's hook entries, and distinguishes:
  - script present + settings declare it -> Current
  - script present + settings missing entries -> PartiallyInstalled
  - settings declare entries + script missing -> HookScriptMissing
- [ ] Surface the diagnostic so the user knows *which half* is missing.

Sequence: the reviewers flagged this as the next follow-up — before/with read-only-aware install — because #140 (manifest seam) makes the half-state reachable. Refs #136, #140.

---

## Comments

### gerchowl — 2026-06-15T00:26:01Z

Update after #140 / #144 land:

- **#140 is merged** — `herdr integration manifest claude --json` now emits the canonical hook entries, built from `CLAUDE_HOOK_ENTRIES` (the single source of truth `install_claude` also consumes). So a manifest-aware `status` no longer has to re-derive the expected hook entries — it can `serde_json::from_value::<Value>(integration_manifest(target)?)["hooks"]` and compare directly against the live settings.json.
- **#144 added a third hook entry** (`Stop`/`stop`, v7 bump). That makes the script-vs-settings half-state scenario in this issue's motivation *more* failure-prone in practice — a v6 settings.json declaring only `SessionStart` + `UserPromptSubmit` is now incomplete against a v7 script, and current `status` can't see it. Concretely: until this is wired, a host whose Nix-managed settings.json was last regenerated against v6 will silently never fire the Stop hook, and recap/reply capture (the whole point of v7) will silently no-op.

So the scope item _"`status` also checks whether settings.json contains the manifest's hook entries"_ should compare against `integration_manifest(target)["hooks"]` element-by-element and distinguish at least:
- script absent + settings declare it → `Broken` (the live bug surfaced here)
- script present + settings declare an older subset → `OutdatedSettings` (the new v6→v7 case)
- script + settings both up-to-date → `Current`
- script present + settings don't declare it → existing `NotInstalled`

The manifest's `version` field is a cheap pre-check before doing the per-entry diff.

