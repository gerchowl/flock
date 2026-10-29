/**
 * Replays S7a's structured-wire scripts (#1692) against the real
 * {@link WireConnection}, with a virtual clock and scripted sockets.
 *
 * **Selection.** Each script's header names the slices it covers in
 * `covers`: `s7-min` (#1746), full `s7`, or `client-only`. Every script covering
 * `s7-min` is replayed, and so is every `s7` script whose behaviour G3a
 * already implements; the rest are listed with the G3b (#1595) feature
 * they need. A script in no list, or an `s7-min` script left to G3b, fails
 * the suite, so nothing is skipped silently. Literal traces run verbatim.
 * A runtime timestamp marker is explicitly refused here: only the actual
 * endpoint port can supply its independent persistence and wall-window oracles.
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

/** The scripts G3a (#1747) and G3b (#1595) so far replay. */
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
  "permission-decision",
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
  "version-gap",
];

/**
 * Scripts that play a client newer than this one, with the protocol their
 * `hello` names; every other script runs the client's own `WIRE_PROTOCOL`.
 */
const NEWER_CLIENT: { readonly [script: string]: number } = {
  "version-gap": 2,
};

/** Left to G3 #1595 (G3b), with the feature each one needs. */
const G3B: { readonly [script: string]: string } = {
  "restored-resync": "persisted resync markers",
  "subfail-operator_only": "operator_only (not emitted by S7-min)",
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
        if (steps.some((line) => line.step === "send" && line.frame.t === "ev" && typeof line.frame.ts_ms !== "number")) {
          expect(() => new Harness(random).run(steps)).toThrow("dynamic timestamp requires persisted and wall-window oracles");
        } else new Harness(random, NEWER_CLIENT[name]).run(steps);
      });
    }
  }
});

describe("harness assertions", () => {
  /** A synthetic script: `c0` is seeded in the outbox, then an observe names `absent`. */
  const seeded = (absent: string): FixtureLine[] =>
    parseScript(
      [
        { v: 1, step: "given", cursors: {}, outbox: [{ cmd_id: "c0", verb: "agent.send", args: { target: "a", text: "hi", submit: true }, principal: "fixture-principal", created_ms: 0, status: "pending", first_sent_ms: null }], covers: ["s7"] },
        { v: 1, step: "observe", outbox: { c0: "pending" }, absent_outbox: [absent] },
      ]
        .map((line) => JSON.stringify(line))
        .join("\n"),
    );

  test("absent_outbox fails when the outbox holds the named command", () => {
    expect(() => new Harness().run(seeded("c0"))).toThrow("command c0 should be absent");
  });

  test("absent_outbox passes for a command the outbox does not hold", () => {
    expect(() => new Harness().run(seeded("c9"))).not.toThrow();
  });
});

test("server conformance selection excludes the client-only fleet fixture", async () => {
  const names = (await readdir(SCRIPTS)).filter((name) => name.endsWith(".jsonl")).map((name) => name.slice(0, -6));
  const server = [];
  for (const name of names) if (coverage(name, await load(name)).includes("s7-min")) server.push(name);
  expect(server).not.toContain("fleet-lite");
  expect(coverage("fleet-lite", await load("fleet-lite"))).toEqual(["client-only"]);
});

// ADR-0036 S6/#1429 and #2580: command scenarios use the ClientOpen grants.
for (const name of ["permission-decision", "agent-send", "token-revoked"]) {
  test(`${name} fixture declares its exact ClientOpen grants`, async () => {
    const welcome = (await load(name)).find(
      (line) => line.step === "send" && line.frame.t === "welcome",
    );
    expect(welcome?.step === "send" && welcome.frame.t === "welcome" && welcome.frame.capabilities)
      .toEqual(["read", "send", "answer", "operate"]);
  });
}

test("permission principal is a literal deterministic token identity", async () => {
  const events = (await load("permission-decision")).flatMap((line) =>
    line.step === "send" && line.frame.t === "ev" ? [line.frame.ev] : []);
  const close = events.find((event) => event !== null && typeof event === "object" && "state" in event && event.state === "closed");
  expect(close !== null && typeof close === "object" && "principal" in close && close.principal)
    .toBe("0123456789abcdef0123456789abcdef");
});

test("permission script assigns one valid principal per scripted token", async () => {
  const identities = new Map<string, Set<string>>();
  let token: string | undefined;
  let observations = 0;
  for (const line of await load("permission-decision")) {
    if (line.step === "expect" && line.frame.t === "hello") token = line.frame.token ?? undefined;
    if (line.step !== "send") continue;
    const frame = line.frame;
    const principal = frame.t === "welcome" ? frame.principal
      : frame.t === "ev" && frame.ev !== null && typeof frame.ev === "object" && "principal" in frame.ev
        ? frame.ev.principal : undefined;
    if (principal === undefined) continue;
    expect(token).toBeDefined();
    expect(typeof principal).toBe("string");
    expect(principal).toMatch(/^[0-9a-f]{32}$/);
    if (token === undefined || typeof principal !== "string") throw new Error("unbound principal");
    const seen = identities.get(token) ?? new Set<string>();
    seen.add(principal);
    identities.set(token, seen);
    observations++;
  }
  expect(observations).toBe(2);
  expect([...identities].map(([token, principals]) => [token, [...principals]]))
    .toEqual([["fixture-token", ["0123456789abcdef0123456789abcdef"]]]);
});
