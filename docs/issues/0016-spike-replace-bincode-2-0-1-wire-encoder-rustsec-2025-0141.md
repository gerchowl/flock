---
number: 16
title: "Spike: replace bincode 2.0.1 wire encoder (RUSTSEC-2025-0141)"
kind: issue
state: OPEN
author: gerchowl
labels: []
created: 2026-06-07T20:31:54Z
closed: 
url: https://github.com/gerchowl/herdr/issues/16
---

# Spike: replace bincode 2.0.1 wire encoder (RUSTSEC-2025-0141)

## Motivation / why

`bincode 2.0.1` is herdr's wire-protocol encoder for all server↔client communication (~30 call sites in `src/protocol/wire.rs`). Upstream development has been **permanently halted** ([RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141)) after a harassment incident; the code itself is complete and not vulnerable, but the project is a dead end:

- No security patches if a CVE lands later
- No Rust-evolution compatibility updates
- Dependency-graph hygiene (now requires an explicit `cargo deny` advisory ignore in `deny.toml`)

Herdr should be off bincode 2.0.1 within the next few releases. This issue spikes the *which-replacement* decision and the *how-to-migrate* path so the actual implementation PR can be uncontroversial.

The decision is sharpened by proposal #0002 (C′ symmetric peer federation): once federation lands, four machines (mba22, anvil, sage, ksb-meatgrinder) will run independently-versioned herdr servers gossiping with each other. **Wire-format stability across encoder versions stops being a nice-to-have and becomes a hard requirement.** Any candidate whose wire format drifts between encoder releases is disqualified.

## Decision / proposed approach

Three viable candidates, two real ones:

| Encoder | Wire stability | Hot-path speed | Payload size | Serde | Verdict |
|---|---|---|---|---|---|
| **wincode** (community fork of bincode 2.0) | byte-identical to bincode | same | same | yes | brownfield: lowest risk |
| **postcard** | stable across postcard versions (project commitment) | ~1.5x slower encode, similar decode | smallest (varint) | yes | greenfield: clean future |
| **bitcode** | unstable between bitcode releases | fastest | small | yes | disqualified by #0002 |
| **rkyv** | stable Archive, but not serde | fastest decode (zero-copy) | medium | no | out of scope (data-model rewrite) |

**Current best-guess decision:** postcard, **with** a Hello-handshake encoding-negotiation field so old and new clients can coexist without bumping `PROTOCOL_VERSION` (currently 12). This preserves \`herdr update --handoff\` UX. Smaller payloads also help over SSH/mosh — the user's actual remote-attach path.

**Fallback if encoding-negotiation is infeasible:** wincode, conditional on the spike confirming it's actually load-bearing-maintained (not one-volunteer-deep).

## What already exists

- `src/protocol/wire.rs:38-80` — all wire types use `#[derive(Serialize, Deserialize)]`. No custom encoder annotations.
- `src/protocol/wire.rs:532, 574` — the only two encode/decode call sites in production code; rest are test helpers.
- `src/protocol/wire.rs:16` — `PROTOCOL_VERSION = 12`. Bumped on incompatible changes.
- `src/protocol/wire.rs:20` — `MAX_FRAME_SIZE = 2 MB` (32 MB for graphics).
- Hello handshake (`ClientMessage::Hello`) already negotiates `RenderEncoding` and `ClientKeybindings` — there's a precedent for runtime feature negotiation.
- `src/logging.rs` — recently added latency instrumentation (`api_server_roundtrip_observed`, `pty_process_observed`, etc.) — gives us baseline measurements for any A/B.
- `deny.toml` — currently ignores RUSTSEC-2025-0141 with a pointer to this issue. Remove the ignore when migration completes.
- `docs/proposals/0002-symmetric-peer-federation.md` — the federation proposal that makes wire-stability a hard requirement.

## Scope

### P0 (this spike's outcome)

- [ ] Verify wincode's maintenance status (active maintainer count, release cadence, response time on issues — is it load-bearing or one-volunteer-deep?)
- [ ] Verify postcard's wire-stability claim (read the project's stability promise; look for past wire-format breaks across versions)
- [ ] Decide: postcard-with-handshake vs wincode-drop-in
- [ ] Confirm or refute the \"can negotiate encoding in Hello without bumping PROTOCOL_VERSION\" plan
- [ ] Empirical: benchmark encode/decode latency + payload size on a representative herdr message set (Hello, Input, render-frame, clipboard) — bincode vs postcard vs wincode

### P1 (implementation, separate PR)

- [ ] Replace the ~30 call sites in `src/protocol/wire.rs`
- [ ] Add encoder-selection to `ClientMessage::Hello` if going the postcard-with-handshake route
- [ ] Update unit tests
- [ ] Add a property test that round-trips every `ClientMessage`/`ServerMessage` variant through the chosen encoder
- [ ] Remove the RUSTSEC-2025-0141 ignore from `deny.toml`
- [ ] Update CHANGELOG

### P2 (follow-ups, filed as separate issues if surfaced)

- [ ] Documented wire format reference (consumer-facing — useful for non-Rust clients to the socket API)
- [ ] CI gate that catches accidental PROTOCOL_VERSION bumps without changelog entry

## Pitfalls

- **wincode bus factor.** If wincode is one maintainer deep, we'd be swapping one dead-end dep for a slow-motion dead-end dep. The whole reason to migrate is dep hygiene; choosing wincode without verifying its health re-introduces the same risk.
- **postcard wire-format claim drift.** Postcard advertises stability — but \"stable\" is project-defined. Need to verify by reading actual past releases / changelog for any wire-affecting changes.
- **Encoding negotiation backward-compat.** If we add an encoding field to Hello, old clients that don't send it must default to bincode (or fail closed); new servers must accept old clients. The version-negotiation logic needs to be water-tight or it bricks active sessions on first connect.
- **Mixed-version federation.** Even with stable encoder wire format, the *schema* (message types in `wire.rs`) evolves. Federation across peers on different herdr versions requires the schema to evolve in compatible ways (add fields with defaults, never reorder enum variants). This is orthogonal to the encoder choice but worth tracking.
- **Performance regression hiding in the noise.** Hot-path latency was just instrumented. Need to use those metrics for the A/B, not artificial microbenches in isolation.
- **\`MAX_FRAME_SIZE\` interaction.** Smaller payloads (postcard) make the 2 MB cap less likely to bite, but if any message currently fits within bincode's 2 MB but blows up via postcard's encoding choices for a specific type, it's a silent bug.

## Acceptance criteria

- [ ] A clear recommendation (one encoder, one rollout strategy) backed by:
  - Maintainer-health verification for the chosen encoder
  - Wire-stability verification across the encoder's recent version history
  - Empirical latency + payload measurements vs bincode baseline
- [ ] An updated implementation plan with a clear answer on whether PROTOCOL_VERSION needs to bump
- [ ] Documented decision trail (ADR or comparable artifact) explaining why the rejected options were rejected
- [ ] All P0 items checked off above

## Out of scope

- Redesigning the message types in \`src/protocol/wire.rs\`
- Changing the length-prefix framing layer
- Touching the version-negotiation handshake structurally (we may add a field, not redesign the mechanism)
- Migrating off serde

## References

- [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141) — bincode unmaintained advisory
- [docs/proposals/0001-flat-sessions-grouped-by-repo.md](https://github.com/gerchowl/herdr/blob/master/docs/proposals/0001-flat-sessions-grouped-by-repo.md) — flat sessions model
- [docs/proposals/0002-symmetric-peer-federation.md](https://github.com/gerchowl/herdr/blob/master/docs/proposals/0002-symmetric-peer-federation.md) — federation proposal that makes wire stability a hard requirement
- \`src/protocol/wire.rs\` — wire protocol module (all 30 call sites live here)
- \`src/logging.rs\` — recently-added hot-path latency instrumentation usable as A/B measurement infra
