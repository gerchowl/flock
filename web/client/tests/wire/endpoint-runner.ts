/**
 * Shared S7 trace execution and accounting (#2467, ADR-0036 §5a).
 *
 * A port must complete the action AND its semantic assertion before returning
 * a checked receipt. Missing handlers and unchecked receipts fail closed. The
 * ledger distinguishes observed server output from deliberate client faults.
 * Protocol deadlines/quiet periods remain the port's virtual-clock work; the
 * separate five-second guard only bounds a silent or dead asynchronous port.
 */
import { readdir } from "node:fs/promises";

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
 * Execute every authored step exactly once. Ports own resource cleanup, including
 * when their bounded operation fails; an assertion failure never earns a receipt.
 * `guardClock` is independent of virtual protocol time and injectable for tests.
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
