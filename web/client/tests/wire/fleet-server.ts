/** Actual S7 server, generated fleet contracts, and the real snapshot reducer. */
import { strict as assert } from "node:assert";
import { createInterface } from "node:readline";
import type { AgentRow } from "../../src/fleet";
import { createWireFleet } from "../../src/wire/fleet";
import { WireConnection } from "../../src/wire/connection";
import { browserClock, browserSockets, memoryStore } from "../../src/wire/env";

const [url] = process.argv.slice(2);
assert(url);
const connection = new WireConnection({ url, token: "a".repeat(64), client: { name: "fixture", version: "1" }, clock: browserClock, sockets: browserSockets, random: () => 0.5, store: memoryStore() });
let rows: readonly AgentRow[] = [];
let completedStage = -1;
let stage = 0;
let retained = false;
let barrier!: () => void;
let fail!: (error: Error) => void;
const failure = new Promise<never>((_, reject) => { fail = reject; });
const ceiling = setTimeout(() => fail(new Error("fleet server/client producer stayed silent")), 5_000);
const input = createInterface({ input: process.stdin });
const lines = input[Symbol.asyncIterator]();
const synced = (): Promise<void> => new Promise((resolve) => { barrier = resolve; });
const observer = {
  rows(next: readonly AgentRow[]) {
    assert.deepEqual(next.map((row) => row.id), stage === 0 ? ["fixture-agent"] : stage === 1 ? ["second-fixture-agent"] : []);
    for (const row of next) { assert.equal(row.state, "offline"); assert.equal(row.activity, ""); assert.deepEqual(row.waitingOnHuman, []); }
    rows = next;
  },
  failed(message: string) { fail(new Error(message)); },
};
const fleet = createWireFleet(connection);
connection.onChange(() => {
  if (connection.state === "Reconnecting" || connection.state === "NeedsToken") fail(new Error(connection.error ?? connection.state));
  if (stage === 1 && [...connection.streams()].some(([, value]) => value.state === "Resyncing")) {
    assert.deepEqual(rows.map((row) => row.id), ["fixture-agent"]);
    retained = true;
  }
  if (completedStage < stage && [...connection.streams()].some(([, value]) => value.state === "Current")) { completedStage = stage; barrier(); }
});
async function run(): Promise<void> {
  let ready = synced();
  let close = fleet.subscribe(observer);
  connection.connect();
  await ready;
  for (const command of ["replace", "empty"]) {
    close();
    stage++;
    console.log(command);
    const next = await lines.next();
    assert.equal(next.done, false);
    assert.equal(next.value, "changed");
    ready = synced();
    close = fleet.subscribe(observer);
    await ready;
  }
  assert(retained, "no old-row observation during the replacement");
  assert.equal(rows.length, 0);
  console.log("complete");
}
try { await Promise.race([run(), failure]); }
finally { clearTimeout(ceiling); connection.disconnect(); input.close(); process.stdin.pause(); }
