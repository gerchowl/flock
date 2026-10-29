/** Ordered checked S7 trace execution (#2467, ADR-0036 §5a).
 * Assertions precede receipts; server output and injected faults stay distinct.
 * A separate five-second guard bounds silence without advancing protocol time.
 */
import { readdir } from "node:fs/promises";

import type { fixtureExamples } from "../../src/gen/FixtureExamples";
import type { FixtureCoverage } from "../../src/gen/FixtureCoverage";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import { browserClock, type Clock } from "../../src/wire/env";
import { parseScript } from "./harness";

export type TraceScript = { readonly name: string; readonly lines: readonly FixtureLine[] };
export type TraceSource = "setup" | "client" | "server" | "client_fault" | "clock" | "observation";
export type CheckedStep = { readonly checked: true; readonly source: TraceSource };
export type StepHandler = (line: Readonly<FixtureLine>, index: number) => Promise<CheckedStep | undefined>;
export type StepHandlers = Partial<Record<FixtureLine["step"], StepHandler>>;
export type TraceFact = CheckedStep & { readonly index: number; readonly step: FixtureLine["step"] };
const CEILING_MS = 5_000;
const COVERAGE: readonly FixtureCoverage[] = ["s7-min", "s7", "client-only"];
const SOURCES: readonly TraceSource[] = ["setup", "client", "server", "client_fault", "clock", "observation"];

/** Exact unwired inventory; each later provider slice must shrink this set (#2467). */
export const PENDING_PART_2B = {
  "agent-gap": "pending part 2b", "agent-send": "pending part 2b",
  "clean": "pending part 2b", "heartbeat-missed": "pending part 2b",
  "permission-decision": "pending part 2b", "ping-pong": "pending part 2b",
  "token-revoked": "pending part 2b", "unknown-subscription": "pending part 2b",
} satisfies Partial<Record<keyof typeof fixtureExamples, "pending part 2b">>;

/** Every selected script belongs exactly once to a real provider or the pending set. */
export function assertInventory(scripts: readonly TraceScript[], wired: readonly string[]): void {
  const actual = [...wired, ...Object.keys(PENDING_PART_2B)].sort();
  const expected = scripts.map((script) => script.name).sort();
  if (!wired.length || new Set(actual).size !== actual.length || JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`unhandled or duplicate script: ${JSON.stringify({ expected, actual })}`);
  }
}

/** Read coverage at its authored source; no script-name allowlist exists. */
export function coverage(script: TraceScript): readonly FixtureCoverage[] {
  const header = script.lines[0];
  if (header?.v !== 1 || header.step !== "given") throw new Error(`${script.name}: expected v1 given`);
  const labels = header.covers ?? [];
  if (!labels.length || new Set(labels).size !== labels.length || labels.some((label) => !COVERAGE.includes(label))) {
    throw new Error(`${script.name}: invalid covers`);
  }
  if (labels.includes("s7-min") && labels.includes("client-only")) {
    throw new Error(`${script.name}: client-only cannot claim server s7-min coverage`);
  }
  return labels;
}

/** Load the same covers-containing selection as the server and client oracles. */
export async function loadS7MinScripts(root: string): Promise<readonly TraceScript[]> {
  const selected: TraceScript[] = [];
  for (const name of (await readdir(root)).sort()) {
    if (!name.endsWith(".jsonl")) continue;
    const script = { name: name.slice(0, -6), lines: parseScript(await Bun.file(`${root}/${name}`).text()) };
    if (coverage(script).includes("s7-min")) selected.push(script);
  }
  if (!selected.length) throw new Error("no authored s7-min scripts selected");
  return selected;
}

/** One ordered completion per authored index; an incomplete run cannot finish. */
export class ExecutionLedger {
  readonly #facts: TraceFact[] = [];
  constructor(readonly script: TraceScript) {}

  complete(index: number, result: CheckedStep): void {
    const line = this.script.lines[index];
    if (!line || index !== this.#facts.length) throw new Error(`${this.script.name}: out-of-order completion ${index}`);
    if (result.checked !== true || !SOURCES.includes(result.source)) throw new Error(`${this.script.name}: unchecked step ${index}`);
    // A send is either a real server expectation or an explicitly injected fault.
    if (line.step === "send" && result.source !== "server" && result.source !== "client_fault") {
      throw new Error(`${this.script.name}: send step ${index} has no server/fault provenance`);
    }
    if (line.step !== "send" && (result.source === "server" || result.source === "client_fault")) {
      throw new Error(`${this.script.name}: non-send step ${index} claims server/fault provenance`);
    }
    this.#facts.push(Object.freeze({ ...result, index, step: line.step }));
  }

  finish(): readonly TraceFact[] {
    if (this.#facts.length !== this.script.lines.length) {
      throw new Error(`${this.script.name}: unhandled step ${this.#facts.length + 1} of ${this.script.lines.length}`);
    }
    return Object.freeze([...this.#facts]);
  }
}

/** Prevent ports from changing authored expected values, including nested frames. */
function freeze(value: unknown): void {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) return;
  for (const child of Object.values(value)) freeze(child);
  Object.freeze(value);
}

async function bounded<T>(work: Promise<T>, clock: Pick<Clock, "setTimeout" | "clearTimeout">): Promise<T> {
  let timer: ReturnType<Clock["setTimeout"]> | undefined;
  const ceiling = new Promise<never>((_, reject) => {
    timer = clock.setTimeout(() => reject(new Error("asynchronous trace port stayed silent")), CEILING_MS);
  });
  try { return await Promise.race([work, ceiling]); }
  finally { if (timer !== undefined) clock.clearTimeout(timer); }
}

/**
 * Execute every step exactly once. Ports clean up resources on failure.
 * The injected guard clock is independent of virtual protocol time.
 */
export async function runTrace(
  script: TraceScript,
  handlers: StepHandlers,
  guardClock: Pick<Clock, "setTimeout" | "clearTimeout"> = browserClock,
): Promise<readonly TraceFact[]> {
  coverage(script);
  if (script.lines.at(-1)?.step !== "quiet") throw new Error(`${script.name}: missing final quiet`);
  freeze(script);
  const ledger = new ExecutionLedger(script);
  for (const [index, line] of script.lines.entries()) {
    try {
      if (line.v !== 1 || (index > 0 && line.step === "given")) throw new Error("invalid version or repeated given");
      const handler = Object.hasOwn(handlers, line.step) ? handlers[line.step] : undefined;
      if (typeof handler !== "function") throw new Error(`unhandled step ${line.step}`);
      const result = await bounded(handler(line, index), guardClock);
      if (result === undefined) throw new Error(`unhandled step ${line.step}: no checked receipt`);
      ledger.complete(index, result);
    } catch (error) {
      throw new Error(`${script.name}: step ${index + 1} (${line.step}): ${(error as Error).message}`, { cause: error });
    }
  }
  return ledger.finish();
}
