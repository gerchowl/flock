import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Drive the actual trace entry point with a socket whose callbacks we control. */
async function refusedAuth(unexpectedFrame: boolean): Promise<{ code: number; stderr: string }> {
  const root = await mkdtemp(join(tmpdir(), "flk-auth-"));
  const preload = join(root, "socket.ts");
  try {
    await writeFile(preload, `
      globalThis.WebSocket = class {
        closed = false;
        constructor() { queueMicrotask(() => this.onopen()); }
        send() {
          queueMicrotask(() => {
            this.closed = true;
            this.onclose({ code: 4401 });
            // The refusal promise has settled before this unexpected callback.
            if (${JSON.stringify(unexpectedFrame)}) this.onmessage({ data: "unexpected" });
          });
        }
        close() { this.closed = true; }
      };
    `);
    const child = Bun.spawn([
      process.execPath, "--preload", preload,
      `${import.meta.dir}/wire/endpoint-auth.ts`, "ws://example.test/v1/ws",
    ], {
      cwd: root, env: { HOME: root }, stdout: "ignore", stderr: "pipe", timeout: 5000,
    });
    try {
      const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
      return { code, stderr };
    } finally {
      if (child.exitCode === null) child.kill();
      await child.exited;
    }
  } finally { await rm(root, { recursive: true, force: true }); }
}

test("the auth trace rejects a post-refusal frame but accepts a clean refusal", async () => {
  const clean = await refusedAuth(false);
  expect(clean.stderr).toBe("");
  expect(clean.code).toBe(0);
  const unexpected = await refusedAuth(true);
  expect(unexpected.code).not.toBe(0);
  expect(unexpected.stderr).toContain("unasserted server frame on refused auth");
}, 10000);
