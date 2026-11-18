/** Real GUI and CLI observations against the Rust driver's isolated fleet. */
import { expect, test } from "bun:test";
import { chromium } from "playwright";
import { mkdir } from "node:fs/promises";

type Node = { id: string; name: string; socket: string; env: Record<string, string> };
type Manifest = { version: 1; binary: string; home: string; nodes: Node[]; artifacts: string };
const STEP_MS = 15_000;

async function step<T>(label: string, action: () => Promise<T>): Promise<T> {
  const signal = AbortSignal.timeout(STEP_MS);
  return Promise.race([
    action(),
    new Promise<never>((_, reject) => signal.addEventListener("abort", () => reject(new Error(`${label}: deadline exceeded`)), { once: true })),
  ]);
}

async function cli(manifest: Manifest, node: Node, args: string[]): Promise<unknown> {
  const child = Bun.spawn([manifest.binary, "--json", ...args], {
    env: { ...node.env, FLOCK_SOCKET_PATH: node.socket }, stdout: "pipe", stderr: "pipe",
  });
  try {
    return await step(`CLI ${node.name} ${args.join(" ")}`, async () => {
      const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
      expect(code, stderr).toBe(0);
      return args.includes("--print") ? stdout.trim() : JSON.parse(stdout);
    });
  } finally {
    if (child.exitCode === null) child.kill("SIGKILL");
    await step("CLI reap", () => child.exited);
  }
}

test("both meshed node rows are online in the real GUI", async () => {
  const path = process.env["FLOCK_GUI_FLEET_MANIFEST"];
  if (!path) throw new Error("Rust fleet driver must provide FLOCK_GUI_FLEET_MANIFEST");
  const manifest = await step("read fleet manifest", () => Bun.file(path).json()) as Manifest;
  expect(manifest.version).toBe(1);
  expect(manifest.nodes).toHaveLength(2);
  await step("create artifact directory", () => mkdir(manifest.artifacts, { recursive: true }));
  const browser = await step("Chromium launch", () => chromium.launch());
  const context = await step("browser context", () => browser.newContext({ viewport: { width: 1440, height: 1000 } }));
  await step("start trace", () => context.tracing.start({ screenshots: true, snapshots: true, sources: true }));
  const page = await step("browser page", () => context.newPage());
  let failed = true;
  try {
    const home = manifest.nodes.find((node) => node.name === manifest.home);
    if (!home) throw new Error("manifest has no home node");
    // Mint after Chromium is ready: operator codes expire in sixty seconds.
    const url = await cli(manifest, home, ["client", "open", "--print"]) as string;
    await step("record home URL", () => Bun.write(path, JSON.stringify({ ...manifest, home_url: url })));
    await step("navigate to real bundle", () => page.goto(url));
    await step("select fleet scope", () => page.getByRole("button", { name: "All nodes", exact: true }).click());
    for (const node of manifest.nodes) {
      const group = page.getByTestId("group-node").filter({ has: page.locator(".group__name", { hasText: node.id }) });
      await step(`${node.name} online row`, () => group.locator('[data-testid="node-status"][data-online="true"]').waitFor({ state: "visible", timeout: STEP_MS }));
      const workspaces = await cli(manifest, node, ["workspace", "list"]) as { result: { workspaces: unknown[] } };
      expect(workspaces.result.workspaces.length).toBeGreaterThan(0);
    }
    const peers = await cli(manifest, home, ["peers", "status"]) as { node_id: string; routes: unknown[] }[];
    for (const node of manifest.nodes.filter((node) => node.name !== manifest.home)) {
      expect(peers.find((peer) => peer.node_id === node.id)?.routes.length).toBeGreaterThan(0);
    }
    failed = false;
  } finally {
    try {
      if (failed) {
        await step("failure screenshot", () => page.screenshot({ path: `${manifest.artifacts}/fleet.png`, fullPage: true })).catch(() => {});
        await step("failure trace", () => context.tracing.stop({ path: `${manifest.artifacts}/trace.zip` }));
      } else {
        await step("discard trace", () => context.tracing.stop());
      }
    } finally {
      await step("close browser", () => browser.close());
    }
  }
}, 90_000);
