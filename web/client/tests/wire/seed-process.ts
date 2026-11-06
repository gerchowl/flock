/** Completion of the owned operator seed, with its diagnostic failure preserved. */
import { strict as assert } from "node:assert";

/** Drain both pipes concurrently; the caller bounds lifetime. A failing exit reports stderr. */
export async function assertSeedSucceeded(seed: {
  readonly exited: Promise<number>;
  readonly stdout: ReadableStream<Uint8Array>;
  readonly stderr: ReadableStream<Uint8Array>;
}): Promise<string> {
  const [exit, stdout, stderr] = await Promise.all([
    seed.exited, new Response(seed.stdout).text(), new Response(seed.stderr).text(),
  ]);
  assert.equal(exit, 0, stderr);
  return stdout;
}
