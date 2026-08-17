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
| [0004](0004-per-repo-config-layer.md) | Per-repo configuration: a committed `.flk.toml` policy layer for repo facts | Proposed |
| [0005](0005-durable-event-log.md) | Durable event log: append-only JSONL as the fleet's audit substrate | Accepted |
| [0006](0006-message-addressing.md) | Message addressing: pane and repo-scoped targets for pane-to-pane messaging | Accepted |
| [0007](0007-peer-io-isolation.md) | Peer I/O isolation: no blocking syscall or Drop on the client render loop | Accepted |
| [0008](0008-agent-message-delivery.md) | Agent-to-agent messages ride the tool surface, not the keyboard | Proposed |
| [0009](0009-fleet-transport.md) | Fleet transport: one held SSH connection per peer, not a replicated log | Proposed |
| [0010](0010-report-composition.md) | Bug reports compose locally and are submitted by a human, never by the binary | Proposed |
| [0011](0011-conversation-first-gui.md) | A conversation-first GUI: surfaces, transports, and the write model | Proposed |
| [0012](0012-conversation-read-model.md) | The conversation read model: canonical entries and a derived index | Proposed |

## Conventions

- Next id = highest existing + 1, zero-padded to four digits;
  `docs/adr/NNNN-kebab-slug.md`.
- Header lines: `- Status:` (`Proposed` / `Accepted` / `Superseded by NNNN`),
  `- Date:`, `- Issues:`, `- Decision owner:`.
- Keep this index table in sync when adding or re-statusing an ADR — the
  gate keys on the Status column here, not on the ADR files.
