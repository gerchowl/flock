# ADR-0041: Configuration v2: 12-factor precedence and a typed, scoped config API

- Status: Accepted (owner decision 2026-10-24)
- Date: 2026-10-24
- Issues: [#2367](https://github.com/gerchowl/flock/issues/2367)
- Decision owner: repository owner, [decision on #2367](https://github.com/gerchowl/flock/issues/2367#issuecomment-6388177147)
- Supersedes: ADR-0002's env-below-file precedence, including resolved question 1. Its other write-target and reload decisions remain in force.
- Builds on: ADR-0004 (repo policy), ADR-0036 §1a, §5–§6 (authority, S6 scopes and S7 wire), ADR-0038 (GUI workbench).
- Implemented: none of this new API or precedence change. Existing config loading, write-target selection and reload behavior are described below as the baseline, not as newly shipped behavior.

## Context

The surface audit on #2367 found only `server.reload_config`, no typed config
read/write methods on the socket or S7 wire, no secret classifications, and an
env layer below files. Serializing `Config::default()` to discover scalar
leaves misses absent optional values and arrays. The blocklist is an env
admission policy, not a complete schema or an authorization mechanism.

At this decision's code baseline, `src/config/io.rs::load_with_overlay` applies
Rust defaults, generic env overrides, the base file and the local overlay in
that order. `FLOCK_CONFIG_PATH` selects the base file and its adjacent overlay.
Otherwise `config_dir()` uses `XDG_CONFIG_HOME`, then `HOME`, with the existing
fallback when neither exists. This is one selected base, not a system file
followed by a separate user file. CLI overrides exist for particular commands,
including `flk web`, rather than for every config key.

ADR-0004's `.flk.toml` and `.flk.local.toml` project-file stack is still not
implemented. It must not be presented as an existing source of effective
values. `advanced.fleet_config_source` is also an ADR-0002 direction rather
than a field in today's `Config`.

Reload has a separate honesty problem: a valid edit can be persisted without
changing its running consumer. [#2043](https://github.com/gerchowl/flock/issues/2043),
[#2064](https://github.com/gerchowl/flock/pull/2064) and
[#2066](https://github.com/gerchowl/flock/issues/2066) describe admission-policy
application, per-section outcomes, startup baselines and mismatches between a
reported outcome and the actual applied value. Their work must be reused,
not replaced by a second reload implementation.

## Decision

### 1. One precedence order, with provenance

For layers implemented today, later wins:

```text
Rust defaults < selected config.toml < adjacent config.local.toml
              < admitted FLOCK_<UPPER_SNAKE_PATH> env values
              < explicit command-specific CLI flags
```

The same order applies at startup and reload, including malformed-overlay
fallback paths. The loader retains its section validation and keep-current
behavior on rejection. Env values do not bypass validation or the env
blocklist. A startup process's environment is not changed by a file write or
by a GUI request. A CLI override belongs to the process/command that supplied
it, not to every subsequent socket caller.

This decision adds no system or project file. When ADR-0004 is implemented,
its policy domains and additive safety floors must be reconciled explicitly
with this order. Env remains above admitted file preferences, but cannot
weaken a non-overridable safety floor.

Each effective leaf has provenance: winning layer, canonical key, and an
appropriate source identifier (env variable name, CLI option, or file layer).
A read distinguishes the resolved candidate from the running value when a
section is rejected or requires restart. Shadowed layers and diagnostics
explain why editing a file may not change the effective value. Source metadata
is redacted with the value when it would disclose secret material or a
sensitive credential path. Raw env strings, TOML snippets and parser errors
must not leak classified data through provenance or diagnostics.

### 2. Inventory before exposure

The first implementation slice inventories every declared config leaf,
including imported types, and assigns a classification. Discovery by field
name alone is insufficient. A new or unclassified leaf must not become
readable by default. The inventory records type, nullability, source support,
validation, sensitivity, write authority and reload owner.

The source audit for this ADR searched `src/config/` for token, credential,
secret, password, API-key, URL and command fields, then followed `Config`'s
imported types. It found no dedicated inline password/API-key/bearer field, but imported
check configuration contains arbitrary env values and argv that can hold
credentials.
Today's credential-related and potentially secret-bearing surfaces are:

| Key or family | Source and meaning | Classification work required before reads |
| --- | --- | --- |
| `structured.token_file` | `Config.structured` imports `StructuredConfig` from `src/structured/launch.rs`; an absolute setup-token path, not token bytes | Credential reference. Never dereference for config reads. Classify path disclosure and redact for read-scoped tokens. |
| `structured.profile`, `structured.run_as`, `security.agent_account` | Profile selector and account/authority policy; file-only env policy | Security-sensitive metadata, not bearer secrets. Document read visibility separately from mutation authority. |
| `spawn.env.<agent>` | `src/config/spawn_env.rs`: lists of env variable names carried to children, not their values | Credential-routing metadata. Never expand or return the referenced process environment. |
| `checks.script[].env`, `checks.script[].args` | `Config.checks` imports `src/checks/config.rs`; arbitrary child env values and argv, unlike the names-only `spawn.env` | Potential literal credentials. Classify/redact values before reads; never infer safety from the env key or array shape. |
| `systemone.urls`, `systemone.full_urls` | `src/config/model.rs`: endpoint strings in arrays | May contain userinfo or query credentials. Classify the whole value or provide a proven safe projection before read exposure. |
| `thermal_command`, `gpu_command`, `keys.command[].command`, `peers[].relay_command` | Optional reporter commands, custom commands and relay commands | Free-form commands can embed secrets. Inventory must choose safe redaction before exposing their values, including defaults and shadowed layers. |
| `peers[].ssh`, `web.allowed_users`, `channels[]` | Destinations, identity allowlists, channel names/owners/targets/scopes | Identity and routing metadata, not authentication tokens. Classify privacy independently of terminal-safe rendering. |

Channel tokens live in the private channel-token store, client bearer tokens
in the client-token store, and harness credentials outside `Config`. They are
not new config keys and are never read by `config.get`. The same applies to
credentials reached via a setup-token file or carried child environment.
The inventory must follow all imported config types and free-form strings,
not treat this initial list as an exhaustive allowlist.

### 3. Typed schema and reads on both transports

Add read-only `config.schema` and `config.get` to the socket API and the S7
WebSocket wire. They use one server-owned model, not separate serializer
walks. The GUI consumes this schema and these values (ADR-0038). Types are
generated through ADR-0036 §6, and unsupported operations get typed refusals.
This ADR fixes semantics; final request/response DTO spelling is a slice-level
review item, not an existing protocol guarantee.

Schema entries describe canonical paths, section, scalar type or structured
shape, optionality, defaults, validation/enum constraints, classification,
supported source layers, env mapping or its absence/reason, write eligibility,
and reload classification. A schema is not permission to disclose a secret
default. `config.get` returns typed values plus provenance and running-state
information. A redacted value has an explicit redacted state, distinct from
null, missing and the literal string `"redacted"`. Its public type may remain
visible without revealing bytes or their length.

| Shape | Representation and documentation |
| --- | --- |
| Required scalar | Typed boolean, integer, float, string or enum, with limits and default; never a stringly typed TOML fragment. |
| Optional scalar | The underlying type plus nullable/optional metadata; null means no value in that layer, not an empty string or the text `"None"`. Document removal from a file separately from the resulting lower-layer value. TOML has no null literal. |
| Array or map | Typed collection with item/value shape and merge semantics, documented even while generic env mapping or editing is unsupported. Existing arrays append across file layers; an empty overlay array does not clear a base list. |
| Blocklisted leaf | Listed with type, reason and source restrictions, not silently omitted. Env blocklisting does not itself decide socket read or operator-write permission. |

The generic env implementation currently supports only scalar leaves present
in serialized defaults. Absent `Option` leaves (for example `icon`,
`client.bind` and `ui.mouse_scroll_lines`) must still appear in the schema.
Representing them does not silently enable an env mapping or array mutation.
The first write surface remains admitted scalars; collection edits require a
separate explicit contract, with file editing available meanwhile.

The current env blocklist is preserved: `keys`, `peers`, `spawn.env`,
`structured`, `security`, `theme.custom`, `ui.agent_aliases`, `ui.disk_path`,
and `ui.sound.path`, `ui.sound.done_path`, `ui.sound.request_path`,
`ui.sound.all_clear_path`. Reasons include structured collection
semantics, config-relative paths and explicit security policy. Every omission
must be documented by the schema rather than inferred from missing entries.
The web-only CSV list-env exceptions for allowed origins/users must be
identified explicitly during migration, not mistaken for generic coverage.

### 4. Authority and secret redaction

| Caller | `config.schema` / `config.get` | `config.set` |
| --- | --- | --- |
| Authenticated WS client with `read` | Classified, redacted view | Refused without `operate`. |
| Authenticated WS client with `operate` | Needs `read` as well to use read methods | Operator mutation, subject to per-key restrictions and validation. |
| Local socket operator established by ADR-0036 §1a | Read model | Operator mutation via the same writer. |
| Other permitted socket read caller | Redacted view under existing read authorization | Refused as operator-only. |

The existing scopes are `read`, `send`, `answer`, `operate`; there is no
`admin` scope, and `operate` does not imply the other three. A GUI editor
therefore needs explicit `read` and `operate`. `send` and `answer` grant no
config authority. Token validity and scope are checked at admission, including
cached command-result access, following S6/S7 rules. An agent principal must
not gain operator authority through config operations. No loopback trust or
new credential-issuance path is introduced.

Secrets are redacted before either transport serializes replies, including
schema defaults, provenance, rejected-value diagnostics and GUI snapshots.
Errors and logs carry typed reasons, never secret inputs. The inventory is a
blocking prerequisite for enabling any read surface. Operator write authority
does not implicitly grant a new secret-export operation.

### 5. Operator writes and honest reload outcomes

`config.set` writes an admitted typed scalar to a chosen writable file layer,
then invokes the existing validation/reload path. It is operator-only on the
socket and requires `operate` on the WS wire. It never edits env or CLI state.
All file selection goes through `resolve_write_target`: today a regular base
file is selected directly, while a symlink or absent base routes to its
adjacent overlay. Extend that resolver for explicit layer selection rather
than bypassing its managed-file policy. Return the actual selected layer and
any redirection. Arbitrary client-supplied filesystem paths are not layers.

Validate before persistence; rejected values preserve the file and running
state. Persist safely through the shared config writer. Report persistence
failure separately from reload failure: a file successfully saved but not
applied must never be presented as rolled back or wholly applied. An env/CLI
winner can shadow a successful file edit; return the resulting provenance so
the GUI can explain it.

For every affected section return exactly one outcome:

- `applied`: the running consumer accepted the effective configuration.
- `restart_required`: the section cannot be applied live; retain the running
  value and identify that a restart is needed.
- `rejected(reason)`: validation or application refused that section, with a
  typed reason and the running value retained.

Do not silently skip a section. Reuse #2043/#2064/#2066's classifications and
startup-baseline logic, translating an existing `invalid` section outcome to
this API's rejected form if necessary. An outcome must match what the
consumer actually did (including `name`), not merely what was parsed. A
restart-required difference persists across reloads until restart; document
that repeated partial result. Live policy changes must retain existing
sessions' admitted snapshots and security boundaries. Exact classifications
for every section are verified in the implementation inventory, not guessed
by this ADR.

### 6. Per-node ownership and GUI

Config belongs to one node, not to a synchronized fleet-wide document. The
initial methods address the node serving the request. The GUI shows that
node, winning layers, redacted/unsupported fields, validation and reload
outcomes. It must not report a masked edit as an active change or offer a
restart-required edit as a live toggle.

Pre-wire addressing for an optional stable `node_id`, aligned with
[#2366](https://github.com/gerchowl/flock/issues/2366) and ADR-0038's node view.
Remote config forwarding is future work: do not resolve by display name or
route alias, fan out mutations, or imply authority on a different node.
Transport authentication and write authority must be checked by the target
node when that feature is designed. Mesh VERSION stays 5; this ADR adds no
mesh config RPC. S7 capability/version negotiation must let older clients
and servers refuse unsupported config verbs without falling back to a
less-scoped method.

## Migration and compatibility

The precedence flip is a separately released implementation slice. Before
changing it, document both orders, audit service launch environments and
show shadowed file values with provenance. Example: a file's
`ui.sound.enabled = true` currently beats
`FLOCK_UI_SOUND_ENABLED=false`; after the flip the env value wins. Unset the
env variable to restore file control, or move deployment intent into the env
layer. Deprecated `FLOCK_DISABLE_SOUND` / `FLOCK_HOST_NAME` aliases retain
their existing diagnostics and canonical-name precedence within the env
layer until their separately documented removal.

The flip must cover startup, reload, absent files and malformed-overlay
fallback, with table-driven tests and content-safe diagnostics. Invalid env
input remains diagnosed and skipped rather than coercing types or discarding
a running config. Document existing web CSV exceptions and any separately
approved change to them. No new project/system layer or widened env admission
is part of this migration. Old binaries retain their old precedence; mixed
versions must expose their own schema/capability and provenance, never imply
that fleet members have identical effective configuration.

## Rejected options

- Keep env below files: rejected by the owner in favor of deployment-friendly
  12-factor precedence, superseding ADR-0002 Q1.
- Env-only configuration: cannot represent the existing file structures or
  replace the operator's managed-file workflow.
- Serialize the whole `Config` directly: would expose unclassified strings,
  hide absent optional leaves and lose provenance and redaction distinctions.
- A GUI-specific config store or writer: would create a second authority and
  bypass `resolve_write_target`, validation and reload ownership.
- Treat `operate` as an implicit `admin`/read grant: contradicts S6's explicit
  scopes and would broaden credential access.
- Return success for skipped reload sections: reproduces #2043's misleading
  running state. Explicit section outcomes are required.

## Open questions

- Exact DTO names, schema versioning and capability fields are to be reviewed
  in the read/wire slices while preserving the semantics above.
- Whether an operator needs any unredacted secret metadata beyond the
  redacted read model, and its explicit authorization, is not decided. No
  secret export is authorized by this ADR.
- The inventory must settle safe projections versus full redaction for
  arbitrary commands, URL credentials and identity metadata before reads ship.
- Explicit write-layer selection, optional-value removal and concurrent-edit
  conflict detection need a precise request contract in the write slice.
- Future project-policy layering (ADR-0004), remote config forwarding and
  collection editing are outside these slices and require their own decisions.

## Ordered implementation slices

Execute in the following order. Each issue carries acceptance criteria and
is part of #2367; this ADR's acceptance does not claim implementation.

1. [config v2: inventory and classify every configuration leaf](https://github.com/gerchowl/flock/issues/2384).
2. [config v2: typed config.schema and config.get on the socket](https://github.com/gerchowl/flock/issues/2385).
3. [config v2: expose scoped and redacted config reads on the S7 wire](https://github.com/gerchowl/flock/issues/2386).
4. [config v2: operator config.set with per-section reload outcomes](https://github.com/gerchowl/flock/issues/2387).
5. [config v2: move admitted env overrides above files with migration](https://github.com/gerchowl/flock/issues/2388).
6. [gui: schema-driven per-node configuration panel](https://github.com/gerchowl/flock/issues/2389).
