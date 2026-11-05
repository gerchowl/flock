/** Owned seed output is consumed completely, with bounded child cleanup. */
import { expect, test } from "bun:test";
import { assertSeedSucceeded } from "./wire/seed-process";

test("the operator seed captures complete stdout while draining stderr", async () => {
  const seed = Bun.spawn([process.execPath, "-e", `await Promise.all([
    Bun.write(Bun.stdout, "o".repeat(1024 * 1024)),
    Bun.write(Bun.stderr, "e".repeat(1024 * 1024))
  ]);`], { stdout: "pipe", stderr: "pipe" });
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const stdout = await Promise.race([
      assertSeedSucceeded(seed),
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("operator seed blocked on an undrained pipe")), 5_000);
      }),
    ]);
    expect(seed.exitCode).toBe(0);
    expect(stdout.length).toBe(1024 * 1024);
    expect(stdout).toBe("o".repeat(1024 * 1024));
  } finally {
    clearTimeout(timer);
    if (seed.exitCode === null) seed.kill();
    await seed.exited;
  }
}, 10_000);
