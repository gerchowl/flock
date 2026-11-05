import { expect, test } from "bun:test";

// The shell exposes only the narrow badge API; the client never imports Tauri.
test("shell badge calls Tauri's window badge command and has only its specific permission", async () => {
  const source = await Bun.file(new URL("../src/set-badge-count.js", import.meta.url)).text();
  const calls: unknown[] = [];
  (globalThis as unknown as { window: unknown }).window = { __TAURI_INTERNALS__: { invoke: async (...args: unknown[]) => { calls.push(args); } } };
  const badge = (0, eval)(source) as (count: number) => Promise<void>;
  await badge(3); await badge(0);
  expect(calls).toEqual([
    ["plugin:window|set_badge_count", { label: "main", count: 3 }],
    ["plugin:window|set_badge_count", { label: "main", count: 0 }],
  ]);
  const capability = await Bun.file(new URL("../capabilities/main.json", import.meta.url)).json();
  expect(capability.permissions).toContain("core:window:allow-set-badge-count");
  expect(capability.permissions).not.toContain("core:window:default");
  const config = await Bun.file(new URL("../src/config.rs", import.meta.url)).text();
  expect(config).toContain('include_str!("set-badge-count.js")');
  expect(config).toContain("setBadgeCount: {}");
});
