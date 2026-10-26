/**
 * A shell's token provider (ADR-0038 §6, the Tauri shell's zero-touch mint):
 * asked at connect, after a 4403 and on the person's Connect; kept in memory
 * only; never a token prompt; bounded re-mints. All time and sockets are
 * virtual (ADR-0037).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App, SHELL_OWNS_TOKENS } from "../src/App";
import { readShellConfig, type TokenProvider } from "../src/config";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { MAX_REMINTS, REMINTS_SPENT } from "../src/wire/provider";
import { RUN_CLIENT_OPEN, type SessionLike } from "../src/wire/token";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
});

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const PENDING = { cmd_id: "c0", verb: "agent.send", args: { target: "a", text: "hi", submit: true }, principal: "fixture-principal", created_ms: 0, status: "pending", first_sent_ms: null } as const;
const CODE = "a".repeat(64);
const PANE = "The flk server refused the mint: this app was started from inside a flk pane.";

type Reply = string | { reject: unknown } | { throws: unknown } | { value: unknown } | "deferred";

/** A provider that answers `replies` in order and counts its calls; "deferred" waits for `resolve`. */
function provider(...replies: Reply[]) {
  let release: ((token: string) => void) | undefined;
  const ask = Object.assign(
    (): Promise<string> => {
      const reply = replies[ask.calls++];
      if (reply === undefined) throw new Error("an unscripted provider call");
      if (reply === "deferred") return new Promise((resolve) => (release = resolve));
      if (typeof reply === "string") return Promise.resolve(reply);
      if ("throws" in reply) throw reply.throws;
      if ("value" in reply) return reply.value as Promise<string>;
      return Promise.reject(reply.reject);
    },
    { calls: 0, resolve: (token: string) => release?.(token) },
  );
  return ask;
}

/** A page whose fragment the test changes the way pasting a link does. */
function page(hash = "") {
  const listeners = new Set<() => void>();
  const location = { hash, pathname: "/", search: "" };
  const history = { state: null, replaceState: (_s: unknown, _u: string, url: string) => void (location.hash = url.includes("#") ? url.slice(url.indexOf("#")) : "") };
  return {
    location,
    history,
    addEventListener: (_type: "hashchange", listener: () => void) => void listeners.add(listener),
    removeEventListener: (_type: "hashchange", listener: () => void) => void listeners.delete(listener),
    paste: (next: string) => {
      location.hash = next;
      listeners.forEach((listener) => listener());
    },
  };
}

function mount(tokenProvider: TokenProvider | undefined, options: { outbox?: readonly (typeof PENDING)[]; code?: string; at?: ReturnType<typeof page> } = {}) {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [...(options.outbox ?? [])] }));
  const entries = new Map<string, string>();
  const session: SessionLike = { getItem: (key) => entries.get(key) ?? null, setItem: (key, value) => void entries.set(key, value), removeItem: (key) => void entries.delete(key) };
  const requests: string[] = [];
  const root = document.createElement("div");
  document.body.append(root);
  const config = { ok: true, config: { transport: "wire", serverUrl: "https://flk.invalid", ...(tokenProvider === undefined ? {} : { tokenProvider }) } } as const;
  const request = async (url: string) => {
    requests.push(url);
    return { status: 500, json: async () => null };
  };
  const env = { clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store, session: () => session, request, page: options.at ?? page() };
  dispose = render(() => <App config={config} env={env} code={options.code ?? null} />, root);
  return { h, root, entries, requests };
}

const status = (root: HTMLElement): string => root.querySelector(".topbar__status")?.textContent ?? "";
const prompt = (root: HTMLElement): Element | null => root.querySelector('input[aria-label="New client token"]');
const click = (root: HTMLElement, selector: string): void => (root.querySelector(selector) as HTMLButtonElement).click();
const frames = (h: Harness, t: string): Record<string, unknown>[] => sentFrames(h).filter((frame) => (frame as { t: string }).t === t) as Record<string, unknown>[];
const send = (h: Harness, frame: unknown): void => h.step(step({ step: "send", frame: frame as ServerFrame }));
const fleetSubs = (h: Harness): Record<string, unknown>[] => frames(h, "sub").filter((frame) => frame["stream"] === "fleet");

/** Opens the current socket, welcomes it as `principal` and syncs the fleet stream at `seq`: Live. */
function welcome(h: Harness, seq: number, principal = WELCOME.t === "welcome" ? WELCOME.principal : ""): void {
  h.step(step({ step: "open" }));
  send(h, { ...WELCOME, principal });
  const fleet = fleetSubs(h).at(-1)?.["sub_id"] as string;
  send(h, { t: "gap", sub_id: fleet, from: 1, to: seq, snapshot_to: seq });
  send(h, { t: "synced", sub_id: fleet, seq });
}

describe("with a token provider", () => {
  test("the client connects with its token and shows no token prompt; a reload asks again, nothing is stored", async () => {
    const first = provider("t1");
    const { h, root, entries } = mount(first);
    expect(status(root)).toBe("Signing in…");
    expect(prompt(root)).toBeNull();
    await tick();
    expect([first.calls, h.net.attempts.length]).toEqual([1, 1]);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1"]);
    expect(status(root)).toBe("Live");
    expect(entries.size).toBe(0);
    dispose?.();
    const reloaded = provider("t2");
    const again = mount(reloaded);
    await tick();
    again.h.step(step({ step: "open" }));
    expect(frames(again.h, "hello").map((frame) => frame["token"])).toEqual(["t2"]);
    expect(reloaded.calls).toBe(1);
  });

  test("a 4403 re-mint welcomed as the same principal keeps cursors and the outbox: the queued command goes out with its cmd_id", async () => {
    const tokens = provider("t1", "t2");
    const { h, root } = mount(tokens, { outbox: [PENDING] });
    await tick();
    // The first socket applies the fleet up to 7 and is revoked before it is Live, so c0 is still queued.
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    send(h, { t: "gap", sub_id: fleetSubs(h)[0]?.["sub_id"], from: 1, to: 7, snapshot_to: 7 });
    h.step(step({ step: "token_refused", code: 4403 }));
    expect(prompt(root)).toBeNull();
    await tick();
    expect([tokens.calls, h.net.attempts.length]).toEqual([2, 2]);
    // The app-session principal is stable across re-mints (ADR-0036's app-session principal).
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t2"]);
    expect(fleetSubs(h).map((frame) => frame["after"])).toEqual([undefined, 7]);
    send(h, { t: "synced", sub_id: fleetSubs(h)[1]?.["sub_id"], seq: 7 });
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd").map((frame) => frame["cmd_id"])).toEqual(["c0"]);
    expect(root.querySelector('[data-testid="outbox-problem"]')).toBeNull();
  });

  test("a re-minted token welcomed as another principal drops the outbox and says so", async () => {
    const tokens = provider("t1", "t2");
    const { h, root } = mount(tokens, { outbox: [PENDING] });
    await tick();
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    welcome(h, 0, "another-principal");
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd")).toEqual([]);
    expect(root.querySelector('[data-testid="outbox-problem"]')?.textContent).toBe("Dropped 1 unsent command: the server now names another identity, and commands never run under another.");
  });

  for (const [name, reply] of [
    ["a rejected string", { reject: PANE }],
    ["a rejected Error", { reject: new Error(PANE) }],
    ["a synchronous throw", { throws: new Error(PANE) }],
  ] as const) {
    test(`a provider failure (${name}) shows its message in NeedsToken, and is not retried`, async () => {
      const tokens = provider(reply);
      const { h, root } = mount(tokens);
      await tick();
      expect(status(root)).toBe(`NeedsToken · ${PANE}`);
      expect(prompt(root)).toBeNull();
      h.clock.advanceTo(h.clock.now() + 120_000);
      await tick();
      expect([tokens.calls, h.net.attempts.length]).toEqual([1, 0]);
    });
  }

  test("a provider that returns no promise lands in NeedsToken, never a crash", async () => {
    const { h, root } = mount(provider({ value: undefined }));
    await tick();
    expect(status(root)).toBe("NeedsToken · The shell gave no token.");
    expect(h.net.attempts).toHaveLength(0);
  });

  test("after a failure, Connect asks the provider again", async () => {
    const tokens = provider({ reject: PANE }, "t2");
    const { h, root } = mount(tokens);
    await tick();
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect(tokens.calls).toBe(2);
    h.step(step({ step: "open" }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t2"]);
  });

  test("a fresh token refused before its welcome is not asked for again", async () => {
    const tokens = provider("t1");
    const { h, root } = mount(tokens);
    await tick();
    h.step(step({ step: "open" }));
    h.step(step({ step: "token_refused", code: 4401 }));
    await tick();
    expect(tokens.calls).toBe(1);
    expect(status(root)).toBe("NeedsToken · The server refused the token.");
  });

  test(`a server that revokes every token at once is asked ${MAX_REMINTS} times on §3's backoff, then NeedsToken says why`, async () => {
    const tokens = provider(...Array.from({ length: MAX_REMINTS + 2 }, (_, index) => `t${index}`));
    const { h, root } = mount(tokens);
    await tick();
    for (let remint = 1; remint <= MAX_REMINTS + 1; remint++) {
      welcome(h, 0);
      h.step(step({ step: "token_refused", code: 4403 }));
      await tick();
      // The first re-mint is immediate; later ones wait for the backoff (random 0: half its ceiling).
      if (remint >= 2 && remint <= MAX_REMINTS) {
        expect(tokens.calls).toBe(remint);
        h.clock.advanceTo(h.clock.now() + Math.min(30_000, 500 * 2 ** (remint - 2)) / 2 - 1);
        await tick();
        expect(tokens.calls).toBe(remint);
        h.clock.advanceTo(h.clock.now() + 1);
        await tick();
      }
      expect(tokens.calls).toBe(Math.min(remint, MAX_REMINTS) + 1);
    }
    expect(status(root)).toBe(`NeedsToken · The token was revoked. ${REMINTS_SPENT}`);
    h.clock.advanceTo(h.clock.now() + 600_000);
    await tick();
    expect(tokens.calls).toBe(MAX_REMINTS + 1);
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect(tokens.calls).toBe(MAX_REMINTS + 2);
  });

  test("an acked command ends the re-mint streak: the next 4403 re-mints at once again", async () => {
    const tokens = provider("t1", "t2", "t3", "t4");
    const { h } = mount(tokens, { outbox: [PENDING] });
    await tick();
    // Revoked before Live: c0 stays queued; the first re-mint is immediate.
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(2);
    // Live: c0 goes out and is acked, so this second 4403 starts a new streak and re-mints at once.
    welcome(h, 0);
    send(h, { t: "ack", cmd_id: "c0", ok: true, result: null });
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(3);
    // Without an ack, the next one waits for the backoff.
    welcome(h, 0);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(3);
  });

  test("a Disconnect while the provider is asked: its token does not reconnect, and the next Connect uses it", async () => {
    const tokens = provider("deferred");
    const { h, root } = mount(tokens);
    await tick();
    expect(tokens.calls).toBe(1);
    click(root, ".connection-bar__toggle");
    expect(status(root)).toBe("Disconnected");
    tokens.resolve("late");
    await tick();
    expect(h.net.attempts).toHaveLength(0);
    expect(status(root)).toBe("Disconnected");
    click(root, ".connection-bar__toggle");
    h.step(step({ step: "open" }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["late"]);
    expect(tokens.calls).toBe(1);
  });

  test("a provider resolving after the app is gone opens nothing", async () => {
    const tokens = provider("deferred");
    const { h } = mount(tokens);
    await tick();
    expect(tokens.calls).toBe(1);
    dispose?.();
    dispose = undefined;
    tokens.resolve("late");
    await tick();
    expect(h.net.attempts).toHaveLength(0);
  });

  test("Connect pressed again while the provider is asked makes no second call", async () => {
    const tokens = provider({ reject: PANE }, "deferred");
    const { root } = mount(tokens);
    await tick();
    click(root, '[data-testid="shell-connect"]');
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect(tokens.calls).toBe(2);
  });

  test("an auth_revoked ack before the 4403 is one provider call", async () => {
    const tokens = provider("t1", "t2");
    const { h } = mount(tokens, { outbox: [PENDING] });
    await tick();
    welcome(h, 0);
    send(h, { t: "ack", cmd_id: "c0", ok: false, error: { code: "auth_revoked", exit: 1, message: "token revoked", retry: "after_human" } });
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    h.clock.advanceTo(h.clock.now() + 120_000);
    await tick();
    expect(tokens.calls).toBe(2);
  });

  test("a link's code is refused with a notice: never exchanged, never stored", async () => {
    const at = page();
    const tokens = provider("t1");
    const { h, root, entries, requests } = mount(tokens, { code: CODE, at });
    await tick();
    expect(root.querySelector('[data-testid="exchange-notice"]')?.textContent).toContain(SHELL_OWNS_TOKENS);
    h.step(step({ step: "open" }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1"]);
    at.paste(`#code=${"b".repeat(64)}`);
    await tick();
    expect(at.location.hash).toBe("");
    expect([requests, [...entries.keys()], tokens.calls]).toEqual([[], [], 1]);
  });
});

test("without a provider, NeedsToken still prompts for a token and names flk client open", () => {
  const { h, root } = mount(undefined);
  expect(h.net.attempts).toHaveLength(0);
  expect(status(root)).toBe(`NeedsToken · ${RUN_CLIENT_OPEN}`);
  expect(prompt(root)).not.toBeNull();
  expect(root.querySelector('[data-testid="shell-connect"]')).toBeNull();
});

test("the contract: a provider is a function, and never beside a fixed token", () => {
  const scope = (config: object) => ({ __FLOCK_SHELL_CONFIG__: config });
  const tokenProvider: TokenProvider = async () => "t";
  expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider }))).toEqual({ ok: true, config: { transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider } });
  for (const bad of [{ tokenProvider: "shell" }, { tokenProvider, token: "t" }]) {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", ...bad })).ok).toBe(false);
  }
});
