# ADR 0003 — Command/brand split: executable is `flk`, product stays `flock`

- Status: Accepted
- Date: 2026-07-02
- Issues: #86
- Decision owner: human; execution reviewed against the issue scope.

## Context

The built binary was `flock`, which **collides with util-linux `flock(1)`** (file
locking) on Linux hosts. Downstream (g-fleet) already worked around this by
shipping the fork under the collision-free name **`flk`** (as a symlink) and
treating `flk` as the canonical fleet-wide command — but the fork still *emitted*
the literal string `flock` in three remote-peer command literals and throughout
its help/usage/error text. On a NixOS host where the fork was installed solely
as `flk` (util-linux owns `/…/bin/flock`), the federated servers-pane health
poll ran `sh -lc 'flock peers summary --json'` on the peer over SSH → it hit
util-linux `flock` → `flock: failed to execute summary: No such file or
directory` → the peer rendered permanently crossed-out/ghosted. Same latent
breakage existed in `peers logs` and `peers checkout-prepare`, and in the
remote-bootstrap discovery (`command -v flock`) + install suffix
(`.local/bin/flock`) — an `flk`-only peer was not discoverable without a
redundant install.

The three obvious alternatives all leave the wart in place:

1. **Keep emitting `flock` and require every host to install `flock` as a
   PATH shadow of util-linux.** — This is what the fork used to do; it forces
   every deployment to trip over the shadow, and the shadow itself is what the
   observed peer breakage was about. Rejected.
2. **Keep the fork's binary as `flock` and rely on `PATH` order to shadow
   util-linux.** — Same problem: on any host where the fork is intentionally
   installed as `flk` only (nix profile, HM), the emitted `flock` literal has
   no owner. Rejected.
3. **Wrap every `flock` self-reference in a runtime `current_exe()` lookup.** —
   Doesn't help the three remote command literals: they run on a peer over SSH,
   the local `current_exe()` is not the peer's binary. Rejected.

## Decision

Split the **command** from the **product/brand**:

- The executable name becomes `flk` (single-place change in
  `Cargo.toml [[bin]]`; the build system, help/usage, remote-peer literals,
  and remote-bootstrap discovery/install path all follow).
- The **repo, product, taglines, crate `pname` (`flock-ai`), `~/.config/flock`
  dirs, `FLOCK_*` env vars, socket/session file names, and log file names**
  stay as `flock`. That is the identity / namespace layer; `flock.log`,
  `flock.dev`, "flock — terminal workspace manager…", etc. are not commands.
- Test fixtures / workspace labels literally named `"flock"` (e.g.
  `src/api/schema.rs`, `src/app/api.rs`, `src/persist/*`, worktree tests using
  `/repo/flock`) stay: they are data, not commands.

Concretely renamed to `flk`:

- `Cargo.toml [[bin]] name`, `nix/package.nix mainProgram`, `flake.nix
  apps.default.program`, workflow `cp target/…/release/flock` steps, and
  `scripts/smoke_live_handoff_sessions.sh`'s `FLOCK_BIN` default.
- The three remote peer-command literals (`sh -lc 'flock peers …'`) —
  `src/config/model.rs` default `summary_command`, `src/peers.rs`
  `run_logs_command` and `run_checkout_prepare_command`.
- Remote-bootstrap identity (addendum in the issue): `src/remote.rs`
  discovery probes (`command -v flock` → `command -v flk`), the single-shot
  switch probe script, and `RemoteFlock::for_platform`'s `install_suffix`
  (`.local/bin/flock` → `.local/bin/flk`). An `flk`-only peer is now
  discovered without a redundant install; the installed binary is named `flk`.
- All user-facing help/usage/error text (`main.rs`, `cli.rs`, `session.rs`,
  `update.rs`, `remote.rs`, subcommand modules).
- Every asserting test (unit + integration) — `CARGO_BIN_EXE_flock` becomes
  `CARGO_BIN_EXE_flk`, and string assertions on help/error text follow.

Kept as `flock` (audit): brand prose in help ("flock — terminal workspace
manager for AI coding agents"), URLs (`flock.dev`, `github.com/gerchowl/flock`),
crate/package names (`flock-ai`, `flock`, `flock-web`), config/state paths
(`~/.config/flock`, `flock.log`, socket names), env vars (`FLOCK_*`), test
fixture workspace labels, comments/messages referring to "flock" as the
product / brand identity (e.g. "remote flock server", "the current local
flock binary"), release-manifest asset naming
(`flock-linux-x86_64` etc. — the download filename is a product identifier
that hosts rename to `flk` on install), and internal Rust identifiers
(`RemoteFlock`, `remote_flock`, `prepare_remote_flock`).

## Consequences

- `nix build .#default` produces `$out/bin/flk` (no `bin/flock`).
- `flk --version` prints `flk <version>` (the same tag the remote binary-match
  probe expects).
- The federated servers-pane health poll now runs `flk peers summary --json`
  on each peer; util-linux `flock(1)` is never shadowed anywhere.
- Downstream g-fleet can drop its `flk`-symlink package (`flkPkg`), its
  flock-only `summary_command` peer-mesh override, and the Macs' "install
  `flock` too" line. `flk` becomes the only command name on every host;
  `flock` survives purely as the repo/product/brand + config namespace.
- CI release/preview workflows still publish artifacts named `flock-<platform>`
  (product identifier); the copy step now sources them from
  `target/…/release/flk`.

## Follow-ups

- Once landed and the `latest` channel bumps, remove g-fleet's shim
  package + peer-mesh override.
- Nothing in this ADR touches the twelve-factor config layers (ADR-0002) or
  the logging redesign; the remote.* observability events emitted through the
  logging facade carry the new command name automatically because they are
  built from the (now renamed) `RemoteFlock` shell path and `--version`
  outputs.
