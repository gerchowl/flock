/**
 * A shell's token provider (ADR-0038 §6, the Tauri shell's zero-touch mint):
 * asked at connect and once after a 4403, kept in memory only, and never a
 * token prompt. All time and sockets are virtual (ADR-0037).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import { readShellConfig, type TokenProvider } from "../src/config";
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

/** A provider that answers `replies` in order and counts its calls. */
function provider(...replies: (string | { reject: unknown })[]): TokenProvider & { calls: number } {
  const ask = Object.assign(
    async (): Promise<string> => {
      const reply = replies[ask.calls++];
      if (reply === undefined) throw new Error("an unscripted provider call");
      if (typeof reply === "string") return reply;
      throw reply.reject;
    },
    { calls: 0 },
  );
  return ask;
}

function mount(tokenProvider: TokenProvider | undefined, outbox: readonly (typeof PENDING)[] = []) {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [...outbox] }));
  const entries = new Map<string, string>();
  const session: SessionLike = { getItem: (key) => entries.get(key) ?? null, setItem: (key, value) => void entries.set(key, value), removeItem: (key) => void entries.delete(key) };
  const root = document.createElement("div");
  document.body.append(root);
  const config = { ok: true, config: { transport: "wire", serverUrl: "https://flk.invalid", ...(tokenProvider === undefined ? {} : { tokenProvider }) } } as const;
  const env = { clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store, session: () => session, request: async () => ({ status: 500, json: async () => null }), page: { location: { hash: "", pathname: "/", search: "" }, history: { state: null, replaceState: () => undefined }, addEventListener: () => undefined, removeEventListener: () => undefined } };
  dispose = render(() => <App config={config} env={env} code={null} />, root);
  return { h, root, entries };
}

const status = (root: HTMLElement): string => root.querySelector(".topbar__status")?.textContent ?? "";
const prompt = (root: HTMLElement): Element | null => root.querySelector('input[aria-label="New client token"]');
const frames = (h: Harness, t: string): Record<string, unknown>[] => sentFrames(h).filter((frame) => (frame as { t: string }).t === t) as Record<string, unknown>[];

/** Opens the current socket, welcomes it and syncs the fleet stream at `seq`. */
function welcome(h: Harness, seq: number): void {
  h.step(step({ step: "open" }));
  h.step(step({ step: "send", frame: WELCOME }));
  const fleet = frames(h, "sub").filter((frame) => frame["stream"] === "fleet").at(-1)?.["sub_id"] as string;
  h.step(step({ step: "send", frame: { t: "gap", sub_id: fleet, from: 1, to: seq, snapshot_to: seq } }));
  h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet, seq } }));
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

  test("a 4403 is one provider call and one reconnect through the resume path, keeping cursors and the outbox", async () => {
    const tokens = provider("t1", "t2");
    const { h, root } = mount(tokens, [PENDING]);
    await tick();
    // The first socket applies the fleet up to 7 and is revoked before it is Live, so c0 is still queued.
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const first = frames(h, "sub").find((frame) => frame["stream"] === "fleet")?.["sub_id"] as string;
    h.step(step({ step: "send", frame: { t: "gap", sub_id: first, from: 1, to: 7, snapshot_to: 7 } }));
    h.step(step({ step: "token_refused", code: 4403 }));
    expect(prompt(root)).toBeNull();
    await tick();
    expect([tokens.calls, h.net.attempts.length]).toEqual([2, 2]);
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    expect(frames(h, "hello").map((frame) => frame["token"])).toEqual(["t1", "t2"]);
    const fleet = frames(h, "sub").filter((frame) => frame["stream"] === "fleet");
    expect(fleet.map((frame) => frame["after"])).toEqual([undefined, 7]);
    h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet[1]?.["sub_id"] as string, seq: 7 } }));
    expect(status(root)).toBe("Live");
    expect(frames(h, "cmd").map((frame) => frame["cmd_id"])).toEqual(["c0"]);
    // Later reconnects (the silent server's liveness) reuse t2: a token is asked for only after a 4403.
    h.clock.advanceTo(h.clock.now() + 120_000);
    await tick();
    expect(tokens.calls).toBe(2);
    expect(new Set(frames(h, "hello").slice(1).map((frame) => frame["token"]))).toEqual(new Set(["t2"]));
  });

  for (const [name, reject] of [
    ["a rejected string", "The flk server refused the mint: this app was started from inside a flk pane."],
    ["a rejected Error", new Error("The flk server refused the mint: this app was started from inside a flk pane.")],
  ] as const) {
    test(`a provider failure (${name}) shows its message in NeedsToken, and is not retried`, async () => {
      const tokens = provider({ reject });
      const { h, root } = mount(tokens);
      await tick();
      expect(status(root)).toBe("NeedsToken · The flk server refused the mint: this app was started from inside a flk pane.");
      expect(prompt(root)).toBeNull();
      h.clock.advanceTo(h.clock.now() + 120_000);
      await tick();
      expect([tokens.calls, h.net.attempts.length]).toEqual([1, 0]);
    });
  }

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
});

test("without a provider, NeedsToken still prompts for a token and names flk client open", () => {
  const { h, root } = mount(undefined);
  expect(h.net.attempts).toHaveLength(0);
  expect(status(root)).toBe(`NeedsToken · ${RUN_CLIENT_OPEN}`);
  expect(prompt(root)).not.toBeNull();
});

test("the contract: a provider is a function, and never beside a fixed token", () => {
  const scope = (config: object) => ({ __FLOCK_SHELL_CONFIG__: config });
  const tokenProvider: TokenProvider = async () => "t";
  expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider }))).toEqual({ ok: true, config: { transport: "wire", serverUrl: "ws://127.0.0.1:1", tokenProvider } });
  for (const bad of [{ tokenProvider: "shell" }, { tokenProvider, token: "t" }]) {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "ws://127.0.0.1:1", ...bad })).ok).toBe(false);
  }
});
