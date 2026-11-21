/** Real GUI, CLI and independent wire observations against the Rust driver's isolated fleet. */
import { expect, test } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { chromium } from "playwright";
import { createConnection, createServer, type Socket } from "node:net";
import { mkdir } from "node:fs/promises";
import type { KnownSessionEvent } from "../src/gen/KnownSessionEvent";
import type { StructuredAgentInfo } from "../src/gen/StructuredAgentInfo";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { WIRE_PROTOCOL } from "../src/wire/connection";

// The unit-test preload installs a simulated DOM; this driver uses real sockets.
await GlobalRegistrator.unregister();

type Node = { id: string; name: string; socket: string; working_directory: string; env: Record<string, string> };
type Manifest = { version: 1; binary: string; home: string; nodes: Node[]; artifacts: string };
const STEP_MS = 15_000;
type HistoryEvent = Extract<ServerFrame, { t: "ev" }> & { ev: KnownSessionEvent };

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

/** Mint for this long-lived driver process, as an operator app shell does. */
async function appToken(home: Node): Promise<string> {
  const socket = createConnection({ path: home.socket });
  try {
    return await step("mint operator app session", () => new Promise<string>((resolve, reject) => {
      let input = "";
      socket.on("error", reject);
      socket.on("end", () => reject(new Error("app mint ended without a response")));
      socket.on("connect", () => socket.write(`${JSON.stringify({ id: "gui-app", method: "client.app.mint", params: {} })}\n`));
      socket.on("data", chunk => {
        input += chunk.toString();
        if (!input.includes("\n")) return;
        try {
          const reply = JSON.parse(input.split("\n")[0]!);
          const token: unknown = reply.result?.issued?.token;
          if (typeof token !== "string") throw new Error("app mint refused");
          resolve(token);
        } catch (error) { reject(error); }
      });
    }));
  } finally { socket.destroy(); }
}

/** Raw transport relay: the minting process also owns the native Unix connection. */
async function nativeRelay(home: Node, homeUrl: string): Promise<{ url: string; close: () => Promise<void> }> {
  const sockets = new Set<Socket>();
  const endpoint = new URL(homeUrl);
  const track = (socket: Socket): void => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  };
  const server = createServer(incoming => {
    track(incoming);
    let pending = Buffer.alloc(0);
    const header = (chunk: Buffer): void => {
      pending = Buffer.concat([pending, chunk]);
      const end = pending.indexOf("\r\n\r\n");
      if (end < 0) { if (pending.length > 8192) incoming.destroy(); return; }
      incoming.off("data", header);
      incoming.pause();
      let request = pending.subarray(0, end).toString();
      const native = request.startsWith("GET /v1/ws ");
      const upstream = native
        ? createConnection({ path: home.socket.replace(/\.sock$/, "-ws.sock") })
        : createConnection({ host: endpoint.hostname, port: Number(endpoint.port) });
      track(upstream);
      incoming.on("error", () => upstream.destroy());
      upstream.on("error", () => incoming.destroy());
      if (!native) {
        request = request.replace(/^host:.*$/im, `Host: ${endpoint.host}`)
          .replace(/^origin:.*$/im, `Origin: ${endpoint.origin}`)
          .replace(/^connection:.*$/im, "Connection: close");
      }
      upstream.write(`${request}\r\n\r\n`);
      upstream.write(pending.subarray(end + 4));
      incoming.pipe(upstream).pipe(incoming);
      incoming.resume();
    };
    incoming.on("data", header);
    incoming.on("error", () => incoming.destroy());
  });
  await step("native relay listen", () => new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  }));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("native relay has no port");
  return {
    url: `http://127.0.0.1:${address.port}`,
    close: () => step("native relay close", () => new Promise<void>((resolve, reject) => {
      for (const socket of sockets) socket.destroy();
      server.close(error => error ? reject(error) : resolve());
    })),
  };
}

/** Independent authenticated replay; never reads the browser's session or store. */
async function history(manifest: Manifest, home: Node, url: string, agent: string): Promise<HistoryEvent[]> {
  const issued = await cli(manifest, home, ["client", "token", "add", `gui-history-${crypto.randomUUID()}`, "--scopes", "read,operate", "--show-token"]) as { result: { issued: { token: string } } };
  const endpoint = new URL("/v1/ws", url);
  endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";
  // DOM declarations hide Bun's supported header overload in this mixed project.
  const BunSocket = WebSocket as typeof WebSocket & { new (url: URL, options: Bun.WebSocketOptions): WebSocket };
  const socket = new BunSocket(endpoint, { headers: { Origin: new URL(url).origin } });
  const frames: ServerFrame[] = [];
  let failure: Error | undefined;
  let wake: (() => void) | undefined;
  const fail = (message: string): void => { failure = new Error(message); wake?.(); };
  socket.addEventListener("error", () => fail("history socket error"));
  socket.addEventListener("close", () => fail("history socket closed"));
  socket.addEventListener("message", event => {
    try {
      const frame = JSON.parse(String(event.data)) as ServerFrame;
      if (frame.t === "ping") socket.send(JSON.stringify({ t: "pong", nonce: frame.nonce }));
      else { frames.push(frame); wake?.(); }
    } catch { fail("invalid history frame"); }
  });
  socket.addEventListener("open", () => socket.send(JSON.stringify({ t: "hello", protocol: WIRE_PROTOCOL, client: { name: "fleet-oracle", version: "1" }, token: issued.result.issued.token })));
  const next = (): Promise<ServerFrame> => step("independent history frame", async () => {
    while (frames.length === 0) {
      if (failure) throw failure;
      await new Promise<void>(resolve => { wake = resolve; });
    }
    return frames.shift()!;
  });
  try {
    expect((await next()).t).toBe("welcome");
    const stream = `agent/${agent}`;
    socket.send(JSON.stringify({ t: "sub", sub_id: "history", stream }));
    const events: HistoryEvent[] = [];
    let cursor = 0;
    for (;;) {
      const frame = await next();
      expect("sub_id" in frame && frame.sub_id).toBe("history");
      if (frame.t === "synced") {
        expect(frame.seq).toBe(cursor);
        return events;
      }
      expect(frame.t).toBe("ev");
      if (frame.t !== "ev") throw new Error(`unexpected history frame: ${frame.t}`);
      expect(frame.stream).toBe(stream);
      expect(frame.seq).toBe(cursor + 1);
      cursor = frame.seq!;
      events.push({ ...frame, ev: frame.ev as KnownSessionEvent });
    }
  } finally {
    socket.close();
  }
}

let remoteStarted: boolean | undefined;

test("the real GUI converses, answers permission, resumes and stops a home agent", async () => {
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
  let relay: Awaited<ReturnType<typeof nativeRelay>> | undefined;
  try {
    const home = manifest.nodes.find((node) => node.name === manifest.home);
    if (!home) throw new Error("manifest has no home node");
    // Discover the served endpoint; the app credential below supplies authority.
    const url = await cli(manifest, home, ["client", "open", "--print"]) as string;
    await step("record home URL", () => Bun.write(path, JSON.stringify({ ...manifest, home_url: url })));
    const token = await appToken(home);
    relay = await nativeRelay(home, url);
    await context.addInitScript(({ serverUrl, token }) => {
      Object.assign(window, { __FLOCK_SHELL_CONFIG__: { transport: "wire", serverUrl, token } });
    }, { serverUrl: relay.url, token });
    await step("navigate to real bundle", () => page.goto(relay!.url));
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
    const agents = await cli(manifest, home, ["agent", "list"]) as { result: { structured_agents: { agent_id: string; name: string; runtime: string; cwd: string }[] } };
    const started = agents.result.structured_agents.find((agent) => agent.name === "gui-home-agent");
    expect(started?.runtime).toBe("structured");
    expect(started?.cwd).toBe(home.working_directory);
    await step("close picker", () => picker.getByRole("button", { name: "Done", exact: true }).click());
    await step("select home agent", () => page.getByTestId("agent-row").filter({ hasText: "gui-home-agent" }).click());
    const transcript = page.getByRole("region", { name: "Transcript", exact: true });
    const composer = page.getByTestId("composer");
    await step("send conversation", () => composer.getByRole("textbox").fill("hello fixture"));
    await step("submit conversation", () => composer.getByRole("button", { name: "Send", exact: true }).click());
    await step("durable user message", () => transcript.locator(".event--user").getByText("hello fixture", { exact: false }).waitFor({ timeout: STEP_MS }));
    await step("final reply", () => transcript.locator(".event--assistant").getByText("hello from fixture", { exact: true }).waitFor({ timeout: STEP_MS }));
    await step("send permission turn", () => composer.getByRole("textbox").fill("request permission"));
    await step("submit permission turn", () => composer.getByRole("button", { name: "Send", exact: true }).click());
    await step("durable permission request", () => transcript.getByText("Permission request · Bash", { exact: true }).waitFor({ timeout: STEP_MS }));
    await step("allow through GUI", () => page.getByTestId("inbox").getByRole("button", { name: "Allow once", exact: true }).click());
    await step("decision follow-up", () => transcript.locator(".event--assistant").getByText("permission accepted", { exact: true }).waitFor({ timeout: STEP_MS }));
    await step("permission closed", () => transcript.getByText("Permission request closed", { exact: true }).waitFor({ timeout: STEP_MS }));
    const recorded = await history(manifest, home, url, started!.agent_id);
    const events = recorded.map(frame => frame.ev);
    const user = events.filter(event => event.type === "user");
    expect(user).toHaveLength(2);
    expect(user[0]).toMatchObject({ message: { role: "user", content: [{ type: "text", text: expect.stringContaining("hello fixture") }] } });
    expect(user[1]).toMatchObject({ message: { role: "user", content: [{ type: "text", text: expect.stringContaining("request permission") }] } });
    const replies = events.filter(event => event.type === "assistant");
    expect(replies).toHaveLength(2);
    expect(replies[0]).toMatchObject({ message: { role: "assistant", content: [{ type: "text", text: "hello from fixture" }] } });
    expect(replies[1]).toMatchObject({ message: { role: "assistant", content: [{ type: "text", text: "permission accepted" }] } });
    const permissions = events.filter(event => event.type === "flk_permission_request");
    expect(permissions).toHaveLength(2);
    const [opened, closed] = permissions;
    expect(opened).toMatchObject({ request_id: "gui-permission", state: "open", request: { tool_name: "Bash", input: { command: "true" } } });
    expect(closed).toMatchObject({ request_id: "gui-permission", request_seq: opened!.request_seq, state: "closed", decision: "allow_once", source: "token" });
    const decisionSeq = recorded.find(frame => frame.ev === closed)!.seq!;
    expect(decisionSeq).toBeGreaterThan(recorded.find(frame => frame.ev === opened)!.seq!);
    expect(recorded.find(frame => frame.ev === replies[1])!.seq).toBeGreaterThan(decisionSeq);
    const observeAgent = async (): Promise<StructuredAgentInfo> => {
      const list = await cli(manifest, home, ["agent", "list"]) as { result: { structured_agents: StructuredAgentInfo[] } };
      const agent = list.result.structured_agents.find(agent => agent.agent_id === started!.agent_id);
      if (!agent) throw new Error("oracle lost the structured agent");
      return agent;
    };
    const original = await observeAgent();
    expect(original.lifecycle).toBe("live");
    const focus = page.getByRole("main", { name: "Focused agent", exact: true });
    await step("stop before resume", () => focus.getByRole("button", { name: "Stop agent", exact: true }).click());
    await step("confirm pre-resume stop", () => focus.getByRole("button", { name: "Confirm stop", exact: true }).click());
    await step("offline before resume", () => focus.getByText("offline", { exact: true }).waitFor({ timeout: STEP_MS }));
    expect((await observeAgent()).lifecycle).toBe("offline");
    await step("restart through GUI", () => focus.getByRole("button", { name: "Restart agent", exact: true }).click());
    await step("restart accepted", () => focus.getByRole("status").getByText("Agent restarted.", { exact: true }).waitFor({ timeout: STEP_MS }));
    await step("resumed agent is online", () => focus.getByText("offline", { exact: true }).waitFor({ state: "hidden", timeout: STEP_MS }));
    const resumed = await observeAgent();
    expect(resumed.session_id).toBe(original.session_id);
    expect(resumed.lifecycle).toBe("live");
    await step("post-restart prompt", () => composer.getByRole("textbox").fill("after restart"));
    await step("submit post-restart prompt", () => composer.getByRole("button", { name: "Send", exact: true }).click());
    await step("post-restart reply", () => transcript.locator(".event--assistant").getByText("resumed fixture reply", { exact: true }).waitFor({ timeout: STEP_MS }));
    const resumedHistory = await history(manifest, home, url, started!.agent_id);
    const resumedReplies = resumedHistory.filter(frame => frame.ev.type === "assistant");
    expect(resumedReplies).toHaveLength(3);
    expect(resumedReplies[2]?.ev).toMatchObject({ type: "assistant", session_id: original.session_id, message: { role: "assistant", content: [{ type: "text", text: "resumed fixture reply" }] } });
    await step("open lifecycle palette", () => page.getByTestId("palette-trigger").click());
    const palette = page.getByRole("dialog", { name: "Command palette", exact: true });
    await step("find stop command", () => palette.getByRole("combobox").fill("Stop agent"));
    await step("open stop confirmation from palette", () => palette.getByRole("option").filter({ hasText: "Stop agent" }).click());
    const confirmation = focus.getByRole("group", { name: "Confirm stop agent", exact: true });
    await step("stop confirmation visible", () => confirmation.waitFor({ timeout: STEP_MS }));
    expect((await observeAgent()).lifecycle).toBe("live");
    await step("confirm stop through GUI", () => confirmation.getByRole("button", { name: "Confirm stop", exact: true }).click());
    await step("GUI reports stopped", () => focus.getByText("offline", { exact: true }).waitFor({ timeout: STEP_MS }));
    expect((await observeAgent()).lifecycle).toBe("offline");
    await step("open remote picker", () => page.getByRole("button", { name: "New agent", exact: true }).click());
    await step("name remote agent", () => picker.getByLabel("Name", { exact: true }).fill("gui-remote-agent"));
    await step("choose remote node", () => picker.getByLabel("Node", { exact: true }).selectOption(remote.id));
    expect(await step("remote directory reset", () => picker.getByLabel("Working directory", { exact: true }).inputValue())).toBe("");
    await step("remote working directory", () => picker.getByLabel("Working directory", { exact: true }).fill(remote.working_directory));
    await step("request remote start", () => picker.getByRole("button", { name: "Start agent", exact: true }).click());
    await step("remote outcome", () => picker.getByRole("status").filter({ hasText: /Agent started\.|Agents cannot be started on another node yet/ }).waitFor({ timeout: STEP_MS }));
    remoteStarted = await picker.getByText("Agent started.", { exact: true }).isVisible();
    if (remoteStarted) {
      const list = await cli(manifest, remote, ["agent", "list"]) as { result: { structured_agents: { name: string }[] } };
      expect(list.result.structured_agents.some(agent => agent.name === "gui-remote-agent")).toBe(true);
    } else {
      await step("remote refusal details", () => picker.getByText("Details", { exact: true }).click());
      await step("typed remote refusal", () => picker.getByText("unsupported_for_agent", { exact: true }).waitFor({ timeout: STEP_MS }));
      expect(await localAgents(remote)).toEqual(before);
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
      try { await step("close browser", () => browser.close()); }
      finally { await relay?.close(); }
    }
  }
}, 90_000);

// The normal scenario validates setup and the typed refusal before this oracle.
// ADR-0043 remote dispatch must make this unexpected pass fail the suite.
test.failing("ADR-0043 starts the agent on node B through the picker", () => {
  expect(remoteStarted).toBe(true);
});
