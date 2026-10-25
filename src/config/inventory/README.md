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

## Mandatory safe projection — slice 3a

`projection` supplies opaque serializable defaults, candidate/running snapshots
and provenance under ADR-0041 §4. Constructors for output nodes are private;
there is no raw-value accessor, caller-role selector or raw-export option.
Every caller uses the same redaction. No socket, mesh, CLI read or GUI API is
exposed by this slice. The approved [3a → 3b split](https://github.com/gerchowl/flock/issues/2384#issuecomment-6390720190)
keeps existing diagnostic producers, `key=` redaction, returned/logged/CLI
sinks and final acceptance closure assigned to #2384 slice 3b. #2385 remains
gated on that completion; this is not a claim that existing diagnostics are
already content-safe.

`content_kind` records semantic purpose separately from sensitivity and
mutation authority: credential reference, names-only environment routing,
executable selector, repository metadata, endpoint, free-form command,
possible literal credentials, identity or general content. These categories
never make a sensitive value readable. In particular, `structured.token_file`
is a reference, not token bytes; `spawn.env` is names, not expanded values;
and script argv/env may carry literal credentials. Channel/client token stores
are outside Config and never traversed. A supplied unknown store key is
redacted like any other unknown field.

Sensitive/opaque values are replaced before serialization or Debug formatting.
Sensitive maps are redacted as a whole, so neither user keys nor their values
escape. Aggregate sections/arrays retain classified descendants. Unknown keys
are not echoed: an object's `unclassified` flag means some unknown input was
redacted. Public values must match a supported numeric/bool/nullable type or
a closed unit representation. Public arrays validate each item. Unknown types,
wrong shapes, out-of-range numbers, conflicting alias/canonical assignments and
unvalidated custom scalar encodings are withheld rather than copied. This
conservative handling can hide a valid custom scalar until its safe encoding
is explicitly supported; no raw fallback exists.

Defaults cover every declared schema path, including entry/variant contexts
absent from the root and fields with no independent default (`Required`).
`parent_absent` is checked across every factory, including global versus
user-keyed restart policies. Candidate and running snapshots remain separate;
callers must supply their actual respective states rather than assuming a
successful parse applied every setting. Existing reload limits, including
#2397 and cached consumers, remain unchanged.

Provenance takes canonical schema paths (including `[]`/`<key>` patterns),
not user instance keys. File paths and arbitrary source identifiers are never
copied. Classified public fields retain closed winner/shadowed layer kinds;
an env identifier is emitted only when it exactly matches the catalog's
computed generic env name, and is rebuilt from that metadata. Sensitive fields
hide their provenance. Unknown instance paths are withheld, not echoed.
Neither references nor source identifiers are dereferenced, executed or
expanded. Probes vary actual credential-file contents and process env values
and check that serialized/Debug output is invariant and contains no marker.

Map wrappers now require an explicit `map_wrapper` record, source-checked as a
transparent one-field map struct; a single-member enum cannot qualify by
accident. Reload evidence includes nested, renamed and indexed copy targets.
The reverse ownership guard rejects a direct startup-only target or a copy
inside a startup-only container. Copying a mixed parent does not claim that
all cached child consumers retarget: the existing whole-UI copy does not move
the sampler's startup-owned disk path. A direct disk-path copy still demands
an ownership review.

### Diagnostic sink projection (slice 3b1)

ADR-0041 §4's internal diagnostic boundary now constructs closed causes,
source categories, outcomes and numeric positions before formatting. Raw parser
messages, I/O messages, file paths and input values are never retained in the
safe diagnostic. `Display` and `Debug` only see safe data. Existing loader and
CLI String consumers format that safe object rather than scrubbing messages.

Diagnostic keys are rebuilt from catalog paths and aliases. User-map entries
become `<key>` and collection indices become `[]`; an unknown descendant stops
at its nearest declared container, and unknown roots become `document root`.
The four removed schema keys are an explicit closed historical exception.
Unknown user spellings are withheld even when they look harmless. This sacrifices
exact unknown-key/entry identification to prevent names from being a secret
channel; numeric source lines remain useful. File scans distinguish base and
overlay, while a merged unknown-key warning identifies the file category only.
TOML syntax locations retain exact one-based byte line/column positions when a
valid span exists. A missing span is unavailable. Key scans know only the line
and do not invent a column. I/O diagnostics retain `ErrorKind`, never `Display`.

Both input-document and edited-output parse refusals in `channel set` and
`config reset-keys` use this boundary and leave the file unchanged. Aggregate
provenance has a structural reason, separately from sensitive content kinds.
No socket/WS read method or raw export is enabled by this slice.

The approved slice 3b2 still owns keybinding, sound, spawn-env, checks and channel
validator producers and their immediate logging. Those existing diagnostics are
not claimed safe by 3b1. #2384 completion and #2385 remain gated on that work.
