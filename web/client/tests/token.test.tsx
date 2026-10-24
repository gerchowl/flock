/**
 * The browser shell's token (ADR-0038 §3, "Getting and keeping the token";
 * S7-min slice 7b, #1746): the fragment code, its one exchange, the
 * `sessionStorage` token and NeedsToken. The exchange runs on an injected
 * request, so no network is touched; all time is virtual (ADR-0037).
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import { readShellConfig, type ConfigResult } from "../src/config";
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
/** Well-formed one-time codes: lowercase hex of 32 random bytes. */
const CODE = "a".repeat(64);
const PASTED = "b".repeat(64);
const USED = "c".repeat(64);
const FRESH = "d".repeat(64);
const sentCode = (code: string): string => JSON.stringify({ code });
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

function mountApp(options: { code?: string | null; session?: () => SessionLike | undefined; request?: ExchangeRequest; page?: ReturnType<typeof page>; config?: ConfigResult }): { h: Harness; root: HTMLElement } {
  const h = new Harness();
  h.step(step({ step: "given", cursors: {}, outbox: [] }));
  const root = document.createElement("div");
  document.body.append(root);
  const env = { clock: h.clock, sockets: h.net.factory, random: () => 0, store: h.store, session: options.session ?? (() => undefined), request: options.request ?? exchange({ status: 500 }), page: options.page ?? page("") };
  dispose = render(() => <App config={options.config ?? config} env={env} code={options.code ?? null} />, root);
  return { h, root };
}

const status = (root: HTMLElement): string => root.querySelector(".topbar__status")?.textContent ?? "";
const hello = (h: Harness): { token?: string } | undefined => sentFrames(h).find((frame) => (frame as { t: string }).t === "hello") as { token?: string } | undefined;

describe("the fragment code", () => {
  test("is taken out of the fragment, keeping the path, the query and the other parameters", () => {
    const calls: unknown[][] = [];
    const code = takeCode({ hash: `#view=inbox&code=${CODE}`, pathname: "/app/", search: "?layout=phone" }, { state: { kept: 1 }, replaceState: (...args: unknown[]) => void calls.push(args) });
    expect(code).toBe(CODE);
    expect(calls).toEqual([[{ kept: 1 }, "", "/app/?layout=phone#view=inbox"]]);
  });

  test("leaves a fragment without a code alone", () => {
    const calls: unknown[][] = [];
    expect(takeCode({ hash: "#view=inbox", pathname: "/", search: "" }, { state: null, replaceState: (...args: unknown[]) => void calls.push(args) })).toBeNull();
    expect(calls).toEqual([]);
  });

  test("is already gone from the fragment when the exchange request is issued", async () => {
    const at = page(`#code=${CODE}`);
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
    at.paste(`#view&code=${PASTED}`);
    await tick();
    at.paste("#view");
    await tick();
    expect(request.calls.map((call) => call.body)).toEqual([sentCode(PASTED)]);
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

describe("codes", () => {
  test("every code= part leaves the fragment, and the first well-formed one is used", () => {
    const urls: string[] = [];
    const replace = { state: null, replaceState: (_state: unknown, _unused: string, url: string) => void urls.push(url) };
    expect(takeCode({ hash: `#code=nothex&x&code=${PASTED}&code=${CODE}`, pathname: "/", search: "" }, replace)).toBe(PASTED);
    expect(takeCode({ hash: "#code=a&x&code=b", pathname: "/", search: "" }, replace)).toBe("a");
    expect(urls).toEqual(["/#x", "/#x"]);
  });

  for (const [name, code] of [
    ["too short", "abc"],
    ["upper case", "A".repeat(64)],
    ["not hex", "g".repeat(64)],
    ["too long", "a".repeat(65)],
  ] as const) {
    test(`a malformed code (${name}) is refused with the one message and no request`, async () => {
      const request = exchange({ status: 200, body: { token: "tok-1" } });
      const { h, root } = mountApp({ code, request });
      await tick();
      expect(request.calls).toHaveLength(0);
      expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
      expect(h.net.attempts).toHaveLength(0);
    });
  }

  test("a pasted malformed code is stripped from the fragment, and nothing is requested", async () => {
    const at = page("");
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    mountApp({ page: at, request });
    at.paste("#view&code=zz");
    await tick();
    expect(at.location.hash).toBe("#view");
    expect(request.calls).toHaveLength(0);
  });

  test("when two pasted codes race, only the latest one's result counts", async () => {
    const storage = session();
    const at = page("");
    const replies: ((token: string) => void)[] = [];
    const request: ExchangeRequest = () =>
      new Promise((resolve) => {
        replies.push((token) => resolve({ status: 200, json: async () => ({ token }) }));
      });
    const { h, root } = mountApp({ session: () => storage, page: at, request });
    at.paste(`#code=${CODE}`);
    at.paste(`#code=${PASTED}`);
    expect(replies).toHaveLength(2);
    replies[1]?.("second");
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("second");
    expect(status(root)).not.toBe("Signing in…");
    replies[0]?.("first");
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("second");
    expect(h.net.attempts).toHaveLength(1);
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("second");
  });
});

describe("pasted codes during an exchange", () => {
  /** An exchange request whose replies the test settles by hand, in any order. */
  function held() {
    const replies: ((reply: { status: number; token?: string }) => void)[] = [];
    const request: ExchangeRequest = () =>
      new Promise((resolve) => {
        replies.push((reply) => resolve({ status: reply.status, json: async () => ({ token: reply.token }) }));
      });
    return { replies, request };
  }

  test("a malformed code pasted during a good exchange does not discard it", async () => {
    const storage = session();
    const at = page("");
    const { replies, request } = held();
    const { h, root } = mountApp({ session: () => storage, page: at, request });
    at.paste(`#code=${CODE}`);
    at.paste("#code=zz");
    expect(at.location.hash).toBe("");
    expect(replies).toHaveLength(1);
    expect(status(root)).toBe("Signing in…");
    replies[0]?.({ status: 200, token: "good" });
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("good");
    expect(root.querySelector('[data-testid="exchange-notice"]')).toBeNull();
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("good");
  });

  test("a malformed code pasted when nothing is in flight is a failure, with no request", async () => {
    const at = page("");
    const request = exchange({ status: 200, body: { token: "tok" } });
    const { root } = mountApp({ page: at, request });
    at.paste("#code=zz");
    expect(request.calls).toHaveLength(0);
    expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
  });

  test("a malformed code in the loaded page is a failure, not an endless sign-in", () => {
    const { root } = mountApp({ code: "zz" });
    expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
  });

  test("the last paste wins even when the older exchange settles last; the status stays Signing in until then", async () => {
    const storage = session();
    const at = page("");
    const { replies, request } = held();
    const { h, root } = mountApp({ session: () => storage, page: at, request });
    at.paste(`#code=${CODE}`);
    at.paste(`#code=${PASTED}`);
    replies[0]?.({ status: 200, token: "first" });
    await tick();
    expect(storage.entries.has(TOKEN_KEY)).toBe(false);
    expect(status(root)).toBe("Signing in…");
    expect(h.net.attempts).toHaveLength(0);
    replies[1]?.({ status: 200, token: "second" });
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("second");
    expect(h.net.attempts).toHaveLength(1);
  });

  test("a failed older exchange settling after the latest began cannot end the latest's sign-in", async () => {
    const at = page("");
    const { replies, request } = held();
    const { root } = mountApp({ page: at, request });
    at.paste(`#code=${CODE}`);
    at.paste(`#code=${PASTED}`);
    replies[0]?.({ status: 401 });
    await tick();
    expect(status(root)).toBe("Signing in…");
    expect(root.querySelector('[data-testid="exchange-notice"]')).toBeNull();
  });
});

describe("no config script: same-origin defaults", () => {
  test("the app mounts, exchanges at the page's origin and connects to its /v1/ws", async () => {
    const shell = readShellConfig({ location: { origin: "http://flk.invalid:7777" } });
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    const { h } = mountApp({ config: shell, code: CODE, request });
    await tick();
    expect(request.calls.map((call) => call.url)).toEqual(["http://flk.invalid:7777/v1/client/exchange"]);
    expect(h.net.attempts.map((attempt) => attempt.url)).toEqual(["ws://flk.invalid:7777/v1/ws"]);
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("tok-1");
  });
});

describe("the exchange", () => {
  test("a successful exchange stores the token, and hello carries it", async () => {
    const storage = session();
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    const { h } = mountApp({ code: CODE, session: () => storage, request });
    expect(status(mountedRoot())).toBe("Signing in…");
    await tick();
    expect(request.calls).toEqual([{ url: "https://flk.invalid/v1/client/exchange", body: sentCode(CODE) }]);
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
      const { h, root } = mountApp({ code: CODE, session: () => session(), request });
      await tick();
      expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
      h.clock.advanceTo(h.clock.now() + 120_000);
      await tick();
      expect(request.calls).toHaveLength(1);
      expect(h.net.attempts).toHaveLength(0);
    });
  }

  test("the request is same-origin, refuses redirects, sends same-origin credentials only and is never cached", async () => {
    const request = exchange({ status: 200, body: { token: "tok-1" } });
    mountApp({ code: CODE, request });
    await tick();
    expect(request.inits).toEqual([
      { method: "POST", headers: { "content-type": "application/json" }, body: sentCode(CODE), mode: "same-origin", redirect: "error", credentials: "same-origin", cache: "no-store" },
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
      const { h, root } = mountApp({ code: CODE, session: () => storage, request });
      await tick();
      expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
      expect(storage.entries.has(TOKEN_KEY)).toBe(false);
      expect(request.calls).toBe(1);
      expect(h.net.attempts).toHaveLength(0);
    });
  }

  test("a new code replaces a stored token", async () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const { h } = mountApp({ code: CODE, session: () => storage, request: exchange({ status: 200, body: { token: "new" } }) });
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("new");
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("new");
  });
});

describe("a pasted link in a connected tab", () => {
  /** A tab Live on the stored token `old`, with a page a link can be pasted into. */
  function connected(reply: Reply): { h: Harness; root: HTMLElement; storage: ReturnType<typeof session>; at: ReturnType<typeof page>; request: ReturnType<typeof exchange> } {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const at = page("");
    const request = exchange(reply);
    const { h, root } = mountApp({ session: () => storage, page: at, request });
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: WELCOME }));
    const fleet = (sentFrames(h).find((frame) => (frame as { stream?: string }).stream === "fleet") as { sub_id: string }).sub_id;
    h.step(step({ step: "send", frame: { t: "gap", sub_id: fleet, from: 1, to: 0, snapshot_to: 0 } }));
    h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet, seq: 0 } }));
    expect(status(root)).toBe("Live");
    return { h, root, storage, at, request };
  }

  test("a failed code leaves the stored token and the Live connection alone, and says so in a dismissible notice", async () => {
    const { h, root, storage, at } = connected({ status: 401 });
    at.paste(`#code=${USED}`);
    expect(storage.entries.get(TOKEN_KEY)).toBe("old");
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("old");
    expect(status(root)).toBe("Live");
    expect(h.net.attempts).toHaveLength(1);
    const notice = root.querySelector('[data-testid="exchange-notice"]');
    expect(notice?.textContent).toContain(EXCHANGE_FAILED);
    (notice?.querySelector("button") as HTMLButtonElement).click();
    expect(root.querySelector('[data-testid="exchange-notice"]')).toBeNull();
  });

  test("a page loaded with a failed code keeps the tab's stored token and connects with it", async () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const { h, root } = mountApp({ code: USED, session: () => storage, request: exchange({ status: 401 }) });
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("old");
    h.step(step({ step: "open" }));
    expect(hello(h)?.token).toBe("old");
    expect(root.querySelector('[data-testid="exchange-notice"]')?.textContent).toContain(EXCHANGE_FAILED);
  });

  test("a good code replaces the token only on success, and reconnects with it", async () => {
    const { h, root, storage, at } = connected({ status: 200, body: { token: "new" } });
    at.paste(`#code=${FRESH}`);
    // While the exchange runs, the working session is untouched.
    expect(storage.entries.get(TOKEN_KEY)).toBe("old");
    expect(status(root)).toBe("Live");
    await tick();
    expect(storage.entries.get(TOKEN_KEY)).toBe("new");
    expect(h.net.attempts).toHaveLength(2);
    h.step(step({ step: "open" }));
    expect(sentFrames(h).filter((frame) => (frame as { t: string }).t === "hello").map((frame) => (frame as { token?: string }).token)).toEqual(["old", "new"]);
    expect([...storage.entries.values()]).toEqual(["new"]);
  });
});

describe("a failed link's message", () => {
  test("does not outlive its link: a later refused token asks for a new one, with no stale notice", async () => {
    const storage = session();
    storage.setItem(TOKEN_KEY, "old");
    const { h, root } = mountApp({ code: USED, session: () => storage, request: exchange({ status: 401 }) });
    await tick();
    expect(root.querySelector('[data-testid="exchange-notice"]')?.textContent).toContain(EXCHANGE_FAILED);
    h.step(step({ step: "open" }));
    h.step(step({ step: "token_refused", code: 4401 }));
    expect(status(root)).toBe(`NeedsToken · The server refused the token. ${RUN_CLIENT_OPEN}`);
    expect(root.querySelector('[data-testid="exchange-notice"]')).toBeNull();
  });

  test("still shows while the failed link is what left the tab without a token", async () => {
    const { root } = mountApp({ code: USED, request: exchange({ status: 401 }) });
    await tick();
    expect(status(root)).toBe(`NeedsToken · ${EXCHANGE_FAILED}`);
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
    const { h, root } = mountApp({ code: CODE, session: throwing, request: exchange({ status: 200, body: { token: "tok-1" } }) });
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
    const code = takeCode({ hash: `#code=${CODE}`, pathname: "/", search: "" }, history);
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

test("a pasted link leaves neither its code nor the token in the page, the notices, the URL or a log", async () => {
  const sentinel = "paste-sentinel-token";
  const logged: string[] = [];
  const methods = ["log", "info", "warn", "error", "debug"] as const;
  const originals = methods.map((name) => console[name]);
  methods.forEach((name) => {
    console[name] = (...args: unknown[]) => void logged.push(args.map(String).join(" "));
  });
  try {
    const storage = session();
    const at = page("");
    const replies = [{ status: 200, body: { token: sentinel } }, { status: 401 }];
    const request: ExchangeRequest = async () => {
      const reply = replies.shift() ?? { status: 500 };
      return { status: reply.status, json: async () => ("body" in reply ? reply.body : undefined) };
    };
    const { h, root } = mountApp({ session: () => storage, page: at, request });
    const seen = (): string[] => [root.innerHTML, document.title, at.location.hash, at.location.pathname, at.location.search, ...logged, ...Object.keys(localStorage).map((key) => localStorage.getItem(key) ?? "")];
    at.paste(`#view&code=${CODE}`);
    await tick();
    expect(at.location.hash).toBe("#view");
    expect(seen().filter((text) => text.includes(sentinel) || text.includes(CODE))).toEqual([]);
    h.step(step({ step: "open" }));
    // The failure notice: a used code, then a malformed one.
    at.paste(`#code=${USED}`);
    await tick();
    expect(root.querySelector('[data-testid="exchange-notice"]')?.textContent).toBe(`${EXCHANGE_FAILED}Dismiss`);
    at.paste("#code=not-a-code");
    await tick();
    expect(seen().filter((text) => text.includes(USED) || text.includes("not-a-code") || text.includes(sentinel))).toEqual([]);
  } finally {
    methods.forEach((name, i) => {
      console[name] = originals[i] as (typeof console)[typeof name];
    });
  }
});

/** The root the last {@link mountApp} rendered into. */
function mountedRoot(): HTMLElement {
  return document.body.lastElementChild as HTMLElement;
}
