# Architecture Decision Records

Sequentially numbered; a decided ADR is immutable — supersede, don't edit.
The `adr-matrix` gate requires every **Accepted** ADR to be cited (as
`ADR-NNNN`) in the repo-root [FEATURE-MATRIX.md](../../FEATURE-MATRIX.md);
Proposed (roadmap) and Superseded rows never trip it. Non-feature decisions
can be exempted in `guardrails-adr-exempt.txt`.

## Index

| ADR | Title | Status |
| --- | ----- | ------ |
| [0001](0001-web-bridge-hosting-and-transport.md) | Web terminal bridge: hosting topology, transport, and gossip freshness | Accepted |
| [0002](0002-twelve-factor-config.md) | Twelve-factor configuration: four layers, one write target, one live source | Accepted |
| [0003](0003-command-brand-split.md) | Command/brand split: executable is `flk`, product stays `flock` | Accepted |
| [0004](0004-per-repo-config-layer.md) | Per-repo configuration: a committed `.flk.toml` policy layer for repo facts | Accepted |
| [0005](0005-durable-event-log.md) | Durable event log: append-only JSONL as the fleet's audit substrate | Accepted |
| [0006](0006-message-addressing.md) | Message addressing: pane and repo-scoped targets for pane-to-pane messaging | Accepted |
| [0007](0007-peer-io-isolation.md) | Peer I/O isolation: no blocking syscall or Drop on the client render loop | Accepted |
| [0008](0008-agent-message-delivery.md) | Agent-to-agent messages ride the tool surface, not the keyboard | Accepted |
| [0009](0009-fleet-transport.md) | Fleet transport: one held SSH connection per peer, not a replicated log | Accepted |
| [0010](0010-report-composition.md) | Bug reports compose locally and are submitted by a human, never by the binary | Accepted |
| [0011](0011-conversation-first-gui.md) | A conversation-first GUI: surfaces, transports, and the write model | Accepted |
| [0012](0012-conversation-read-model.md) | The conversation read model: canonical entries and a derived index | Accepted |
| [0013](0013-node-declared-thermal-health.md) | Thermal health is a host-declared ordinal, rendered as colour on glyphs that already exist | Accepted |
| [0014](0014-agent-initiated-agent-spawn.md) | An agent may start an agent, but only through a narrowed verb with a lineage-aware ceiling | Accepted |
| [0015](0015-operator-initiated-cross-repo-issue-filing.md) | The operator may file an issue from flock, over the API, into any repo their own token can reach | Accepted |
| [0016](0016-operator-notification-log.md) | Outcomes are filed as durable events; unread is a projection, not a second store | Accepted |
| [0017](0017-mcp-resource-surface.md) | Handed-over files are MCP resources with a durable identity; tools stay for parameterised calls | Accepted |
| [0018](0018-message-delivery-response.md) | A message says how much it needs, a recipient can say "not now" and must say so, and an idle agent is reachable | Accepted |
| [0019](0019-channel-push-delivery.md) | Agent mail may also arrive as a Claude Code channel push, as a first knock over the pull that stays the source of truth | Accepted |
| [0020](0020-agent-kind-is-a-type.md) | The spawn agent kind is a type, not an allowlist; it gains opencode, and a caller may never name a profile | Accepted |
| [0021](0021-model-registry-and-tier-matrix.md) | A model is a named capability the agent chooses from a declared set, and a tier is a property of the task rather than of the call | Accepted |
| [0022](0022-adr-0016-implemented-unaccepted.md) | ADR-0016 shipped without its decision; the log exists and §5 is now the operator's answer to give | Accepted |
| [0023](0023-decisions-and-agent-filing.md) | A decision is a record with an answer, and an agent may raise one | Accepted |
| [0024](0024-accept-the-implemented-proposed-adrs.md) | The eleven implemented `Proposed` ADRs are decided; what each acceptance does and does not assert | Accepted |
| [0025](0025-version-identity-and-release-automation.md) | Version identity: this project restarts its version line, and the deterministic `just release` path stays | Accepted |
| [0026](0026-mesh-fleet-transport.md) | Mesh fleet transport and durable message custody (implemented: v1.0.0, #623 / #661) | Accepted |

ADR-0026 supersedes the affected transport, reply, deferral and deduplication
contracts in ADR-0006, ADR-0008, ADR-0009, ADR-0018 and ADR-0019. Their other
decisions remain accepted; each carries a mesh amendment cross-reference.

## Conventions

- Next id = highest existing + 1, zero-padded to four digits;
  `docs/adr/NNNN-kebab-slug.md`.
- Header lines: `- Status:` (`Proposed` / `Accepted` / `Superseded by NNNN`),
  `- Date:`, `- Issues:`, `- Decision owner:`.
- `Accepted` claims a decision, never code. An ADR that needs to say what its
  acceptance covers carries `- Implemented:` (what shipped, and what did not) or
  `- Scope of acceptance:` (where the status *is* an answer, as ADR-0019's is).
  ADR-0022 §4 established the first of those as the way to mark a `Proposed` ADR
  with code behind it; ADR-0024 §3 keeps it after the sweep made it moot as a
  status mechanism.
- Keep this index table in sync when adding or re-statusing an ADR — the
  gate keys on the Status column here, not on the ADR files.
