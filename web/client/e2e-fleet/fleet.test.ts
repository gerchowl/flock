/** Real GUI and CLI observations against the Rust driver's isolated fleet. */
import { expect, test } from "bun:test";
import { chromium } from "playwright";
import { mkdir } from "node:fs/promises";

type Node = { id: string; name: string; socket: string; working_directory: string; env: Record<string, string> };
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

test("the real GUI starts a home agent and refuses a remote start", async () => {
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
    const remote = manifest.nodes.find((node) => node.name !== manifest.home)!;
    const localAgents = async (node: Node): Promise<unknown> => {
      const list = await cli(manifest, node, ["agent", "list"]) as { result: { agents: unknown[]; structured_agents: unknown[] } };
      return { agents: list.result.agents, structured_agents: list.result.structured_agents };
    };
    const before = await localAgents(remote);
    await step("open new agent picker", () => page.getByRole("button", { name: "New agent", exact: true }).click());
    const picker = page.getByRole("dialog", { name: "New agent", exact: true });
    expect(await step("home node default", () => picker.getByLabel("Node", { exact: true }).inputValue())).toBe(home.id);
    await step("name home agent", () => picker.getByLabel("Name", { exact: true }).fill("gui-home-agent"));
    await step("invalid working directory", () => picker.getByLabel("Working directory", { exact: true }).fill(`${home.working_directory}/missing`));
    await step("request invalid directory", () => picker.getByRole("button", { name: "Start agent", exact: true }).click());
    await step("directory refusal", () => picker.getByText("The agent could not be started with these settings. Check the name and working directory, then try again.", { exact: true }).waitFor({ timeout: STEP_MS }));
    await step("home working directory", () => picker.getByLabel("Working directory", { exact: true }).fill(home.working_directory));
    await step("start home agent", () => picker.getByRole("button", { name: "Start agent", exact: true }).click());
    await step("home start accepted", () => picker.getByText("Agent started.", { exact: true }).waitFor({ timeout: STEP_MS }));
    const agents = await cli(manifest, home, ["agent", "list"]) as { result: { structured_agents: { name: string; runtime: string; cwd: string }[] } };
    const started = agents.result.structured_agents.find((agent) => agent.name === "gui-home-agent");
    expect(started?.runtime).toBe("structured");
    expect(started?.cwd).toBe(home.working_directory);
    await step("close picker", () => picker.getByRole("button", { name: "Done", exact: true }).click());
    await step("open remote picker", () => page.getByRole("button", { name: "New agent", exact: true }).click());
    await step("name remote agent", () => picker.getByLabel("Name", { exact: true }).fill("gui-remote-agent"));
    await step("choose remote node", () => picker.getByLabel("Node", { exact: true }).selectOption(remote.id));
    expect(await step("remote directory reset", () => picker.getByLabel("Working directory", { exact: true }).inputValue())).toBe("");
    await step("remote working directory", () => picker.getByLabel("Working directory", { exact: true }).fill(remote.working_directory));
    await step("request remote start", () => picker.getByRole("button", { name: "Start agent", exact: true }).click());
    await step("remote refusal", () => picker.getByText("Agents cannot be started on another node yet. Choose the home node.", { exact: true }).waitFor({ timeout: STEP_MS }));
    expect(await localAgents(remote)).toEqual(before);
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
