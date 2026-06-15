---
number: 112
title: "config overlay: deep-merge so config.local.toml can OVERRIDE base-set scalars"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-13T18:35:35Z
closed: 
url: https://github.com/gerchowl/herdr/issues/112
---

# config overlay: deep-merge so config.local.toml can OVERRIDE base-set scalars

## Follow-up to #108

#108's overlay uses text concatenation, which toml 0.8 rejects for duplicate table keys -- so config.local.toml can add new sections/keys but cannot OVERRIDE a scalar the base already sets (e.g. ui.tab_mode, which the HM-generated base sets). For a 'tweak my config locally' tool that is the common case.

## Fix
Parse base AND overlay as toml::Value (or toml_edit Document), DEEP-MERGE (overlay scalars/tables win, [[peers]] arrays append or replace -- decide), then deserialize the merged Value into Config. Replaces the concat in load_live_config. Preserves the per-section keep-current-on-error contract: a section that fails to merge/deserialize keeps the base value + a diagnostic.

## Acceptance
- config.local.toml setting ui.tab_mode overrides a base that already sets it.
- Malformed overlay still keeps the base + diagnostic.
- Round-trips through the existing reload report.

## References
#108 (the concat v1 + its documented limitation), src/config/io.rs load_live_config, toml::Value/toml_edit.
