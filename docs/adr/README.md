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

## Conventions

- Next id = highest existing + 1, zero-padded to four digits;
  `docs/adr/NNNN-kebab-slug.md`.
- Header lines: `- Status:` (`Proposed` / `Accepted` / `Superseded by NNNN`),
  `- Date:`, `- Issues:`, `- Decision owner:`.
- Keep this index table in sync when adding or re-statusing an ADR — the
  gate keys on the Status column here, not on the ADR files.
