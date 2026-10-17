/**
 * Replays S7a's structured-wire scripts (#1692) against the real
 * {@link WireConnection}, with a virtual clock and scripted sockets.
 *
 * **Selection.** Each script's header names its slice in `covers`:
 * `s7-min` (#1746) or full `s7`. Every `s7-min` script is replayed, and so
 * is every `s7` script whose behaviour G3a already implements; the rest are
 * listed with the G3b (#1595) feature they need. Scripts that need a later slice of #1747 are listed with it. A script in no
 * list, or an `s7-min` script left to G3b, fails the suite, so nothing is
 * skipped silently. Fixtures are S7a's and never edited; where one asserts
 * more than S7-min may, the extra is dropped here, with the reason.
 *
 * Each script runs twice, with the jitter source at its lowest and highest
 * value, so both ends of every backoff interval are exercised.
 */
import { describe, expect, test } from "bun:test";
import { readdir } from "node:fs/promises";

import type { FixtureCoverage } from "../src/gen/FixtureCoverage";
import type { FixtureLine } from "../src/gen/FixtureLine";
import { Harness, parseScript } from "./wire/harness";

const SCRIPTS = `${import.meta.dir}/../test/fixtures/wire`;

/** The scripts G3a (#1747) replays, so far. */
const REPLAYED = [
  "backoff-cap",
  "backoff-reset",
  "connect-deadline",
  "foreground",
  "handshake-deadline",
  "network-change",
  "ping-pong",
  "protocol-refusal",
  "token-refused",
];

/** Scripts that need a later slice of #1747. */
const LATER: { readonly [script: string]: string } = {
  "agent-gap": "streams: #1747's second slice",
  "agent-send": "the outbox: #1747's third slice",
  "bootstrap-snapshot": "streams: #1747's second slice",
  "clean": "streams: #1747's second slice",
  "client-disconnect": "the outbox: #1747's third slice",
  "command-failure": "the outbox: #1747's third slice",
  "empty-bootstrap": "streams: #1747's second slice",
  "empty-snapshot": "streams: #1747's second slice",
  "gap-snapshot": "streams: #1747's second slice",
  "heartbeat-missed": "streams: #1747's second slice",
  "lagged": "streams: #1747's second slice",
  "old-socket": "streams: #1747's second slice",
  "set-token-changed-principal": "the outbox: #1747's third slice",
  "set-token-same-principal": "the outbox: #1747's third slice",
  "stream-independence": "streams: #1747's second slice",
  "subfail-cursor_ahead": "streams: #1747's second slice",
  "subfail-scope": "streams: #1747's second slice",
  "subfail-unknown_stream": "streams: #1747's second slice",
  "subscription-deadline": "streams: #1747's second slice",
  "token-revoked": "the outbox: #1747's third slice",
};

/** Left to G3 #1595 (G3b), with the feature each one needs. */
const G3B: { readonly [script: string]: string } = {
  "restored-resync": "persisted resync markers",
  "subfail-operator_only": "operator_only (not emitted by S7-min)",
  "version-gap": "ReadOnly",
  "uncertain-command": "resume-only resends and unknown_outcome",
  "restored-outbox-status": "resume-only resends and unknown_outcome",
  "outbox-window-expiry-connecting": "window expiry",
  "outbox-window-expiry-disconnected": "window expiry",
  "outbox-window-expiry-hello": "window expiry",
  "outbox-window-expiry-live": "window expiry",
  "outbox-window-expiry-needstoken": "window expiry",
  "outbox-window-expiry-readonly": "window expiry",
  "outbox-window-expiry-reconnecting": "window expiry",
  "outbox-window-expiry-syncing": "window expiry",
};

/**
 * Scripts replayed without their seeded outbox. `token-refused.jsonl` seeds
 * a pending `queue.add`, a verb S7-min scripts must not request (#1727), so
 * only its 4401/4403 path is asserted: the seed and its expected status are
 * both dropped.
 */
const WITHOUT_SEEDED_OUTBOX = new Set(["token-refused"]);

function withoutSeededOutbox(steps: FixtureLine[]): FixtureLine[] {
  return steps.map((step) => {
    if (step.step === "given") return { ...step, outbox: [] };
    if (step.step === "observe") return { ...step, outbox: {} };
    return step;
  });
}

async function load(name: string): Promise<FixtureLine[]> {
  return parseScript(await Bun.file(`${SCRIPTS}/${name}.jsonl`).text());
}

/** The script's one slice label; anything else fails. */
function coverage(name: string, steps: FixtureLine[]): FixtureCoverage {
  const given = steps[0];
  const covers = given?.step === "given" ? given.covers : undefined;
  if (covers?.length !== 1 || covers[0] === undefined) throw new Error(`${name}: covers is ${JSON.stringify(covers)}, expected one slice label`);
  return covers[0];
}

const JITTER = { low: () => 0, high: () => 0.999_999 } as const;

describe("S7a scripts", () => {
  test("every script is either replayed or skipped with a reason", async () => {
    const names = (await readdir(SCRIPTS)).filter((name) => name.endsWith(".jsonl")).map((name) => name.slice(0, -6));
    expect([...REPLAYED, ...Object.keys(LATER), ...Object.keys(G3B)].sort()).toEqual(names.sort());
  });

  test("every s7-min script is G3a's, and every script names one slice", async () => {
    const names = (await readdir(SCRIPTS)).filter((name) => name.endsWith(".jsonl")).map((name) => name.slice(0, -6));
    const min: string[] = [];
    for (const name of names) if (coverage(name, await load(name)) === "s7-min") min.push(name);
    expect(min.filter((name) => Object.hasOwn(G3B, name))).toEqual([]);
    expect(min.filter((name) => ![...REPLAYED, ...Object.keys(LATER)].includes(name))).toEqual([]);
  });

  for (const name of REPLAYED) {
    for (const [jitter, random] of Object.entries(JITTER)) {
      test(`${name} (${jitter} jitter)`, async () => {
        const steps = await load(name);
        new Harness(random).run(WITHOUT_SEEDED_OUTBOX.has(name) ? withoutSeededOutbox(steps) : steps);
      });
    }
  }
});
