---
number: 76
title: "feat(client): connection slots — multi-connection client, warm-all, pointer-flip switching (#65)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T23:24:21Z
closed: 2026-06-12T00:17:38Z
merged: 2026-06-12T00:17:38Z
base: feat/sidebar-row-gap
head: connection-slots
url: https://github.com/gerchowl/herdr/pull/76
---

# feat(client): connection slots — multi-connection client, warm-all, pointer-flip switching (#65)

Implements #65 stage 1 (the federation-v2 capstone, tracked under #68). Stage 2 — live status over paused slots, retiring the ssh summary poller + snapshot staleness — is filed as #75.

## Architecture — what replaced what

The client becomes **multi-connection inside one process**. A *slot* is one framed server connection: the home unix socket, or a fleet peer reached over the existing ssh-stdio bridge (which already presents a remote server as a local forwarded socket — so every slot is uniformly a `UnixStream` to a socket path). The **active** slot feeds the painter and receives input; **warm** slots hold their transport with frames **PAUSED**. Switching servers **flips which slot is active in process** — the client never releases the terminal — instead of exiting and relaunching an attach leg.

- **`src/client/slots.rs`** — two layers. `SlotRegistry` is the pure flip / pause / resume / demote / cold-dial-backoff state machine plus `warm_all_targets` derivation (no I/O). `SlotManager` wraps it with the live warm/active `SlotConnection`s and turns registry effects into wire messages.
- **`src/client/mod.rs`** — when `[slots] enabled`, `run_client_loop` builds the manager, **background-dials the warm-all fleet** on a throttled timer sweep (each dial on a detached thread so a slow/failed dial never stalls the active paint path; failures back off and ghost), and on a `SwitchServer` to a **warm** slot performs the in-process flip — pause old, resume new (full redraw), swap the active write stream, retire the old reader and bind a new one — **without writing the launcher switch file**, so no relaunch leg spawns. Cold/unknown targets fall through to the existing exit-and-relaunch leg path (the #67 frozen-frame UX); a flip error demotes the slot and falls back. Transport death of a warm slot demotes it to cold silently (surfaced only on switch attempt). The `HeldRestoreGuard` (#72) still covers whatever exits.
- **Warm-all policy** — `warm_all_targets` = home (always, always warm) + config `[[peers]]` (a hub knows its fleet) + the carried fleet snapshot's peers/origin (a spoke learns its fleet from the #73 down-gossip), deduped and capped by `[slots] max`.

## Protocol decision

Additive `ClientMessage::SetFrameSubscription { enabled }` appended at the **end** of the enum (existing positional bincode variant indices unchanged), wire **proto 17→18**. A warm slot sends `enabled: false` so the server stops streaming frames (not relying on backpressure); the server filters paused clients out of `render_targets`. Resume (`enabled: true`) calls `request_full_redraw` so the slot repaints from a clean baseline. All handshake/protocol test literals bumped to 18 (per the #67/#73 convention). ssh keepalive (`ServerAliveInterval 15`) already holds paused slot transports alive — no app-level keepalive added.

## Rollout default

**`[slots] enabled` defaults FALSE** for this first lockstep deploy. The slots machinery is a large change to the live attach path; the leg model stays the default while the fork owner flips it on in config and dogfoods. `[slots] max` defaults to 8 (a generous sanity cap; warm-all is the policy). Flip to TRUE once #75 makes the slots the sole status/frame/snapshot path.

## Tests

- **Slot registry/manager (13 unit, `client::slots`)**: flip/pause/resume/demote/backoff; warm-all target derivation (dedup, home-first, cap); `SlotManager` in-process flip over real socketpairs proving switching to a warm slot — and switching **back** — returns a swapped stream with pause/resume on the wire and **no respawn**; cold flip returns None (relaunch fallback).
- **Pause stops frames (e2e, `client_mode`)**: `pause_subscription_stops_frames_and_resume_redraws` drives a real server — a paused slot receives no frames on fresh input; resume triggers a redraw frame.
- **Protocol**: `SetFrameSubscription` wire roundtrip; `render_targets` excludes a paused client and re-includes on resume.

### Suite results (devShell, `--test-threads=4`)
`peer_federation` 7/7 · `live_handoff` 16/16 · `client_mode` 18/18 · `--bin herdr` unit **2107**/0 · `detach_reattach` 11/11 · `cross_area` 9/9 · `multi_client` 11/11 · `server_headless` 16/16 · `api_ping` 10/10 · clippy `--all-targets` clean · fmt clean. (`cli_wrapper`/`auto_detect` are `not(target_os=macos)`-gated — compiled with the updated literals, run on Linux CI.)

Caution honored: no edits to sidebar render internals / agents panel — integration is confined to switch-request handling/state, client, protocol, and main.rs.

---

## Review-blocker fixes (commit e721dd4)

Three architecture-review blockers addressed; see the [fix comment](https://github.com/gerchowl/herdr/pull/76) for the full write-up.

- **Warm-slot death is now isolated** — reader threads slot-tag every event; a non-active slot's disconnect/shutdown demotes that slot silently, only the active slot's death is `ConnectionLost`.
- **Stale frames dropped at apply time** — the flip updates the active-slot tag before applying further events, so queued frames from the retired reader never paint over the new slot.
- **ssh warm-dial uses the real bridge socket** — the launcher hands the active leg's bridge socket to the client via `HERDR_ACTIVE_BRIDGE_SOCKET` (it can't recompute the launcher-pid-keyed path). **Stage-1 warm-all scope:** only home and the **active leg's** bridge have a live socket and are warm-dialed; **additional** fleet peers have no bridge yet and remain **cold-by-design** (a switch to them falls back to the exit-and-relaunch leg) — warming those over their own bridges is stage-2 scope (#75).

---

## Comments

### gerchowl — 2026-06-11T23:48:03Z

## Addressed the three review blockers (commit e721dd4)

**Blocker 1 — warm-slot death no longer kills the session.** Reader threads now tag every `ClientLoopEvent` with their slot key (`ServerMessage(slot_key, ..)`, `ServerDisconnected(slot_key)`). A non-active slot's disconnect or `ServerShutdown` routes to a silent registry demote (the designed ghost) via `SlotManager::handle_dead`; only the **active** slot's death keeps today's `ConnectionLost` teardown. The routing is a pure `slots::route_slot_event(event_slot, active, is_lifecycle_death)` helper so it's unit-tested directly.

**Blocker 2 — reader-rebind race no longer paints stale frames.** The flip now flips `active_slot_key` **before** any further event is applied, so a frame the old reader had already queued arrives tagged with the old slot's key and is dropped at **apply time** (not send time) instead of painting over the new slot's redraw. I deliberately did **not** `shutdown(Read)` the old fd: it's a `try_clone` sharing the underlying socket with the warm slot we keep for a switch-**back** flip, so shutting it down would kill that paused transport. The old reader stays blocked but harmless — the paused slot streams no new frames, and apply-time tagging drops anything in flight (matches the reviewer's fallback path).

**Blocker 3 — ssh warm-dial now uses the real bridge socket path.** The client is a separate child from the launcher (`run_remote`) that created the bridge socket keyed on the launcher's `std::process::id()`, so the client could never recompute the path — every ssh warm-dial missed. The launcher now passes the actual bridge socket explicitly via `HERDR_ACTIVE_BRIDGE_SOCKET` (the `HERDR_FLEET_SNAPSHOT` precedent). `slot_socket_path` now returns `Option`: home and the **active leg's** bridge are reachable; any **additional** peer has no bridge in stage 1 (#75) and returns `None` — cold-by-design, switch falls back to the relaunch leg. The warm-all dial sweep only attempts targets with a real reachable socket.

### Nits
- **proto literal**: `tests/api_ping.rs` asserted `protocol == 16` on a proto-17 base before this branch; the proto-18 commit on this PR corrects it to 18 (now consistent with the rest of the handshake/protocol literals).
- **New tests** (in `client::slots`, +5, unit suite 2107→2112):
  - `route_active_slot_frame_applies`, `route_stale_frame_from_old_slot_is_dropped`, `route_non_active_slot_death_demotes_not_drops`, `route_active_slot_death_applies_connection_lost` — the apply-time stale-frame + active-vs-warp routing matrix.
  - `warm_slot_death_demotes_and_session_survives` — kills a warm slot's socketpair peer, drives `handle_dead`, asserts the slot is demoted to cold **and** the active (home) slot is untouched (session survives), and a later switch re-dials it.

### Suite results (devShell, `--test-threads=4`)
`--bin herdr` unit **2112**/0 · `peer_federation` 7/7 · `live_handoff` 16/16 · `client_mode` 18/18 · clippy `--all-targets` clean · fmt clean.

