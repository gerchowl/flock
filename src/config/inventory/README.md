# Core config inventory — ADR-0041 §2

This is internal classification data for #2384 slices 2a/2b, not a read API or
redaction projection. `core.json` records 504 fields/container and payload nodes
and 29 enum/newtype representations. The source-based test walks `Config`
through core declarations and their imported structured-launch, check, cron,
fleet, channel, restart-policy and notification-sound declarations. Every
reachable field and variant must be classified, including entries absent from
defaults. Missing, orphaned, duplicate, changed-type and changed-representation
records fail coverage. Unsupported flattening fails explicitly.

The six former opaque boundaries are fully traversed. Collections use `[]`
and `<key>` in schema paths; externally tagged enum payloads use their serde
variant spelling. Manual/untagged representations use structural variant and
payload indices, not invented accepted TOML keys. File support means the value
is accepted through that representation, not that a structural path is itself
a file key. Custom scalar serializers (cron expressions and channel targets)
pin their Rust representation and classify the entire serialized value.

No catalog classification grants read visibility. Slice 3 of #2384 still owns
safe projections, including sensitive user-chosen diagnostic keys and the
remaining CLI/key/map/error sinks recorded on that issue. No config API ships
before that work is complete.

## Record meaning

- `type` is the stable declared model name, preserving qualified paths and generic
  arguments; it never uses compiler-produced `std::any::type_name` strings.
  `nullable` describes the field's `Option`, separately from collection or
  section shape. `file_aliases` lists additional accepted file keys, including
  `keys.fullscreen` and `advanced.scrollback_lines`. `representations` uses
  serde rename/rename_all spellings, including `blocked&done`, and pins payload
  types and private scalar/transparent-wrapper fields. Untagged/manual serde
  enums describe their structural variants rather than inventing wire tags.
  Unsupported `serde(flatten)` fails coverage until explicitly inventoried.
- A default reference names only a compiled factory and a relative path.
  `Root` uses `Config::default`; `CustomTheme` describes a field when the
  otherwise absent optional theme exists; `KeyCommand` and `Peer` describe
  fields when a collection entry exists. `parent_absent` distinguishes entry
  defaults from an entry or optional section actually present in the root.
  An empty, skip-serialized command array is explicit. A declared Option
  omitted by serde is normalized to null at its checked factory path. Imported
  entry factories use the actual compiled script/restart/action defaults and
  serde field-default functions. `Required` means an entry or selected variant
  requires a value; it does not invent a null default. Variant payload defaults
  are independent of a containing section's default-selected variant. Defaults are not read from a user file, and
  paths, executables and carried env names are never dereferenced or expanded.
- Sources report the current loader, not the ADR's future precedence migration:
  base and adjacent overlay files, the actual generic env scalar walk and its
  blocklist, omitted-default/collection limitations, and the two deprecated
  env aliases. Looking up this metadata does not read process env values.
  Command-specific flags are not a generic config-field source today.
- `validation` identifies the serde contract and additional validation owners.
  Tags apply conditionally in those owners; they do not turn warning/clamp/
  fallback behavior into strict rejection. `SidebarBounds` is the app bounds
  gate; row/pane gaps and prompt lines are model clamps; `ToastDelay` is the
  custom deserializer. Other tags refer to keybinding parsing, theme/color
  resolution, sound-file checks, idle timing, gossip cadence, peer validation,
  spawn-env normalization, reconnect bounds and session storage limits.
- `PotentialSecret` covers all free-form strings, paths, commands, endpoint
  arrays, keybinding strings and aliases, even if a field name sounds harmless.
  `RoutingMetadata` includes listener addresses and the names-only spawn env
  map, never its referenced values. `Public` is restricted to typed numbers, booleans and closed choices;
  it grants no read capability. `Aggregate` requires descendant projection.
  `Opaque` remains fail-closed but no current record uses it. User-chosen map keys have their own
  `PotentialSecret` class, including `ui.agent_aliases`,
  `session.restart_agents` and `spawn.env`; key spelling is not safe metadata.
- `LocalOperatorFile` describes current file ownership, not permission for a
  future socket/mesh write. Transport writes need the ADR's separate authority
  gates. This catalog changes no mutation behavior.
- Reload ownership records the current application entry point: `AppLive`
  requires an incoming-value copy in `App::apply_live_config`, checked against
  that method's syntax tree rather than trusting the JSON. `EventHubLive`
  specifically updates the event hub's read-wait bound, not `state.config`.
  `AppAndClient` also follows the local
  client's sound/focus reload; `ClientLive` is the client's window-title gate;
  `FuturePane` applies terminal policy to subsequently created panes.
  `ProcessStartup` includes the socket account policy, storage, system-one
  settings, reporter commands and the sampler disk path, which that app reload
  does not retarget. `msg` and `spawn` (including `spawn.env`) also stay at
  startup values because reload does not copy them into the running config;
  their `reload_issue: 2397` records the existing product bug (#2397). This
  inventory corrects the claim and does not fix that reload behavior.
  The reverse copy-set check also rejects a startup-only root section that
  gains a live copy without an ownership review. `checks`, `fleet` and
  `structured` remain startup-owned; session restart policy and channels follow
  their existing section copies. `CommandStartup` covers listener/command startup settings. `ReadOnUse` is
  the advertised icon's config lookup. `Mixed` is a container with multiple
  owners. Ownership is not a claim that every
  cached downstream consumer hot-reloads. Peer-row reconciliation also depends
  on the app's valid-UI gate. Existing ownership limitations stay tracked by
  #2384 rather than being silently changed in this inventory slice.

The JSON is reviewed record data, not generated at build time. A declaration
change requires an explicit classification/default/source/owner review; never
regenerate a public classification for an unknown field merely to pass coverage.

## Diagnostics and imported validation

Deserializer errors resolve expected types through catalog paths, array item
shapes and user-map value shapes, never `std::any::type_name`. Unknown fields
retain the nearest declared container type; an unknown classification reports
`unclassified config field`. Alias spellings resolve to canonical fields. Transparent map entry types
resolve through the checked wrapper representation without inventing a storage
field in the file schema.
No parser message is mined for a type or variant name. Diagnostic paths still
include user-map keys today: the probes demonstrate their independent
`PotentialSecret` key policy; slice 3 must redact them before API exposure.

`ChecksPolicy` names `ChecksConfig::diagnostics`; `CronExpression` names the
cron custom deserializer; `ChannelBindings` names channel validation; and
`StructuredLaunch` names launch admission/private-file checks. These are
ownership references, not a claim that loading config executes scripts or
opens a token file. `structured.token_file`, executable paths, script argv/env,
issue-guard identifiers and channel free-form identities/targets are
`PotentialSecret`; typed closed choices do not grant mutation authority.
