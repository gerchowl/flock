/**
 * A shell's token provider (ADR-0038 §6, the Tauri shell's zero-touch mint):
 * asked at connect, after a 4403 and on the person's Connect; kept in memory
 * only; never a token prompt; bounded re-mints. All time and sockets are
 * virtual (ADR-0037).
 *
 * The suite runs twice: against a token provider, and against a shell's
 * `openBridge` (#2500), whose fake plays the native side. Each of its
 * sessions is a scripted socket that adds the session's bearer to the
 * client's `hello`, so a reply's token names the bearer that `hello` went
 * out with, in both modes. A bridge is also asked on every reconnect, where
 * a provider's token is reused; the few tests with a reconnect say so.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App, SHELL_OWNS_TOKENS } from "../src/App";
import { BRIDGE_ALONE, readShellConfig, type BridgeSocket, type OpenBridge, type ShellToken, type TokenProvider } from "../src/config";
import type { ServerFrame } from "../src/gen/ServerFrame";
import { OUTBOX_KEY } from "../src/wire/outbox";
import { WireConnection } from "../src/wire/connection";
import { MAX_REMINTS, MAX_TIMER_DELAY, REMINTS_SPENT, RENEW_AT, driveBridge, driveTokenProvider } from "../src/wire/provider";
import { RUN_CLIENT_OPEN, type SessionLike } from "../src/wire/token";
import { FIXTURE_TOKEN, Harness, WELCOME, live, sentFrames, step, type FakeNetwork, type FakeSocket } from "./wire/harness";

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

type Mode = "provider" | "bridge";
const MODES: readonly Mode[] = ["provider", "bridge"];

/** A shell's value: `token` is the bearer; `expiresInMs` is passed on as given, valid or not. */
type Issued = { readonly token: string; readonly expiresInMs?: unknown };
type Reply = string | Issued | { reject: unknown } | { throws: unknown } | { value: unknown } | "deferred";

/** The native side's current bearer: when it was minted, and whether the server refused or ended it. */
type Bearer = Issued & { readonly mintedAt: number; refused: boolean };

/**
 * A bridged session as the native side opens it on `bearer`: a scripted
 * socket on `net` that adds the bearer to the client's first frame, the
 * `hello`, which it records as the client sent it. A 4401 or 4403 marks the
 * bearer refused, so the next open mints.
 */
function bridgedSession(net: FakeNetwork, bearer: Bearer, hellos: Record<string, unknown>[]): BridgeSocket {
  const fake = net.factory("bridge:") as FakeSocket;
  let first = true;
  const socket: BridgeSocket = {
    send: (text) => {
      if (!first) return fake.send(text);
      first = false;
      const hello = JSON.parse(text) as Record<string, unknown>;
      hellos.push(hello);
      fake.send(JSON.stringify({ ...hello, token: bearer.token }));
    },
    close: (code = 1000, reason = "") => fake.close(code, reason),
    onmessage: null,
    onclose: null,
    onerror: null,
  };
  // Open already: the harness's `open` step finds no handler and does nothing.
  fake.onmessage = (event) => socket.onmessage?.(event.data as string);
  fake.onclose = (event) => {
    if (event.code === 4401 || event.code === 4403) bearer.refused = true;
    socket.onclose?.(event.code, event.reason);
  };
  fake.onerror = () => socket.onerror?.();
  return socket;
}

/**
 * A shell for `mode` that answers `replies` in order (a string is
 * `{ token }`) and counts its calls; "deferred" waits for `resolve`.
 *
 * A bridge plays the native side (ADR-0038 §6, "When the native side
 * mints"): it keeps one bearer, and only a mint takes the next reply, so
 * `calls` counts mints, as a provider's calls do. It mints when it has no
 * bearer, on `renew: true`, and after the server refused or ended the
 * current one; any other open reuses it, with its remaining lifetime. It
 * records every open's `renew`, every `hello` its client sent, and when each
 * session with a lifetime was opened.
 */
function provider(mode: Mode, ...replies: Reply[]) {
  let release: ((issued: Issued) => void) | undefined;
  let bearer: Bearer | null = null;
  const shell = {
    mode,
    calls: 0,
    renews: [] as boolean[],
    hellos: [] as Record<string, unknown>[],
    lifetimes: [] as { readonly at: number; readonly expiresInMs: number }[],
    net: null as FakeNetwork | null,
    resolve: (token: string) => release?.({ token }),
    /** A bridge session on a bearer minted outside the script, as before a driver is attached. */
    open: (token: string): BridgeSocket => session((bearer = { token, mintedAt: network().clock.now(), refused: false })),
    config: {} as { readonly tokenProvider: TokenProvider } | { readonly openBridge: OpenBridge },
  };
  const network = (): FakeNetwork => shell.net ?? (() => { throw new Error("the shell has no network yet"); })();
  const session = (on: Bearer, expiresInMs?: unknown): BridgeSocket => {
    if (typeof expiresInMs === "number" && expiresInMs > 0) shell.lifetimes.push({ at: network().clock.now(), expiresInMs });
    return bridgedSession(network(), on, shell.hellos);
  };
  const value = (issued: Issued): unknown => {
    if (mode === "provider") {
      if (typeof issued.expiresInMs === "number" && issued.expiresInMs > 0) shell.lifetimes.push({ at: network().clock.now(), expiresInMs: issued.expiresInMs });
      return issued;
    }
    bearer = { ...issued, mintedAt: network().clock.now(), refused: false };
    const socket = session(bearer, issued.expiresInMs);
    return "expiresInMs" in issued ? { socket, expiresInMs: issued.expiresInMs } : { socket };
  };
  const answer = (): Promise<unknown> => {
    const reply = replies[shell.calls++];
    if (reply === undefined) throw new Error("an unscripted shell call");
    if (reply === "deferred") return new Promise<Issued>((resolve) => (release = resolve)).then(value);
    if (typeof reply === "string") return Promise.resolve(value({ token: reply }));
    if ("token" in reply) return Promise.resolve(value(reply));
    if ("throws" in reply) throw reply.throws;
    if ("value" in reply) return reply.value as Promise<unknown>;
    return Promise.reject(reply.reject);
  };
  /** The current bearer again, with what is left of its lifetime. */
  const reuse = (current: Bearer): Promise<unknown> => {
    const left = typeof current.expiresInMs === "number" ? current.expiresInMs - (network().clock.now() - current.mintedAt) : undefined;
    const socket = session(current, left);
    return Promise.resolve(left !== undefined && left > 0 ? { socket, expiresInMs: left } : { socket });
  };
  shell.config =
    mode === "provider"
      ? { tokenProvider: () => answer() as Promise<ShellToken> }
      : {
          openBridge: ({ renew }) => {
            shell.renews.push(renew);
            return (!renew && bearer !== null && !bearer.refused ? reuse(bearer) : answer()) as ReturnType<OpenBridge>;
          },
        };
  return shell;
}
type Shell = ReturnType<typeof provider>;

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

function mount(shell: Shell | undefined, options: { outbox?: readonly (typeof PENDING)[]; stored?: unknown; code?: string; at?: ReturnType<typeof page> } = {}) {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [...(options.outbox ?? [])] }));
  if (shell !== undefined) shell.net = h.net;
  if (options.stored !== undefined) h.store.setItem(OUTBOX_KEY, JSON.stringify(options.stored));
  const entries = new Map<string, string>();
  const session: SessionLike = { getItem: (key) => entries.get(key) ?? null, setItem: (key, value) => void entries.set(key, value), removeItem: (key) => void entries.delete(key) };
  const requests: string[] = [];
  const root = document.createElement("div");
  document.body.append(root);
  const config = { ok: true, config: shell?.mode === "bridge" ? { transport: "bridge", ...shell.config } : { transport: "wire", serverUrl: "https://flk.invalid", ...shell?.config } } as Parameters<typeof App>[0]["config"];
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
/** Sessions the client started: a provider's sockets, or the bridged sessions a `hello` went out on. */
const started = (h: Harness, mode: Mode): number => (mode === "bridge" ? frames(h, "hello").length : h.net.attempts.length);
const fleetSubs = (h: Harness): Record<string, unknown>[] => frames(h, "sub").filter((frame) => frame["stream"] === "fleet");

/** Opens the current socket, welcomes it as `principal` and syncs the fleet stream at `seq`: Live. */
function welcome(h: Harness, seq: number, principal = WELCOME.t === "welcome" ? WELCOME.principal : ""): void {
  h.step(step({ step: "open" }));
  send(h, { ...WELCOME, principal });
  const fleet = fleetSubs(h).at(-1)?.["sub_id"] as string;
  send(h, { t: "gap", sub_id: fleet, from: 1, to: seq, snapshot_to: seq });
  send(h, { t: "synced", sub_id: fleet, seq });
}

for (const mode of MODES) {
  describe(`with a shell ${mode}`, () => {
  test("the client connects with its token and shows no token prompt; a reload asks again, nothing is stored", async () => {
    const first = provider(mode, "t1");
    const { h, root, entries } = mount(first);
    // A bridged attempt waits for its session in Connecting; a provider's token is asked for in NeedsToken.
    expect(status(root)).toBe(mode === "bridge" ? "Connecting" : "Signing in…");
    expect(prompt(root)).toBeNull();
    await tick();
    expect([first.calls, started(h, mode)]).toEqual([1, 1]);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1"]);
    // The client's own `hello` on a bridge carries no token key at all: the native side adds its bearer.
    expect(first.hellos.map((frame) => "token" in frame)).toEqual(mode === "bridge" ? [false] : []);
    expect(status(root)).toBe("Live");
    expect(entries.size).toBe(0);
    dispose?.();
    const reloaded = provider(mode, "t2");
    const again = mount(reloaded);
    await tick();
    again.h.step(step({ step: "open" }));
    expect(frames(again.h, "hello").map((frame) => frame["token"])).toEqual(["t2"]);
    expect(reloaded.calls).toBe(1);
  });

  test("a 4403 re-mint welcomed as the same principal keeps cursors and the outbox: the queued command goes out with its cmd_id", async () => {
    const tokens = provider(mode, "t1", "t2");
    const { h, root } = mount(tokens, { outbox: [PENDING] });
    await tick();
    // The first socket applies the fleet up to 7 and is revoked before it is Live, so c0 is still queued.
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    send(h, { t: "gap", sub_id: fleetSubs(h)[0]?.["sub_id"], from: 1, to: 7, snapshot_to: 7 });
    h.step(step({ step: "token_refused", code: 4403 }));
    expect(prompt(root)).toBeNull();
    await tick();
    expect([tokens.calls, started(h, mode)]).toEqual([2, 2]);
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

  test("a re-minted token welcomed as another principal drops the outbox and says so, beside the restore problem, until dismissed", async () => {
    const tokens = provider(mode, "t1", "t2");
    const stored = { principal: "fixture-principal", commands: [PENDING, "junk"] };
    const { h, root } = mount(tokens, { stored });
    await tick();
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    welcome(h, 0, "another-principal");
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd")).toEqual([]);
    const dropped = root.querySelector('[data-testid="outbox-dropped"]');
    expect(dropped?.textContent).toContain("Dropped 1 unsent command: the server now names another identity, and commands never run under another.");
    expect(root.querySelector('[data-testid="outbox-problem"]')?.textContent).toBe("Dropped unreadable stored command: record 2.");
    (dropped?.querySelector("button") as HTMLButtonElement).click();
    expect(root.querySelector('[data-testid="outbox-dropped"]')).toBeNull();
    expect(root.querySelector('[data-testid="outbox-problem"]')).not.toBeNull();
  });

  for (const [name, reply] of [
    ["a rejected string", { reject: PANE }],
    ["a rejected Error", { reject: new Error(PANE) }],
    ["a synchronous throw", { throws: new Error(PANE) }],
  ] as const) {
    test(`a provider failure (${name}) shows its message in NeedsToken, and is not retried`, async () => {
      const tokens = provider(mode, reply);
      const { h, root } = mount(tokens);
      await tick();
      expect(status(root)).toBe(`NeedsToken · ${PANE}`);
      expect(prompt(root)).toBeNull();
      h.clock.advanceTo(h.clock.now() + 120_000);
      await tick();
      expect([tokens.calls, started(h, mode)]).toEqual([1, 0]);
    });
  }

  test("a provider that returns no promise lands in NeedsToken, never a crash", async () => {
    const { h, root } = mount(provider(mode, { value: undefined }));
    await tick();
    expect(status(root)).toBe(`NeedsToken · ${mode === "bridge" ? "The shell gave no bridge session." : "The shell gave no token."}`);
    expect(started(h, mode)).toBe(0);
  });

  test("after a failure, Connect asks the provider again", async () => {
    const tokens = provider(mode, { reject: PANE }, "t2");
    const { h, root } = mount(tokens);
    await tick();
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect(tokens.calls).toBe(2);
    h.step(step({ step: "open" }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t2"]);
  });

  test("after a server restart, the welcomed token refused at hello (4401) is one re-mint: Live as the same principal, the outbox kept", async () => {
    const tokens = provider(mode, "t1", "t2");
    const { h, root } = mount(tokens, { outbox: [PENDING] });
    await tick();
    // Welcomed, then the server goes away; c0 is still queued (the fleet never synced).
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    h.step(step({ step: "drop" }));
    h.clock.advanceTo(h.clock.now() + 30_000);
    // A bridge's reconnect opens a session on the same bearer: the native side mints nothing for a 1006.
    await tick();
    // The restarted server no longer knows t1.
    h.step(step({ step: "open" }));
    h.step(step({ step: "token_refused", code: 4401 }));
    await tick();
    expect(tokens.calls).toBe(2);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t1", "t2"]);
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd").map((frame) => frame["cmd_id"])).toEqual(["c0"]);
  });

  test("a never-welcomed token refused at hello counts toward the streak: re-minted on the backoff, and the cap holds", async () => {
    const tokens = provider(mode, ...Array.from({ length: MAX_REMINTS + 1 }, (_, index) => `t${index}`));
    const { h, root } = mount(tokens);
    await tick();
    for (let refused = 1; refused <= MAX_REMINTS + 1; refused++) {
      h.step(step({ step: "open" }));
      h.step(step({ step: "token_refused", code: 4401 }));
      await tick();
      h.clock.advanceTo(h.clock.now() + 30_000);
      await tick();
    }
    expect(tokens.calls).toBe(MAX_REMINTS + 1);
    expect(status(root)).toBe(`NeedsToken · The server refused the token. ${REMINTS_SPENT}`);
  });

  test(`a server that revokes every token at once is asked ${MAX_REMINTS} times on §3's backoff, then NeedsToken says why`, async () => {
    const tokens = provider(mode, ...Array.from({ length: MAX_REMINTS + 2 }, (_, index) => `t${index}`));
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
    const tokens = provider(mode, "t1", "t2", "t3", "t4");
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

  test("a session Live for §3's healthy period ends the streak: every revocation re-mints at once, and the cap never trips", async () => {
    const tokens = provider(mode, ...Array.from({ length: MAX_REMINTS + 3 }, (_, index) => `t${index}`));
    const { h, root } = mount(tokens);
    await tick();
    for (let revoked = 1; revoked <= MAX_REMINTS + 2; revoked++) {
      welcome(h, 0);
      // 60 s of Live, kept alive by the server's pings within the 45 s liveness window.
      h.clock.advanceTo(h.clock.now() + 30_000);
      send(h, { t: "ping", nonce: `n${revoked}` });
      h.clock.advanceTo(h.clock.now() + 30_000);
      expect(status(root)).toBe("Live");
      h.step(step({ step: "token_refused", code: 4403 }));
      await tick();
      expect(tokens.calls).toBe(revoked + 1);
    }
  });

  test("Connect during a backoff wait asks at once and cancels the wait", async () => {
    const tokens = provider(mode, "t1", "t2", "deferred", "t4");
    const { h, root } = mount(tokens);
    await tick();
    welcome(h, 0);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    welcome(h, 0);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    // The second re-mint waits for the backoff; Connect does not.
    expect([tokens.calls, status(root)]).toEqual([2, "Signing in…"]);
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect([tokens.calls, status(root)]).toEqual([3, "Signing in…"]);
    h.clock.advanceTo(h.clock.now() + 120_000);
    await tick();
    expect(tokens.calls).toBe(3);
    tokens.resolve("t3");
    await tick();
    expect(started(h, mode)).toBe(3);
  });

  test("with expiresInMs, the token is renewed at RENEW_AT of it, through the resume path: same principal, the outbox kept", async () => {
    const tokens = provider(mode, { token: "t1", expiresInMs: 20_000 }, "t2");
    const { h, root } = mount(tokens, { outbox: [PENDING] });
    await tick();
    h.step(step({ step: "open" }));
    send(h, WELCOME);
    const start = h.clock.now();
    h.clock.advanceTo(start + 20_000 * RENEW_AT - 1);
    await tick();
    expect(tokens.calls).toBe(1);
    h.clock.advanceTo(start + 20_000 * RENEW_AT);
    await tick();
    expect(tokens.calls).toBe(2);
    expect(started(h, mode)).toBe(2);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t2"]);
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd").map((frame) => frame["cmd_id"])).toEqual(["c0"]);
    // t2 carries no expiresInMs, so nothing renews it.
    h.clock.advanceTo(h.clock.now() + 600_000);
    await tick();
    expect(tokens.calls).toBe(2);
  });

  test("a failed renewal is a dismissible notice while Live, is not retried, and the reactive path takes over at the next revocation", async () => {
    const tokens = provider(mode, { token: "t1", expiresInMs: 20_000 }, { reject: PANE }, "t3");
    const { h, root } = mount(tokens);
    await tick();
    welcome(h, 0);
    h.clock.advanceTo(h.clock.now() + 20_000 * RENEW_AT);
    await tick();
    expect(tokens.calls).toBe(2);
    expect(status(root)).toBe("Live");
    const notice = root.querySelector('[data-testid="renewal-notice"]');
    expect(notice?.textContent).toContain(PANE);
    (notice?.querySelector("button") as HTMLButtonElement).click();
    expect(root.querySelector('[data-testid="renewal-notice"]')).toBeNull();
    h.clock.advanceTo(h.clock.now() + 600_000);
    await tick();
    expect(tokens.calls).toBe(2);
    expect(frames(h, "hello").every((frame) => frame["token"] === "t1")).toBe(true);
    welcome(h, 0);
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(3);
  });

  test("a Disconnect clears the renewal: it calls nothing while disconnected, nor once the person connects again", async () => {
    for (const reconnectFirst of [false, true]) {
      const tokens = provider(mode, { token: "t1", expiresInMs: 20_000 });
      const { h, root } = mount(tokens);
      await tick();
      welcome(h, 0);
      const start = h.clock.now();
      click(root, ".connection-bar__toggle");
      if (reconnectFirst) {
        click(root, ".connection-bar__toggle");
        await tick();
        welcome(h, 0);
      }
      h.clock.advanceTo(start + 20_000 * RENEW_AT);
      await tick();
      // A bridge's new session reports its bearer's remaining lifetime, and so arms a renewal of its own.
      expect([tokens.calls, tokens.renews]).toEqual(mode === "bridge" && reconnectFirst ? [2, [false, false, true]] : [1, mode === "bridge" ? [false] : []]);
      dispose?.();
    }
  });

  test("a renewal due after the re-mint cap is spent calls nothing and keeps the cap's message", async () => {
    const tokens = provider(mode, ...Array.from({ length: MAX_REMINTS + 1 }, () => ({ token: "t", expiresInMs: 600_000 })));
    const { h, root } = mount(tokens);
    await tick();
    for (let revoked = 1; revoked <= MAX_REMINTS + 1; revoked++) {
      welcome(h, 0);
      h.step(step({ step: "token_refused", code: 4403 }));
      await tick();
      h.clock.advanceTo(h.clock.now() + 30_000);
      await tick();
    }
    expect(status(root)).toBe(`NeedsToken · The token was revoked. ${REMINTS_SPENT}`);
    h.clock.advanceTo(h.clock.now() + 600_000);
    await tick();
    expect(tokens.calls).toBe(MAX_REMINTS + 1);
    expect(status(root)).toBe(`NeedsToken · The token was revoked. ${REMINTS_SPENT}`);
  });

  for (const [name, expiresInMs] of [
    ["0", 0],
    ["a negative number", -1],
    ["NaN", Number.NaN],
    ["Infinity", Number.POSITIVE_INFINITY],
    ["a string", "5000"],
  ] as const) {
    test(`an expiresInMs of ${name} is refused: NeedsToken says why, and nothing connects`, async () => {
      const { h, root } = mount(provider(mode, { token: "t1", expiresInMs }));
      await tick();
      expect(status(root)).toBe(`NeedsToken · The shell's ${mode === "bridge" ? "bridge session" : "token"} has an invalid expiresInMs: it must be a positive, finite number of milliseconds.`);
      expect(started(h, mode)).toBe(0);
    });
  }

  test("a Disconnect while the provider is asked: its token does not reconnect, and the next Connect uses it", async () => {
    const tokens = provider(mode, "deferred");
    const { h, root } = mount(tokens);
    await tick();
    expect(tokens.calls).toBe(1);
    click(root, ".connection-bar__toggle");
    expect(status(root)).toBe("Disconnected");
    tokens.resolve("late");
    await tick();
    expect(started(h, mode)).toBe(0);
    expect(status(root)).toBe("Disconnected");
    click(root, ".connection-bar__toggle");
    h.step(step({ step: "open" }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["late"]);
    expect(tokens.calls).toBe(1);
  });

  test("a provider resolving after the app is gone opens nothing", async () => {
    const tokens = provider(mode, "deferred");
    const { h } = mount(tokens);
    await tick();
    expect(tokens.calls).toBe(1);
    dispose?.();
    dispose = undefined;
    tokens.resolve("late");
    await tick();
    expect(started(h, mode)).toBe(0);
  });

  test("Connect pressed again while the provider is asked makes no second call", async () => {
    const tokens = provider(mode, { reject: PANE }, "deferred");
    const { root } = mount(tokens);
    await tick();
    click(root, '[data-testid="shell-connect"]');
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect(tokens.calls).toBe(2);
  });

  test("an auth_revoked ack before the 4403 is one provider call", async () => {
    const tokens = provider(mode, "t1", "t2");
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
    const tokens = provider(mode, "t1");
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
}

test("without a provider, NeedsToken still prompts for a token and names flk client open", () => {
  const { h, root } = mount(undefined);
  expect(h.net.attempts).toHaveLength(0);
  expect(status(root)).toBe(`NeedsToken · ${RUN_CLIENT_OPEN}`);
  expect(prompt(root)).not.toBeNull();
  expect(root.querySelector('[data-testid="shell-connect"]')).toBeNull();
});

test("the contract: a provider is a function, and never beside a fixed token", () => {
  const scope = (config: object) => ({ __FLOCK_SHELL_CONFIG__: config });
  const tokenProvider: TokenProvider = async () => ({ token: "t" });
  expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider }))).toEqual({ ok: true, config: { transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider } });
  for (const bad of [{ tokenProvider: "shell" }, { tokenProvider, token: "t" }]) {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", ...bad })).ok).toBe(false);
  }
});

test("the contract: openBridge is a function, and never beside serverUrl, token or tokenProvider", () => {
  const scope = (config: object) => ({ __FLOCK_SHELL_CONFIG__: config });
  const openBridge: OpenBridge = async () => Promise.reject(new Error("unused"));
  const tokenProvider: TokenProvider = async () => ({ token: "t" });
  expect(readShellConfig(scope({ transport: "bridge", openBridge }))).toEqual({ ok: true, config: { transport: "bridge", openBridge } });
  for (const beside of [{ serverUrl: "ws://127.0.0.1:1" }, { token: "t" }, { tokenProvider }]) {
    expect(readShellConfig(scope({ transport: "bridge", openBridge, ...beside }))).toEqual({ ok: false, error: { message: BRIDGE_ALONE } });
  }
  expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", openBridge }))).toEqual({ ok: false, error: { message: BRIDGE_ALONE } });
  expect(readShellConfig(scope({ transport: "bridge", openBridge: "shell" }))).toEqual({ ok: false, error: { message: "A bridge configuration needs an openBridge function." } });
});

describe("the bridge transport", () => {
  test("openBridge passes renew: false at connect and on every reconnect, and renew: true only from the renewal timer", async () => {
    const shell = provider("bridge", { token: "t1", expiresInMs: 40_000 }, "t2");
    const { h, root } = mount(shell);
    await tick();
    welcome(h, 0);
    // An ordinary drop: the reconnect opens a session on the same bearer, with what is left of its lifetime.
    h.step(step({ step: "drop" }));
    expect(status(root)).toContain("Reconnecting");
    h.clock.advanceTo(h.clock.now() + 1_000);
    await tick();
    welcome(h, 0);
    expect([shell.renews, shell.calls]).toEqual([[false, false], 1]);
    // That session reported 39 s left, so the renewal is due ¾ of it after the reconnect.
    const reconnected = h.clock.now();
    h.clock.advanceTo(reconnected + Math.floor((40_000 - 1_000) * RENEW_AT) - 1);
    await tick();
    expect(shell.renews).toEqual([false, false]);
    h.clock.advanceTo(reconnected + Math.floor((40_000 - 1_000) * RENEW_AT));
    await tick();
    expect([shell.renews, shell.calls]).toEqual([[false, false, true], 2]);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t1", "t2"]);
    expect(shell.hellos.some((frame) => "token" in frame)).toBe(false);
    expect(status(root)).toBe("Live");
  });

  for (const [end, code, welcomed] of [
    ["a 4403", 4403, true],
    ["a 4401 after a welcome", 4401, true],
    ["a 4401 before one", 4401, false],
  ] as const) {
    test(`${end} leads to one new openBridge call, which mints, within the streak`, async () => {
      const shell = provider("bridge", "t1", "t2", "t3");
      const { h, root } = mount(shell);
      await tick();
      if (welcomed) welcome(h, 0);
      h.step(step({ step: "token_refused", code }));
      await tick();
      expect([shell.renews, shell.calls]).toEqual([[false, false], 2]);
      // The second end in a row waits for the backoff: the streak counts it.
      if (welcomed) welcome(h, 0);
      h.step(step({ step: "token_refused", code }));
      await tick();
      expect([shell.calls, status(root)]).toEqual([2, "Signing in…"]);
      h.clock.advanceTo(h.clock.now() + 250);
      await tick();
      expect(shell.calls).toBe(3);
      expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t2", "t3"]);
    });
  }

  test("a 1006 is an ordinary reconnect: Reconnecting on §3's backoff, then one openBridge that mints nothing, and the streak is untouched", async () => {
    const shell = provider("bridge", "t1");
    const { h, root } = mount(shell);
    await tick();
    welcome(h, 0);
    h.step(step({ step: "drop" }));
    expect(status(root)).toBe("Reconnecting · Connection lost: the socket closed (1006).");
    // Attempt 0's backoff is [250, 500] ms; random 0 takes 250.
    h.clock.advanceTo(h.clock.now() + 249);
    await tick();
    expect(shell.renews).toEqual([false]);
    h.clock.advanceTo(h.clock.now() + 1);
    await tick();
    expect([shell.renews, shell.calls, started(h, "bridge")]).toEqual([[false, false], 1, 2]);
    welcome(h, 0);
    expect(status(root)).toBe("Live");
  });

  test("a 1008 is a protocol close: the diagnosis is shown and the connection goes to Reconnecting, never NeedsToken", async () => {
    const shell = provider("bridge", "t1");
    const { h, root } = mount(shell);
    await tick();
    h.step(step({ step: "close", code: 1008, reason: "the first frame was not a token-free hello" }));
    expect(status(root)).toBe("Reconnecting · Connection lost: the socket closed (1008: the first frame was not a token-free hello).");
    h.clock.advanceTo(h.clock.now() + 250);
    await tick();
    expect([shell.renews, shell.calls, started(h, "bridge")]).toEqual([[false, false], 1, 2]);
  });

  test("a refusal on a reconnect is NeedsToken with the shell's message, is not retried, and Connect asks again", async () => {
    const shell = provider("bridge", "t1", { reject: PANE }, "t3");
    const { h, root } = mount(shell);
    await tick();
    welcome(h, 0);
    // The server ended t1, so the reconnect's open must mint, and the shell refuses.
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(status(root)).toBe(`NeedsToken · The token was revoked. ${PANE}`);
    h.clock.advanceTo(h.clock.now() + 600_000);
    await tick();
    expect([shell.renews, shell.calls]).toEqual([[false, false], 2]);
    click(root, '[data-testid="shell-connect"]');
    await tick();
    expect([shell.renews, shell.calls]).toEqual([[false, false, false], 3]);
    welcome(h, 0);
    expect(status(root)).toBe("Live");
  });

  test("a session held after a Disconnect that closes before Connect is dropped: Connect opens another on the same bearer", async () => {
    const shell = provider("bridge", "deferred");
    const { h, root } = mount(shell);
    await tick();
    click(root, ".connection-bar__toggle");
    shell.resolve("late");
    await tick();
    // The native side's socket to the server went away while nobody used it.
    h.net.current().drop();
    click(root, ".connection-bar__toggle");
    expect(status(root)).toBe("Connecting");
    await tick();
    expect([shell.renews, shell.calls]).toEqual([[false, false], 1]);
    welcome(h, 0);
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["late"]);
    expect(status(root)).toBe("Live");
  });

  test("a session that resolves after the app is gone is closed, and one with an invalid lifetime is closed too", async () => {
    const late = provider("bridge", "deferred");
    const first = mount(late);
    await tick();
    dispose?.();
    dispose = undefined;
    late.resolve("late");
    await tick();
    expect(first.h.net.closes.map((close) => close.code)).toEqual([1000]);
    const invalid = mount(provider("bridge", { token: "t1", expiresInMs: 0 }));
    await tick();
    expect(invalid.h.net.closes.map((close) => close.code)).toEqual([1000]);
    expect(frames(invalid.h, "hello")).toEqual([]);
  });
});

for (const mode of MODES) {
  describe(`the ${mode} driver on a bare connection`, () => {
  /** A Live connection (given → hello → welcome), a driver on it, and every delay it armed. */
  function driven(...replies: Reply[]) {
    const h = new Harness();
    const tokens = provider(mode, ...replies);
    if (mode === "provider") live(h);
    else {
      // A session on the native side's first bearer, then hello → welcome: Live before the driver is attached.
      h.conn = new WireConnection({ bridged: true, client: { name: "fixture", version: "1" }, clock: h.clock, random: () => 0, store: h.store });
      tokens.net = h.net;
      h.connection.setSocket(tokens.open(FIXTURE_TOKEN));
      h.connection.connect();
      send(h, WELCOME);
    }
    // The provider's `given` replaced the harness's network.
    tokens.net = h.net;
    const delays: number[] = [];
    // Like a browser, a delay above 2^31 - 1 ms fires at once.
    const browserDelay = (ms: number): number => (ms > 2 ** 31 - 1 ? 0 : ms);
    const clock = { ...h.clock, now: () => h.clock.now(), wallNow: () => h.clock.wallNow(), clearTimeout: (handle: Parameters<typeof h.clock.clearTimeout>[0]) => h.clock.clearTimeout(handle), setTimeout: (callback: () => void, ms: number) => (delays.push(ms), h.clock.setTimeout(callback, browserDelay(ms))) };
    const config = tokens.config;
    if ("openBridge" in config) driveBridge(h.connection, config.openBridge, clock, () => 0, () => undefined);
    else driveTokenProvider(h.connection, config.tokenProvider, clock, () => 0, () => undefined);
    return { h, tokens, delays };
  }

  test("a renewal timer is armed only for a token with expiresInMs, at RENEW_AT of it", async () => {
    const plain = driven("t2");
    plain.h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect([plain.tokens.calls, plain.delays]).toEqual([1, []]);
    const expiring = driven({ token: "t2", expiresInMs: 40_000 });
    expiring.h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(expiring.delays).toEqual([40_000 * RENEW_AT]);
  });

  for (const expiresInMs of [2 ** 32, Number.MAX_SAFE_INTEGER]) {
    test(`a lifetime past the longest timer (${expiresInMs} ms) never renews early and never loops`, async () => {
      const { h, tokens, delays } = driven({ token: "t2", expiresInMs }, "t3");
      h.step(step({ step: "token_refused", code: 4403 }));
      await tick();
      h.step(step({ step: "open" }));
      send(h, WELCOME);
      const due = h.clock.now() + Math.floor(expiresInMs * RENEW_AT);
      // Ten of the longest timers: each re-arms, none renews.
      for (let step = 0; step < 10; step++) {
        h.clock.advanceTo(Math.min(due - 1, h.clock.now() + MAX_TIMER_DELAY));
        await tick();
      }
      expect(tokens.calls).toBe(1);
      expect(delays.every((ms) => ms <= MAX_TIMER_DELAY)).toBe(true);
      if (mode === "bridge") {
        // Nothing keeps this socket alive, so the bridge reconnects: each session on the same bearer arms the renewal at ¾ of what is left, never before `due`.
        const latest = tokens.lifetimes.at(-1);
        expect(tokens.lifetimes.length).toBeGreaterThan(1);
        expect(latest === undefined ? null : latest.at + Math.floor(latest.expiresInMs * RENEW_AT) >= due).toBe(true);
      } else if (due - h.clock.now() <= 20 * MAX_TIMER_DELAY) {
        h.clock.advanceTo(due);
        await tick();
        expect(tokens.calls).toBe(2);
      }
    });
  }

  test("acks are counted monotonically: an ack after a principal change still ends the streak", async () => {
    const { h, tokens } = driven("t2", "t3");
    const ack = (cmd: string): void => send(h, { t: "ack", cmd_id: cmd, ok: true, result: null });
    ack(h.connection.command("agent.send", { target: "a", text: "one" }));
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(1);
    // The new token is another principal: the acked command leaves the outbox.
    h.step(step({ step: "open" }));
    send(h, { ...WELCOME, principal: "another-principal" });
    expect(h.connection.commands()).toEqual([]);
    ack(h.connection.command("agent.send", { target: "a", text: "two" }));
    h.step(step({ step: "token_refused", code: 4403 }));
    await tick();
    expect(tokens.calls).toBe(2);
  });
});
}

test("a principal change's drop notice clears at the next acked command", () => {
  const h = new Harness();
  live(h, { outbox: [{ ...PENDING, principal: "old-principal" }] });
  expect(h.connection.outboxDropped).toContain("Dropped 1 unsent command");
  const cmd = h.connection.command("agent.send", { target: "a", text: "again" });
  expect(h.connection.outboxDropped).not.toBeNull();
  send(h, { t: "ack", cmd_id: cmd, ok: true, result: null });
  expect(h.connection.outboxDropped).toBeNull();
});
