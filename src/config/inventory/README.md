# Core config inventory — ADR-0041 §2

This is internal classification data for #2384 slice 2a, not a read API or a
redaction projection. `core.json` records 276 canonical fields/container nodes
and 21 core enum/newtype representations. The source-based test walks `Config`
through the five core declaration files. It rejects missing, orphaned or
duplicate records, changed field types/optionality, new enum payloads and
unclassified imported types. A serialized default alone is not coverage.

The six exact opaque boundaries are `structured`, `checks`, `fleet`,
`channels`, `session.restart` and the values of `session.restart_agents`.
Their source/model references are explicit; defaults remain opaque and no
child is made readable. Slice 2b of #2384 follows those imported declarations,
replaces the boundaries, completes whole-graph coverage and connects stable
per-field type names to deserializer diagnostics. Slice 3 implements the safe
projections before any config API ships. Neither the existing diagnostic audit
nor its remaining CLI/key/map/error paths is claimed complete here.

## Record meaning

- `type` is the stable, canonical declared model name, including generic
  arguments; it never uses compiler-produced `std::any::type_name` strings.
  `nullable` describes the field's `Option`, separately from collection or
  section shape. `representations` pins core enum payloads and the private
  implementation fields of scalar/transparent wrappers.
- A default reference names only a compiled factory and a relative path.
  `Root` uses `Config::default`; `CustomTheme` describes a field when the
  otherwise absent optional theme exists; `KeyCommand` and `Peer` describe
  fields when a collection entry exists. `parent_absent` distinguishes entry
  defaults from an entry or optional section actually present in the root.
  An empty, skip-serialized command array is explicit. A declared Option
  omitted by serde is normalized to null at its checked factory path. Opaque defaults cannot
  be resolved by this catalog. Defaults are not read from a user file, and
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
  `Opaque` is unreadable pending 2b. User-chosen map keys have their own
  `PotentialSecret` class, including `ui.agent_aliases`,
  `session.restart_agents` and `spawn.env`; key spelling is not safe metadata.
- `LocalOperatorFile` describes current file ownership, not permission for a
  future socket/mesh write. Transport writes need the ADR's separate authority
  gates. This catalog changes no mutation behavior.
- Reload ownership records the current application entry point: `AppLive`
  follows `App::apply_live_config`; `AppAndClient` also follows the local
  client's sound/focus reload; `ClientLive` is the client's window-title gate;
  `FuturePane` applies terminal policy to subsequently created panes.
  `ProcessStartup` includes the socket account policy, storage, system-one
  settings, reporter commands and the sampler disk path, which that app reload
  does not retarget.
  `CommandStartup` covers listener/command startup settings. `ReadOnUse` is
  the advertised icon's config lookup. `Mixed` is a container with multiple
  owners; `Opaque` is unresolved until 2b. Ownership is not a claim that every
  cached downstream consumer hot-reloads. Peer-row reconciliation also depends
  on the app's valid-UI gate. Existing ownership limitations stay tracked by
  #2384 rather than being silently changed in this inventory slice.

The JSON is reviewed record data, not generated at build time. A declaration
change requires an explicit classification/default/source/owner review; never
regenerate a public classification for an unknown field merely to pass coverage.
