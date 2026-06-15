---
number: 39
title: "feat(federation): hub-and-spoke — client-carried fleet snapshot, home row, switch_home (#36)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T07:22:31Z
closed: 2026-06-11T07:46:38Z
merged: 2026-06-11T07:46:38Z
base: feat/sidebar-row-gap
head: hub-spoke
url: https://github.com/gerchowl/herdr/pull/39
---

# feat(federation): hub-and-spoke — client-carried fleet snapshot, home row, switch_home (#36)

Implements #36 — hub-and-spoke federation: the client carries its provenance and the hub's fleet view into every server switch, so a spoke renders the way home (and the rest of the fleet) with zero spoke-side ssh config.

## What landed

**Down-gossip snapshot at switch.** When a server emits `SwitchServer` it attaches a `FleetSnapshot`: origin host label + the peer summaries it holds (names, ssh targets, system health, workspace rollups, staleness ages). The hop target is excluded — it becomes the self row on the receiving end.

**Pass-through, never re-stamp.** A server that itself received a snapshot forwards it unchanged on the next leap (ages recomputed, origin untouched), so nested leaps keep the ORIGINAL origin. Only a directly-reached server (the hub) stamps a fresh snapshot from its own identity + polled `peer_summaries`.

**Handshake carries it.** The client records `{target, fleet}` in the launcher's switch file (now JSON); the launcher hands the snapshot to the next remote leg via `HERDR_FLEET_SNAPSHOT` (child-process env, never exported); the leg's `Hello` carries it to the new server. A CLI-launched `--remote` leg stamps an origin-only snapshot (local `short_host_name`), so even a direct leap knows the way home. Locally-attached clients send nothing — zero behavior change (monolithic + headless modes untouched).

**Spoke renders the snapshot.** Servers band order: `← mba22 home` (pinned origin row with snapshot age), self, carried snapshot rows (with explicit `… old` staleness chips, reachability decays — no reverse polling, structurally: the poll loop only iterates config-owned `state.peers`), then the server's own config peers. Selecting a snapshot row emits `SwitchServer` with that row's address; the client (living on the hub) re-attaches directly.

**Home semantics.** The home row resolves to the reserved `HOME_SWITCH_TARGET` (`"<home>"`); the client's launcher interprets it as `AttachLeg::Local` — the existing switch exit path, no new message, no `Mode` variant. A new `switch_home` keybind (`BindingConfig::empty` default, `toggle_collapse_all` pattern) does the same without the sidebar, answering "already home" when no origin was carried.

**Shared chokepoint.** `request_peer_switch` became an enum (`ConfigPeer` / `SnapshotPeer` / `Home`) consumed in BOTH deferred-request loops: headless resolves via `App::prepare_switch_server`, monolithic keeps its notice ("already home" for `Home`).

## ⚠ Deliberate protocol bump: 13 → 14

The wire is positional bincode with exact-version equality — no unknown-field tolerance, so the issue's fallback applies: additive `fleet` fields on `Hello`/`SwitchServer` + a deliberate `PROTOCOL_VERSION` bump. The bump itself is one constant; the churn is mechanical version literals in integration tests.

Two wire subtleties worth knowing:
- `api::schema::Peer*Summary` types use `serde(skip_serializing_if)`, which bincode cannot round-trip — the wire uses skip-free mirror types (`FleetSystem`, `FleetWorkspace`) with `From` conversions. Caught by a round-trip test before it shipped.
- A v13 client greeting a v14 server now fails Hello *decode* (shape changed) instead of getting the friendly version-mismatch `Welcome`; the out-of-band status-API protocol check (`autodetect` / `ensure_remote_server_running`) still produces the clean error path during attach.

## Deviations from the letter of the issue

- Snapshot self-dedup happens at **emit** (exclude the hop target) rather than render: receiver-side hostname matching would misfire when several servers share a host (exactly the e2e sandbox), and pass-through stays byte-faithful. Consequence: a nested leap drops the intermediate hop from the carried view — origin is always preserved.
- Snapshot peers render in the **servers band only**; their workspaces are not folded into the spaces list (that machinery indexes config-owned `peer_summaries`). The issue specifies the band; folding is a cheap follow-up if wanted.
- Each app-mode attach **replaces** the stored snapshot — including clearing it for origin-less attaches, so no stale home row survives a later local attach. Direct terminal attaches never touch it.

## Tests

- `cargo test` full suite green: **1945 unit** + all integration targets (`client_mode`, `detach_reattach`, `cross_area`, `multi_client`, `server_headless`, `live_handoff`, `auto_detect`, `api_ping`, `cli_wrapper`, `peer_federation`).
- New unit coverage: wire round-trip with a populated snapshot (Hello + SwitchServer), `PeerSummaryState` ⇄ `FleetPeer` freshness mapping (live stays live, stale decays, never-reached stays unreached), pass-through keeps origin + excludes hop target, home sentinel resolution (with/without origin), sidebar order home → self → snapshot → config, no home row without origin, snapshot-only spoke shows the band, staleness chips, switch-file JSON round-trip (incl. home sentinel + bare-target fallback), headless set/clear/terminal-attach-no-touch, `switch_home` chokepoint.
- e2e (`tests/peer_federation.rs`, extended): hub with two peers (one reachable via the fake-ssh shim, one unreachable "ghost") → clicking the remote row yields `SwitchServer` carrying `Some(fleet)` with ghost included and the hop target excluded → the raw fleet bytes are spliced verbatim into a fresh handshake against the spoke (exactly what the launcher does) → the spoke renders `← <origin> home` + the ghost row → clicking home yields `SwitchServer("<home>")` with no fleet.
- `cargo fmt` + `cargo clippy --all-targets` clean.

## g-fleet consequence (follow-up, per issue)

Only the hub generates `[[peers]]`; spokes get none; spoke→hub inbound keys can be dropped. Not part of this PR.
