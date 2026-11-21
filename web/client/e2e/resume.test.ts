/** Browser resume events against open, silently dead WebSockets, under Playwright's clock. */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { chromium, type Browser, type Page } from "playwright";
import { CLIENT_ROOT } from "../tests/build";
import { WELCOME } from "../tests/wire/harness";

// Independent acceptance ceilings: changing production deadlines cannot move these assertions.
const RESUME_PROBE_MS = 3_000;
const RESUME_FALLBACK_MS = 17_000;
const OUT = `${CLIENT_ROOT}/artifacts/resume-harness`;
const ORIGIN = "http://resume.test";
let browser: Browser;
beforeAll(async () => {
  const build = Bun.spawn([process.execPath, "x", "vite", "build", "--config", "e2e/resume/vite.config.ts", "--outDir", OUT, "--emptyOutDir", "--logLevel", "error"], { cwd: CLIENT_ROOT, stdout: "pipe", stderr: "pipe", env: { ...process.env, NODE_ENV: "production" } });
  const [code, stderr] = await Promise.all([build.exited, new Response(build.stderr).text()]);
  if (code !== 0) throw new Error(`resume fixture build exited ${code}: ${stderr}`);
  browser = await chromium.launch();
}, 120_000);
afterAll(async () => { await browser?.close(); });

async function open(healthy = false, capable = true) {
  const page = await browser.newPage();
  page.setDefaultTimeout(5_000);
  await page.clock.install({ time: new Date("2026-01-01T00:00:00Z") });
  await page.clock.pauseAt(new Date("2026-01-01T00:00:01Z"));
  await page.route("**/*", (route) => new URL(route.request().url()).origin === ORIGIN ? route.fulfill({ path: `${OUT}${new URL(route.request().url()).pathname}` }) : route.abort());
  const pings: string[] = [];
  let connections = 0;
  let closes = 0;
  await page.routeWebSocket("wss://example.invalid/v1/ws", (socket) => {
    connections++;
    socket.onClose(() => { closes++; });
    socket.onMessage((data) => {
      const frame = JSON.parse(String(data));
      if (frame.t === "hello") socket.send(JSON.stringify({ ...WELCOME, capabilities: capable ? ["read", "resume_probe"] : ["read"] }));
      if (frame.t === "sub") socket.send(JSON.stringify({ t: "synced", sub_id: frame.sub_id, seq: 0 }));
      if (frame.t === "ping") {
        pings.push(frame.nonce);
        if (healthy) socket.send(JSON.stringify({ t: "pong", nonce: frame.nonce }));
      }
    });
  });
  await page.goto(`${ORIGIN}/e2e/resume/index.html`);
  await page.locator('[data-testid="host-status"][data-health="live"]').waitFor();
  return { page, pings, connections: () => connections, closes: () => closes };
}

const restore = (page: Page) => page.evaluate(() => window.dispatchEvent(new PageTransitionEvent("pageshow", { persisted: true })));
const visibleAgain = (page: Page) => page.evaluate(() => {
  for (const state of ["hidden", "visible"]) {
    Object.defineProperty(document, "visibilityState", { configurable: true, value: state });
    document.dispatchEvent(new Event("visibilitychange"));
  }
});

for (const [name, resume] of [["BFCache", restore], ["hidden to visible", visibleAgain]] as const) {
  test(`${name}: silently dead open socket reconnects by the probe deadline`, async () => {
    const h = await open();
    try {
      await resume(h.page);
      await h.page.clock.runFor(RESUME_PROBE_MS - 1);
      expect(h.pings).toHaveLength(1);
      expect(h.closes()).toBe(0);
      await h.page.clock.runFor(1);
      expect(await h.page.locator("body").textContent()).toContain("Reconnecting");
      await h.page.clock.runFor(500);
      expect(h.connections()).toBe(2);
      expect(h.closes()).toBe(1);
    } finally { await h.page.close(); }
  });
}

test("a healthy browser socket stays connected with no visible change", async () => {
  const h = await open(true);
  try {
    const before = await h.page.locator("body").textContent();
    await restore(h.page);
    await h.page.clock.runFor(RESUME_PROBE_MS + 500);
    expect(h.pings).toHaveLength(1);
    expect(h.connections()).toBe(1);
    expect(h.closes()).toBe(0);
    expect(await h.page.locator("body").textContent()).toBe(before);
  } finally { await h.page.close(); }
});

test("an older server uses the bounded heartbeat fallback", async () => {
  const h = await open(false, false);
  try {
    await visibleAgain(h.page);
    await h.page.clock.runFor(RESUME_FALLBACK_MS - 1);
    expect(h.pings).toHaveLength(0);
    expect(h.closes()).toBe(0);
    await h.page.clock.runFor(501);
    expect(h.connections()).toBe(2);
  } finally { await h.page.close(); }
});


test("browser resume signals share one pending probe", async () => {
  const h = await open();
  try {
    await restore(h.page);
    await visibleAgain(h.page);
    await restore(h.page);
    await h.page.clock.runFor(RESUME_PROBE_MS - 1);
    expect(h.pings).toHaveLength(1);
    expect(h.closes()).toBe(0);
    await h.page.clock.runFor(501);
    expect(h.connections()).toBe(2);
  } finally { await h.page.close(); }
});

test("disposing the browser client prevents later resume work", async () => {
  const h = await open();
  try {
    await h.page.evaluate(() => Reflect.get(globalThis, "disposeResume")());
    await restore(h.page);
    await visibleAgain(h.page);
    await h.page.clock.runFor(60_000);
    expect(h.pings).toHaveLength(0);
    expect(h.connections()).toBe(1);
    expect(h.closes()).toBe(1);
  } finally { await h.page.close(); }
});
