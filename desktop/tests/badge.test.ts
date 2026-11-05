import { expect, test } from "bun:test";

// The shell exposes only the narrow badge API; the client never imports Tauri.
test("shell badge calls Tauri's window badge command and has only its specific permission", async () => {
  const source = await Bun.file(new URL("../src/set-badge-count.js", import.meta.url)).text();
  const calls: unknown[] = [];
  (globalThis as unknown as { window: unknown }).window = { __TAURI_INTERNALS__: { invoke: async (...args: unknown[]) => { calls.push(args); } } };
  const badge = (0, eval)(source) as (count: number) => Promise<void>;
  await badge(3); await badge(0);
  expect(calls).toEqual([
    ["set_badge_count", { count: 3 }],
    ["set_badge_count", { count: 0 }],
  ]);
  const capability = await Bun.file(new URL("../capabilities/main.json", import.meta.url)).json();
  expect(capability.permissions).toContain("allow-set-badge-count");
  expect(capability.permissions).not.toContain("core:window:default");
  const config = await Bun.file(new URL("../src/config.rs", import.meta.url)).text();
  expect(config).toContain('include_str!("set-badge-count.js")');
  expect(config).toContain("setBadgeCount: {}");
});

// The command uses the pinned crate's public typed API instead of encoding its private IPC.
test("native badge uses Tauri's typed setter rather than the internal command payload", async () => {
  const source = (await Bun.file(new URL("../src/main.rs", import.meta.url)).text()).replace(/\s+/g, "");
  expect(source).toContain("window.set_badge_count(Some(count.into()))");
  expect(source).toContain("fnset_badge_count(window:tauri::Window,count:u32)");
  const adapter = await Bun.file(new URL("../src/set-badge-count.js", import.meta.url)).text();
  expect(adapter).not.toContain("plugin:window|");
});
