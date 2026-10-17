/**
 * The browser shell's token (ADR-0038 §3, "Getting and keeping the token";
 * S7-min slice 7b, #1746): the fragment code, its one exchange, the
 * `sessionStorage` token and NeedsToken. The exchange runs on an injected
 * request, so no network is touched; all time is virtual (ADR-0037).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import { captureCode } from "../src/capture-code";
import { EXCHANGE_FAILED, RUN_CLIENT_OPEN, TOKEN_KEY, createTokenStore, takeCode, type ExchangeRequest, type SessionLike } from "../src/wire/token";
import { Harness, WELCOME, sentFrames, step } from "./wire/harness";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
  localStorage.clear();
});

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const config = { ok: true, config: { transport: "wire", serverUrl: "https://flk.invalid" } } as const;

/** A `sessionStorage` stand-in that records what is kept. */
function session(): SessionLike & { readonly entries: Map<string, string> } {
  const entries = new Map<string, string>();
  return { entries, getItem: (key) => entries.get(key) ?? null, setItem: (key, value) => void entries.set(key, value), removeItem: (key) => void entries.delete(key) };
}

type Reply = { readonly status: number; readonly body?: unknown; readonly bodyThrows?: boolean } | "network-error";

/** An exchange request that answers `reply` and records every call; `onCall` runs as it is issued. */
function exchange(reply: Reply, log: string[] = [], onCall: () => void = () => undefined): ExchangeRequest & { readonly calls: { url: string; body: string }[]; readonly inits: unknown[] } {
  const calls: { url: string; body: string }[] = [];
  const inits: unknown[] = [];
  const request = async (url: string, init: { body: string }) => {
    onCall();
    log.push("request");
    calls.push({ url, body: init.body });
    inits.push(init);
    if (reply === "network-error") throw new TypeError("Failed to fetch");
    return {
      status: reply.status,
      json: async () => {
        if (reply.bodyThrows === true) throw new SyntaxError("Unexpected token");
        return reply.body;
      },
    };
  };
  return Object.assign(request, { calls, inits });
}

/** A page whose location, history and `hashchange` events the test drives. */
function page(hash: string) {
  const listeners = new Set<() => void>();
  const location = { hash, pathname: "/", search: "" };
  const history = {
    state: null,
    replaceState: (_state: unknown, _unused: string, url: string) => {
      const at = url.indexOf("#");
      location.hash = at < 0 ? "" : url.slice(at);
    },
  };
  return {
    location,
    history,
    addEventListener: (_type: "hashchange", listener: () => void) => void listeners.add(listener),
    removeEventListener: (_type: "hashchange", listener: () => void) => void listeners.delete(listener),
    /** Navigates to `next` the way pasting a link does: only the fragment changes. */
    paste: (next: string) => {
      location.hash = next;
      listeners.forEach((listener) => listener());
    },
  };
}

function mountApp(options: { code?: string | null; session?: () => SessionLike | undefined; request?: ExchangeRequest; page?: ReturnType<typeof page> }): { h: Harness; root: HTMLElement } {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [] }));
  const root = document.createElement("div");
  document.body.append(root);
  const env = { clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store, session: options.session ?? (() => undefined), request: options.request ?? exchange({ status: 500 }), page: options.page ?? page("") };
  dispose = render(() => <App config={config} env={env} code={options.code ?? null} />, root);
  return { h, root };
}

const status = (root: HTMLElement): string => root.querySelector(".topbar__status")?.textContent ?? "";
const hello = (h: Harness): { token?: string } | undefined => sentFrames(h).find((frame) => (frame as { t: string }).t === "hello") as { token?: string } | undefined;

describe("the fragment code", () => {
  test("is taken out of the fragment, keeping the path, the query and the other parameters", () => {
    const calls: unknown[][] = [];
    const code = takeCode({ hash: "#view=inbox&code=abc123", pathname: "/app/", search: "?layout=phone" }, { state: { kept: 1 }, replaceState: (...args: unknown[]) => void calls.push(args) });
    expect(code).toBe("abc123");
    expect(calls).toEqual([[{ kept: 1 }, "", "/app/?layout=phone#view=inbox"]]);
  });

  test("leaves a fragment without a code alone", () => {
    const calls: unknown[][] = [];
    expect(takeCode({ hash: "#view=inbox", pathname: "/", search: "" }, { state: null, replaceState: (...args: unknown[]) => void calls.push(args) })).toBeNull();
    expect(calls).toEqual([]);
  });

  test("is already gone from the fragment when the exchange request is issued", async () => {
    const at = page("#code=abc");
    const hashes: string[] = [];
    const code = captureCode(at);
    mountApp({ code, page: at, request: exchange({ status: 200, body: { token: "tok-1" } }, [], () => void hashes.push(at.location.hash)) });
    await tick();
    expect(hashes).toEqual([""]);
  });

  test("keeps the rest of the fragment byte for byte, and the code verbatim", () => {
    const urls: string[] = [];
    const replace = { state: null, replaceState: (_state: unknown, _unused: string, url: string) => void urls.push(url) };
    expect(takeCode({ hash: "#view&code=a+b/c%2Fd", pathname: "/", search: "" }, replace)).toBe("a+b/c%2Fd");
    expect(takeCode({ hash: "#code=x+y&tab=a/b+c&view", pathname: "/p", search: "?q=1" }, replace)).toBe("x+y");
    expect(takeCode({ hash: "#code=only", pathname: "/", search: "" }, replace)).toBe("only");
    expect(urls).toEqual(["/#view", "/p?q=1#tab=a/b+c&view", "/"]);
  });

  test("a pasted link (only the fragment changes) is cleared and exchanged exactly once", async () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const at = page("");
    const hashes: string[] = [];
    const request = exchange({ status: 200, body: { token: "new" } }, [], () => void hashes.push(at.location.hash));
    const { h } = mountApp({ session: () => storage, page: at, request });
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("old");
    at.paste("#view&code=pasted");
    await tick();
    at.paste("#view");
    await tick();
    expect(request.calls.map((call) => call.body)).toEqual(['{"code":"pasted"}']);
    expect(hashes).toEqual(["#view"]);
    expect(at.location.hash).toBe("#view");
    expect(storage.entries.get(TOKEN_KEY)).toBe("new");
    h.step(step({ step: "open" }));
    expect(sentFrames(h).filter((frame) => (frame as { t: string }).t === "hello").map((frame) => (frame as { token?: string }).token)).toEqual(["old", "new"]);
  });

  test("is captured by the first module the entry imports", async () => {
    const main = await Bun.file(`${import.meta.dir}/../src/main.tsx`).text();
    expect(main.match(/^import .*$/m)?.[0]).toBe('import { capturedCode } from "./capture-code";');
  });
});

describe("the exchange", () => {
  test("a successful exchange stores the token, and hello carries it", async () => {
    const storage = session();
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    const { h } = mountApp({ code: "abc", session: () => storage, request });
    expect(status(mountedRoot())).toBe("Signing in…");
    await tick();
    expect(request.calls).toEqual([{ url: "https://flk.invalid/v1/client/exchange", body: '{"code":"abc"}' }]);
    expect(storage.entries.get(TOKEN_KEY)).toBe("tok-1");
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("tok-1");
  });

  for (const [name, reply] of [
    ["a 401", { status: 401, body: { error: "expired" } }],
    ["a network error", "network-error"],
    ["a malformed body", { status: 200, body: ["tok-1"] }],
    ["a body that is not JSON", { status: 200, bodyThrows: true }],
  ] as const) {
    test(`${name} shows the one message and makes no second request`, async () => {
      const request = exchange(reply);
      const { h, root } = mountApp({ code: "abc", session: () => session(), request });
      await tick();
      expect(status(root)).toBe(EXCHANGE_FAILED);
      h.clock.advanceTo(h.clock.now() + 120_000);
      await tick();
      expect(request.calls).toHaveLength(1);
      expect(h.net.attempts).toHaveLength(0);
    });
  }

  test("the request is same-origin, refuses redirects, sends same-origin credentials only and is never cached", async () => {
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    mountApp({ code: "abc", request });
    await tick();
    expect(request.inits).toEqual([
      { method: "POST", headers: { "content-type": "application/json" }, body: '{"code":"abc"}', mode: "same-origin", redirect: "error", credentials: "same-origin", cache: "no-store" },
    ]);
  });

  /** A browser's fetch: it follows a redirect, or reaches another origin, unless the options forbid it. */
  const browser = (refuses: (init: Record<string, unknown>) => boolean): ExchangeRequest & { calls: number } => {
    const request = async (_url: string, init: Record<string, unknown>) => {
      request.calls++;
      if (refuses(init)) throw new TypeError("Failed to fetch");
      return { status: 200, json: async () => ({ token: "from-elsewhere" }) };
    };
    request.calls = 0;
    return request as unknown as ExchangeRequest & { calls: number };
  };

  for (const [name, refuses] of [
    ["a redirect", (init: Record<string, unknown>) => init["redirect"] === "error"],
    ["a cross-origin reply", (init: Record<string, unknown>) => init["mode"] === "same-origin"],
  ] as const) {
    test(`${name} is the one failure`, async () => {
      const storage = session();
      const request = browser(refuses);
      const { h, root } = mountApp({ code: "abc", session: () => storage, request });
      await tick();
      expect(status(root)).toBe(EXCHANGE_FAILED);
      expect(storage.entries.has(TOKEN_KEY)).toBe(false);
      expect(request.calls).toBe(1);
      expect(h.net.attempts).toHaveLength(0);
    });
  }

  test("a new code replaces a stored token", async () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const { h } = mountApp({ code: "abc", session: () => storage, request: exchange({ status: 200, body: { token: "new" } }) });
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("new");
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("new");
  });
});

describe("the stored token", () => {
  test("a reload with a stored token connects without a code; once the tab's storage is gone, a code is needed again", () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "kept");
    const first = mountApp({ session: () => storage });
    first.h.step(step({ step: "open" }));
    expect(hello(first.h)?.token).toBe("kept");
    dispose?.();
    storage.entries.clear();
    const second = mountApp({ session: () => storage });
    expect(second.h.net.attempts).toHaveLength(0);
    expect(status(second.root)).toBe(`NeedsToken · ${RUN_CLIENT_OPEN}`);
  });

  test("NeedsToken clears it", () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "kept");
    const { h, root } = mountApp({ session: () => storage });
    h.step(step({ step: "open" }));
    h.step(step({ step: "token_refused", code: 4401 }));
    expect(storage.entries.has(TOKEN_KEY)).toBe(false);
    expect(status(root)).toBe(`NeedsToken · The server refused the token. ${RUN_CLIENT_OPEN}`);
  });

  test("a sessionStorage that throws falls back to memory, quietly", async () => {
    const throwing = (): SessionLike => {
      throw new DOMException("The operation is insecure.", "SecurityError");
    };
    const { h, root } = mountApp({ code: "abc", session: throwing, request: exchange({ status: 200, body: { token: "tok-1" } }) });
    await tick();
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("tok-1");
    expect(status(root)).not.toMatch(/insecure|storage|Security/i);
    const store = createTokenStore(throwing);
    store.set("t");
    expect(store.get()).toBe("t");
    store.clear();
    expect(store.get()).toBeNull();
  });
});

test("the token appears only in hello: never in the page, the URL, a log, an error or a stored record", async () => {
  // A sentinel the sweep looks for; any value works, as long as it is distinctive.
  const sentinel = "leak-sentinel-value";
  const logged: string[] = [];
  const methods = ["log", "info", "warn", "error", "debug"] as const;
  const originals = methods.map((name) => console[name]);
  methods.forEach((name) => {
    console[name] = (...args: unknown[]) => void logged.push(args.map(String).join(" "));
  });
  const urls: string[] = [];
  const replace = history.replaceState.bind(history);
  const push = history.pushState.bind(history);
  history.replaceState = (state, unused, url) => (urls.push(String(url)), replace(state, unused, url));
  history.pushState = (state, unused, url) => (urls.push(String(url)), push(state, unused, url));
  try {
    const storage = session();
    const code = takeCode({ hash: "#code=abc", pathname: "/", search: "" }, history);
    const { h, root } = mountApp({ code, session: () => storage, request: exchange({ status: 200, body: { token: sentinel } }) });
    await tick();
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const seen = (): string[] => [
      root.innerHTML,
      document.title,
      ...urls,
      ...logged,
      ...[...h.store.entries.values()],
      ...Object.keys(localStorage).map((key) => `${key}=${localStorage.getItem(key)}`),
      ...h.net.attempts.map((attempt) => attempt.url),
    ];
    expect(seen().filter((text) => text.includes(sentinel))).toEqual([]);
    h.step(step({ step: "token_refused", code: 4401 }));
    expect(seen().filter((text) => text.includes(sentinel))).toEqual([]);
    expect(sentFrames(h).filter((frame) => JSON.stringify(frame).includes(sentinel)).map((frame) => (frame as { t: string }).t)).toEqual(["hello"]);
  } finally {
    methods.forEach((name, i) => {
      console[name] = originals[i] as (typeof console)[typeof name];
    });
    history.replaceState = replace;
    history.pushState = push;
  }
});

/** The root the last {@link mountApp} rendered into. */
function mountedRoot(): HTMLElement {
  return document.body.lastElementChild as HTMLElement;
}
