# ADR 0013 — Thermal health is a host-declared ordinal, rendered as colour on glyphs that already exist

- Status: Proposed
- Date: 2026-08-19
- Issues: #291 (gossip + render), #298 (the host reporter), #299 (the column
  budget that forced the rendering shape). Follows the self-declared-value
  pattern established by #164 (fleet icons) and #50 (per-node disk mount).
  Constrained by ADR-0001 (fleet gossip is pull) and by #284 (the
  no-hardcoded-tunables baseline).
- Decision owner: operator; design from four independent expert reviews on
  #291 (TUI density, wire protocol, sensor feasibility, maintenance burden)
  plus a second three-review round against the shipped code.

## Context

A fleet node — a Mac Studio running local MLX inference — sat with its fans at
full power for an extended period. From the servers band there was no way to
tell *"is it stuck, or just busy?"*; the only recourse was to ssh in and read
`ps`/`pmset`. That same box had previously hit a **47-minute SMC thermal
emergency and hard-powered off**.

The obvious response is "gossip the temperature". The question this ADR settles
is what, exactly, crosses the wire, and what flock is allowed to conclude from
it.

## What decided it

**Load metrics cannot express the failure.** CPU% answers "is it working hard".
It can never answer "is this box out of cooling headroom". The diagnostic
pattern is the *divergence* between the two: a hot glyph beside a busy value is
a machine doing its job, while a hot glyph beside an idle value is cooling
failure. Only the second is an incident, and no load metric shows it.

**Degrees are not comparable across a heterogeneous fleet.** 90 °C on Apple
silicon under load is normal; an RTX 5090 idles near 40 °C and is fine at 80 °C;
a microVM guest can read nothing at all. Any temperature threshold held by flock
would false-alarm on every inference run.

**The band has no columns left.** Measured, not assumed: a servers self row runs
~30–32 columns against ~24 available at the default sidebar width, so
`clamp_line` already truncates. Every variant that added text was dead on
arrival.

**Per-component sensors are not obtainable anyway.** `powermetrics` requires
root. SMC keys drift per M-generation and Pro/Max/Ultra variant. The MacBook Air
is fanless. microVM guests read nothing. A design demanding cpu°/gpu°/fan-RPM
per node cannot be filled on this fleet.

## Decision

1. **The host declares a coarse ordinal**, not a temperature: `0` nominal, `1`
   fair, `2` serious, `3` critical, plus which component it applies to
   (`cpu`/`gpu`/`node`) and a short free-text label. flock ships **no**
   temperature table and holds **zero** new thresholds — calibration lives with
   the host, where the platform knowledge already is. This keeps #284 from
   growing by ~18 tunables.

2. **Colour is the signal; the band spends no columns.** Metric glyphs were
   already rendered in a constant dim grey — an unused channel. Severity ≥ 2
   tints the declared component's glyph. Below that, a node renders
   byte-identically to one declaring nothing, so a nominal fleet looks exactly
   as it did and the one box in trouble still stands out.

3. **An alarm is never silently dropped.** When the declared component has no
   metric to tint — a Linux box reports no GPU utilization at all, a pre-#291
   peer gossips none, cpu is absent before the sampler's first delta — the
   severity gets its own thermometer glyph *leading* the line, because
   truncation eats from the right.

4. **The reporter is a host-configured command**, run on a slow tick behind a
   hard timeout, failing closed: missing binary, non-zero exit, unparseable
   output or timeout all declare *nothing*. A synthesized nominal reading would
   assert health nobody observed, which is worst precisely when a node is
   critical.

5. **Every boundary sanitizes** — bincode conversion, JSON summary parse,
   relayed entries two hops back, and the local reporter on the way out. A
   broken reporter must not reach a render pass with an out-of-range rank or an
   unbounded label.

## Alternatives considered

**Temperature next to every stat item (`cpu 42% 78°`, `gpu 37% 64°`).** The
original proposal, and the intuitive one. Rejected on four independent grounds:
it overruns even `sidebar_max_width` against a band that already truncates; it
needs ~18 thresholds (3 components × 3 platforms × amber/red), worsening #284;
the fields cannot be filled on a fanless MBA or in a microVM; and it costs a
`PROTOCOL_VERSION` bump per sensor type forever. The glyph tint preserves the
per-component *intent* at zero column cost.

**One numeric temperature per node (`anvil 72°`).** ~6 columns on every row to
show a number that is nominal almost always, and not comparable across machine
types without flock-side calibration it must not hold.

**A fully opaque host-declared display string.** flock could transport it but
could not colour a ramp from it — `"72°"` carries no severity. Some typed signal
is required; the ordinal is the smallest one that works.

**A continuous severity 0..=100.** Buys cross-fleet sorting and gradient
rendering that a glanceable strip does not need, at the cost of asking hosts to
invent a normalisation for a signal that is natively 4-valued on macOS
(`ProcessInfo.thermalState`).

**Fan RPM as a first-class field.** It is a *symptom* of thermal pressure, half
the fleet cannot report it (the MBA has no fans), and the operator noticed the
motivating incident **by ear**. Detail-view material at most.

**A file the host writes out of band, instead of a command.** Considered for
#298 because it lets a privileged helper own sensor access. Rejected: a dead
writer looks exactly like a healthy one, and closing that silent-staleness hole
costs an mtime-ceiling policy, while the fork cost at a 30s cadence is
negligible.

## Consequences

- A new machine type joins the fleet and reports thermal health **without a
  flock release** — it writes a reporter, flock is unchanged.
- flock gives up cross-fleet sortable numbers and native sparkline history. The
  servers band is a glanceable strip, not an APM dashboard; the trade is
  deliberate.
- Colour is the only channel in the band, so severity is not distinguishable by
  a red/green colourblind viewer. Accepted consciously; the accessible detail
  lives in the host-authored label and the expanded peer view (#291 P1).
- The reporter reimplements timeout/backoff/no-overlap locally. It is registered
  on #295 as a consumer to migrate when the general supervisor lands, so it does
  not become a divergent policy nobody remembers.
