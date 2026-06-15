---
number: 52
title: "feat(client): attach retry during handoff + port client terminal theme to remote servers (#38, #47)"
kind: pr
state: MERGED
author: gerchowl
labels: []
created: 2026-06-11T10:17:10Z
closed: 2026-06-11T11:20:33Z
merged: 2026-06-11T11:20:33Z
base: feat/sidebar-row-gap
head: attach-ux
url: https://github.com/gerchowl/herdr/pull/52
---

# feat(client): attach retry during handoff + port client terminal theme to remote servers (#38, #47)

Closes #38, closes #47. The two issues share the attach path (client handshake, launcher legs, server accept), so they land together.

## #38 — client auto-retries the attach during a live-handoff window

- **Client retry loop** (`src/client/mod.rs`): every attach — direct `herdr`, `herdr client`, direct terminal attach, and each launcher leg (local and `--remote`) — funnels through `run_client_with_mode`, which now wraps the connect → handshake → session sequence in a retry loop. A refusal carrying the live-update notice opens a ~30s window with ~200ms pauses behind a single spinner status line (`| herdr: handoff in progress, reconnecting…`, rewritten in place on a tty, printed once otherwise). On success it attaches normally; at the deadline it prints the original error, exactly like today.
- **Window semantics**: inside the window, transient connect/handshake failures (the dying old server, the socket rebind gap, an older-protocol Welcome) are retried too. A rejection from a *newer* server short-circuits immediately to the upgrade guidance — retrying cannot succeed. A session that ran ≥5s before being refused earns a fresh window (a later, separate handoff); shorter refusal flaps keep draining the original budget so a flapping server cannot pin the client forever.
- **Server side** (`src/server/client_accept.rs`): connections drained during a handoff now receive a rejection `Welcome` carrying the notice instead of a silent drop. This is what lets a *fresh* attach (including a SwitchServer relaunch racing the handoff) see the notice pre-terminal-setup and wait — previously it died on an opaque EOF. The two existing `ServerShutdown` notice sites now share a `protocol::LIVE_HANDOFF_ATTACH_NOTICE` constant with the client matcher.
- **Launcher legs** (`src/main.rs`): no launcher-level retry needed — both legs end in the shared client code (local in-process, remote via the spawned `herdr client` subprocess, whose reconnects re-execute the SSH bridge connection per attempt). Documented on `run_attach_legs`.
- The stdin reader thread is now spawned once per client process and survives retry attempts, so no typed bytes are stranded in a session-scoped reader between attempts.

## #47 — remote attach ports the client's terminal theme

- **Capture** (`src/client/mod.rs`): just before the handshake the client queries OSC 10/11 itself (raw mode for ≤300ms; terminals answer in single-digit ms), parses the replies with the existing `raw_input` parser, and puts the result in the `Hello`. Any non-reply bytes read during capture are forwarded as the session's first input — nothing is lost. Capture runs **per attach leg**, so SwitchServer relaunches re-capture from the same host terminal with zero launcher plumbing (no env var needed).
- **Wire** (`src/protocol/wire.rs`): `Hello` gains `host_theme: Option<TerminalTheme>` next to #39's `fleet` field. `TerminalTheme`/`RgbColor` get serde derives (positional layout documented as wire format).
- **Adoption** (`src/server/headless.rs`): on app-client `ClientConnected`, a non-empty carried theme is adopted via the existing `App::set_host_terminal_theme`, which (a) makes NEW panes spawn with it and (b) pushes OSC 10/11 default-color updates into every EXISTING pane runtime (the same `apply_host_terminal_theme` write path used at spawn) so headless-spawned spoke panes repaint. Last attach wins (single-user fleet). Theme-less clients change nothing; direct terminal attaches never adopt. The per-client connection record also starts from the carried theme so the live OSC-reply input path dedups correctly.
- Local attach is unchanged in effect (the local server adopts the same theme it would have learned from the input path, just deterministically at attach time).

## Protocol decision: 14 → 15 (deliberate)

Bincode's positional encoding has no unknown-field tolerance, so the additive `Hello` field changes the wire shape. Protocol 14 (#39) is **unreleased in any tag** (latest preview tag carries 13), but it is already fleet-deployed from source — riding 14 would make a theme-bearing client misparse against those live servers with an opaque decode failure. Bumping to 15 gives the clean version-mismatch path instead; the fork owner controls all deployments, so lockstep redeploy applies. (This consciously supersedes the AGENTS.md "don't double-bump between releases" guidance for the fleet-deployed-from-source case, as anticipated in #47.)

During the 14→15 handoff itself, the running protocol-14 old server drains pending connections silently (old binary) — a client racing *that one* handoff still sees today's EOF error. Every handoff after this deploy gets the full retry behavior.

## Tests

- `wire.rs`: `Hello` round-trips with and without `host_theme` (theme handshake round-trip).
- `client_transport.rs`: a themed `Hello` arrives in `ClientConnected` with the theme intact.
- `client_accept.rs`: drained connections receive the notice `Welcome`.
- `headless.rs`: app attach adopts the carried theme for new panes **and** pushes it into an existing channel-runtime pane; theme-less attach keeps the prior theme; direct terminal attach never adopts.
- `client/mod.rs`: refusal recognition in both phases; retry window opens on refusal and retries transients; newer-server rejection bails immediately; window deadline stops retries; ≥5s session earns a fresh window; OSC capture-buffer parsing (incl. keystrokes mixed in).
- Integration: `tests/server_headless.rs` attaches to a real server with a themed `Hello` (guards the hand-rolled wire fixtures against bincode drift). All protocol-14 fixtures/expectations across `tests/` bumped to 15; support helpers now encode the theme option byte.

Full suite (`cargo test -- --test-threads=4`, macOS): **bin 2010 passed / 0 failed** (1 ignored); integration: api_ping 10, client_mode 16, cross_area 9, detach_reattach 11, live_handoff 16, multi_client 11, server_headless 16, peer_federation 5+1 — all green except:

- `peer_federation::switch_snapshot_renders_home_row_on_spoke_and_home_switches_back` — **pre-existing failure on the base branch**: it fails identically on a pristine `babcea6` (`feat/sidebar-row-gap` tip, no changes from this PR), timing out waiting for a peer workspace row to fold into the sidebar. Likely fallout from the #48/#50 sidebar rework; worth a separate issue.
- Two known parallel-load flakes (`detach_reattach::detached_output_preserves_last_attached_pty_size`, `client_mode::client_receives_notify_on_agent_state_change`) reproduced once each under `-j4` and pass in isolation/rerun.
- `auto_detect`/`cli_wrapper` are `cfg(not(macos))` and run 0 tests locally; their protocol-15 literal updates are included for Linux CI.

One existing test needed updating for new behavior (not a workaround): `client_mode::server_crash_after_attach_causes_lost_connection_error` treated *any* PTY byte as "client attached", which the pre-handshake OSC theme query now defeats; it now waits for rendered frame content, matching the cross-area crash test's heuristic.

`cargo fmt` + `cargo clippy --all-targets -- -D warnings` clean; guardrails commit gates pass.
