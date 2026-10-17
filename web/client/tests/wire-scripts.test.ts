/**
 * Replays S7a's structured-wire scripts (#1692) against the real
 * {@link WireConnection}, with a virtual clock and scripted sockets.
 *
 * **Selection.** Each script's header names the slices it covers in
 * `covers`: `s7-min` (#1746), full `s7`, or both. Every script covering
 * `s7-min` is replayed, and so is every `s7` script whose behaviour G3a
 * already implements; the rest are listed with the G3b (#1595) feature
 * they need. A script in no list, or an `s7-min` script left to G3b, fails
 * the suite, so nothing is skipped silently. Fixtures are S7a's, replayed
 * verbatim and never edited.
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

/** The scripts G3a (#1747) replays. */
const REPLAYED = [
  "agent-gap",
  "agent-send",
  "backoff-cap",
  "backoff-reset",
  "bootstrap-snapshot",
  "clean",
  "client-disconnect",
  "command-failure",
  "connect-deadline",
  "empty-bootstrap",
  "empty-snapshot",
  "fleet-lite",
  "foreground",
  "gap-snapshot",
  "handshake-deadline",
  "heartbeat-missed",
  "lagged",
  "network-change",
  "old-socket",
  "ping-pong",
  "protocol-refusal",
  "set-token-changed-principal",
  "set-token-same-principal",
  "stream-independence",
  "subfail-cursor_ahead",
  "subfail-scope",
  "subfail-unknown_stream",
  "subscription-deadline",
  "token-refused",
  "token-revoked",
  "unknown-subscription",
];

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

async function load(name: string): Promise<FixtureLine[]> {
  return parseScript(await Bun.file(`${SCRIPTS}/${name}.jsonl`).text());
}

/** The script's slice labels: at least one, each named once; anything else fails. */
function coverage(name: string, steps: FixtureLine[]): readonly FixtureCoverage[] {
  const given = steps[0];
  const covers = given?.step === "given" ? given.covers : undefined;
  if (covers === undefined || covers.length === 0 || new Set(covers).size !== covers.length) {
    throw new Error(`${name}: covers is ${JSON.stringify(covers)}, expected one or more distinct slice labels`);
  }
  return covers;
}

const JITTER = { low: () => 0, high: () => 0.999_999 } as const;

describe("S7a scripts", () => {
  test("every script is either replayed or skipped with a reason", async () => {
    const names = (await readdir(SCRIPTS)).filter((name) => name.endsWith(".jsonl")).map((name) => name.slice(0, -6));
    expect([...REPLAYED, ...Object.keys(G3B)].sort()).toEqual(names.sort());
  });

  test("every s7-min script is G3a's, and every script names its slices", async () => {
    const names = (await readdir(SCRIPTS)).filter((name) => name.endsWith(".jsonl")).map((name) => name.slice(0, -6));
    const min: string[] = [];
    for (const name of names) if (coverage(name, await load(name)).includes("s7-min")) min.push(name);
    expect(min.length).toBeGreaterThan(0);
    expect(min.filter((name) => Object.hasOwn(G3B, name))).toEqual([]);
    expect(min.filter((name) => !REPLAYED.includes(name))).toEqual([]);
  });

  for (const name of REPLAYED) {
    for (const [jitter, random] of Object.entries(JITTER)) {
      test(`${name} (${jitter} jitter)`, async () => {
        const steps = await load(name);
        new Harness(random).run(steps);
      });
    }
  }
});
