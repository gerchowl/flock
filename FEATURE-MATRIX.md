# flock feature matrix

<!-- flock is the product/brand and repo name; the invoked executable is `flk`
     (ADR-0003). Feature rows describing user-typed commands cite `flk`. -->


Feature-level roll-up citing the decisions and issue clusters that own each
area. Every **Accepted** ADR must appear here (gate: `adr-matrix`); rows
without an ADR are areas that grew PR-by-PR — see `docs/PRs/README.md` for
the full archive.

| Area | Feature | Status | ADRs | Issues / PRs |
| ---- | ------- | ------ | ---- | ------------ |
| Web bridge | `flk web` — browser terminal over the tailnet (xterm.js, gossip freshness) | Shipped | ADR-0001 | #131, #109, #147–#151 |
| Command name | Executable renamed `flock` → `flk` (avoid util-linux `flock(1)` collision); brand/config/env stay `flock` | Shipped | ADR-0003 | #86 |
| Fleet | Symmetric peer federation, servers band, cross-host switching | Shipped | — | #18, #19, #34, #40, #86 |
| Remote | SSH stdio bridge, remote install/update, live handoff | Shipped | — | #52, #61 (PR), #72 (PR) |
| Client resilience | Per-slot writer actor: no blocking I/O on the render loop, off-loop bridge teardown, redial circuit breaker, active-slot degrade-to-home | In review | ADR-0007 | #176 |
| Panes / HUD | Reserved headers, status line, floating prompt, attention cycling | Shipped | — | #1, #5, #8, #12, #24 |
| Worktrees | Merge-gated kill, fleet sweep, workspace-as-unit | Shipped | — | #2, #4, #81, #83 |
| Config | Twelve-factor config: four layers, env convention, settings pane as shim, fleet-source write target (planned) | Shipped | ADR-0002 | #108, #112, docs/PRs |
| Governance | guardrails gates, clippy print funnel, trace-field debt registry | Shipped | — | #21, #22, docs/DEBT.md |
| Observability | JSONL logging spine, named-facade schema surface (raw trace-field debt census: 0) | Shipped | — | #87, docs/DEBT.md |
| Fleet control | `agent.fork` + `flk agent fork`, durable event log, `flk lineage` ancestry, pane-to-pane messaging | Shipping (#175 phases) | ADR-0005, ADR-0006 | #175, #177, #183 |
| Agent-initiated spawn | `flock_agent_start` (MCP) → narrowed `agent.spawn`, closed agent kind, lineage-aware ceiling (depth → fanout → `[fleet] max_concurrent_agents`) shared with `agent.fork` and gated on caller class rather than verb, pause-refused, environment cut to a per-agent allowlist, opening turn composed behind a flock-owned preamble with control bytes refused. `agent.spawn` off by default | In review | ADR-0014 | #329, #345, #347, #348, #349 |
