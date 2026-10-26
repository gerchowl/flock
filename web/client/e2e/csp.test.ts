/**
 * The production bundle under the served document's Content-Security-Policy
 * (#1848).
 *
 * Builds the production bundle and serves it from a loopback host
 * (`e2e/csp-server.ts`) that sends `flk`'s exact entry-document policy, read
 * from the same file the Rust router embeds (`src/web/gui/document-policy.txt`).
 * Chromium loads the page with every `securitypolicyviolation` recorded from
 * before the first script runs. The checks:
 *
 * - the fixture-configured workbench and the live sign-in path (code
 *   exchange, then the `/v1/ws` hello) run with no violation at all;
 * - controls prove the policy is enforced, not merely absent: an inline
 *   script, an inline style and a WebSocket to another loopback port are
 *   blocked, and that other server receives nothing.
 *
 * The test configuration is injected with Playwright's init script, which
 * the browser runs outside the page's policy, so the served document needs
 * no inline-script exception. Every wait is on an event with a deadline,
 * never a timer.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { chromium, type Browser, type Page } from "playwright";

import { CLIENT_ROOT, buildClient } from "../tests/build";

const DIST = `${CLIENT_ROOT}/artifacts/csp-dist`;
const POLICY_FILE = `${CLIENT_ROOT}/../../src/web/gui/document-policy.txt`;
const FIXTURE = `${CLIENT_ROOT}/../../tests/fixtures/claude-session/events.jsonl`;
const CODE = "a".repeat(64);
const READY_MS = 10_000;

type Event = {
  ready?: { site: number; foreign: number };
  request?: string;
  socket?: string;
  frame?: string;
  foreign?: string;
};

/** The host's observations, and waiters for the next matching one. */
class Host {
  readonly events: Event[] = [];
  #waiters: { match: (event: Event) => boolean; resolve: (event: Event) => void }[] = [];
  readonly child = Bun.spawn([process.execPath, `${CLIENT_ROOT}/e2e/csp-server.ts`, DIST, POLICY_FILE], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "inherit",
  });

  constructor() {
    void this.#read();
  }

  async #read(): Promise<void> {
    let buffer = "";
    for await (const chunk of this.child.stdout.pipeThrough(new TextDecoderStream())) {
      buffer += chunk;
      let end: number;
      while ((end = buffer.indexOf("\n")) >= 0) {
        const event = JSON.parse(buffer.slice(0, end)) as Event;
        buffer = buffer.slice(end + 1);
        this.events.push(event);
        this.#waiters = this.#waiters.filter((waiter) => !(waiter.match(event) && (waiter.resolve(event), true)));
      }
    }
  }

  /** The first observation (past or future) matching `match`, within the deadline. */
  next(match: (event: Event) => boolean, what: string): Promise<Event> {
    const found = this.events.find(match);
    if (found) return Promise.resolve(found);
    const deadline = AbortSignal.timeout(READY_MS);
    return new Promise((resolve, reject) => {
      this.#waiters.push({ match, resolve });
      deadline.addEventListener("abort", () => reject(new Error(`csp host: no ${what} before the deadline`)), { once: true });
      this.child.exited.then((code) => reject(new Error(`csp host exited ${code} before ${what}`)));
    });
  }
}

let browser: Browser;
let host: Host;
let site: number;
let foreign: number;

beforeAll(async () => {
  await buildClient("production", DIST);
  host = new Host();
  const ready = (await host.next((event) => event.ready !== undefined, "ready line")).ready!;
  ({ site, foreign } = ready);
  browser = await chromium.launch();
});

afterAll(async () => {
  await browser?.close();
  host?.child.stdin.end();
  await host?.child.exited;
});

/** A page whose violations and errors are recorded before any page script runs. */
async function open(config?: object): Promise<{ page: Page; errors: string[] }> {
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript((shell) => {
    const violations: string[] = [];
    Reflect.set(globalThis, "__cspViolations", violations);
    document.addEventListener("securitypolicyviolation", (event) => {
      violations.push(`${event.effectiveDirective} ${event.blockedURI}`);
    });
    if (shell !== null) Reflect.set(globalThis, "__FLOCK_SHELL_CONFIG__", shell);
  }, config ?? null);
  return { page, errors };
}

function violations(page: Page): Promise<string[]> {
  return page.evaluate(() => [...(Reflect.get(globalThis, "__cspViolations") as string[])]);
}

describe("served document policy", () => {
  test("the entry document carries the embedded policy with the listener authority", async () => {
    const page = await browser.newPage();
    const response = await page.goto(`http://127.0.0.1:${site}/`);
    expect(response?.headers()["content-security-policy"]).toBe(
      `default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self' ws://127.0.0.1:${site}/v1/ws; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'`,
    );
    await page.close();
  });

  test("the fixture workbench renders with no violation", async () => {
    const fixtureJsonl = await Bun.file(FIXTURE).text();
    const { page, errors } = await open({ transport: "fixture", fixtureJsonl, source: "csp-probe" });
    await page.goto(`http://127.0.0.1:${site}/`);
    await page.getByTestId("transcript-synced").waitFor({ timeout: READY_MS });
    // The external stylesheet applied under style-src 'self'.
    expect(await page.evaluate(() => parseFloat(getComputedStyle(document.body).fontSize))).toBe(14);
    expect(await violations(page)).toEqual([]);
    expect(errors).toEqual([]);
    await page.close();
  });

  test("the live sign-in exchanges its code and says hello on /v1/ws with no violation", async () => {
    const { page, errors } = await open();
    await page.goto(`http://127.0.0.1:${site}/#code=${CODE}`);
    await host.next((event) => event.request === "POST /v1/client/exchange", "code exchange");
    await host.next((event) => event.request === "GET /v1/ws", "WebSocket upgrade");
    const hello = await host.next((event) => event.frame?.includes("csp-probe-token") ?? false, "hello frame");
    expect(hello.frame).toContain('"hello"');
    expect(await violations(page)).toEqual([]);
    expect(errors).toEqual([]);
    await page.close();
  });

  test("inline script, inline style and a foreign WebSocket are blocked", async () => {
    const { page } = await open({ transport: "error", message: "csp control" });
    await page.goto(`http://127.0.0.1:${site}/`);
    await page.getByTestId("config-error").waitFor({ timeout: READY_MS });
    const outcome = await page.evaluate(async (port) => {
      const blocked = (directive: string) =>
        new Promise<void>((resolve) => {
          const listen = (event: SecurityPolicyViolationEvent) => {
            if (event.effectiveDirective !== directive) return;
            document.removeEventListener("securitypolicyviolation", listen);
            resolve();
          };
          document.addEventListener("securitypolicyviolation", listen);
        });
      const script = blocked("script-src-elem");
      const inline = document.createElement("script");
      inline.textContent = "globalThis.__inlineRan = true";
      document.head.append(inline);
      await script;
      const style = blocked("style-src-elem");
      const sheet = document.createElement("style");
      sheet.textContent = "body { font-size: 99px }";
      document.head.append(sheet);
      await style;
      const connect = blocked("connect-src");
      try {
        new WebSocket(`ws://127.0.0.1:${port}/v1/ws`);
      } catch {
        // Some engines throw instead of failing the socket; both are refusals.
      }
      await connect;
      return {
        inlineRan: Reflect.get(globalThis, "__inlineRan") === true,
        fontSize: parseFloat(getComputedStyle(document.body).fontSize),
      };
    }, foreign);
    expect(outcome.inlineRan).toBe(false);
    expect(outcome.fontSize).not.toBe(99);
    expect(host.events.filter((event) => event.foreign !== undefined)).toEqual([]);
    await page.close();
  });
});
