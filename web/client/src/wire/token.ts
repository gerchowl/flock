/**
 * The browser shell's token (ADR-0038 §3, "Getting and keeping the token";
 * ADR-0036 §5; S7-min slice 7b, #1746).
 *
 * - `flk client open` opens the page with a one-time code in the fragment
 *   (`#code=…`). {@link takeCode} reads it and removes it at once with
 *   `history.replaceState`, keeping the path, the query and the rest of the
 *   fragment. `src/capture-code.ts` runs it before any other module.
 * - {@link exchangeCode} trades the code for a token, exactly once:
 *   `POST /v1/client/exchange` with `{"code"}` returns `{"token"}`. Every
 *   failure (a non-200 reply, a malformed body, a network error) is the
 *   same one, {@link EXCHANGE_FAILED}, and the code is never retried.
 * - {@link createTokenStore} keeps the token in `sessionStorage` under
 *   {@link TOKEN_KEY}: it survives a reload, not closing the tab. Every
 *   access is guarded, and a storage that throws or is missing falls back
 *   to memory without saying anything alarming.
 *
 * The token goes only into `hello`. It is never part of a URL, a log, an
 * error message or an outbox record.
 *
 * @module
 */

/** The `sessionStorage` key of the token. */
export const TOKEN_KEY = "flock.token";

/** The one message for a failed exchange (ADR-0038 §3). */
export const EXCHANGE_FAILED = "This link has expired or was already used. Run `flk client open` again.";

/** What NeedsToken asks for when no token is left (ADR-0038 §3). */
export const RUN_CLIENT_OPEN = "Run `flk client open` to connect.";

/** The part of `Storage` the token store uses. */
export type SessionLike = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** Reads, replaces and clears the token. */
export type TokenStore = {
  readonly get: () => string | null;
  readonly set: (token: string) => void;
  readonly clear: () => void;
};

/**
 * A token store over `session()`, normally `() => sessionStorage`. Reading
 * the accessor itself may throw (a sandboxed frame, storage disabled), so it
 * is called inside the guard on every access. Whatever storage refuses is
 * kept in memory instead, for this page's life.
 */
export function createTokenStore(session: () => SessionLike | undefined): TokenStore {
  let memory: string | null = null;
  const attempt = <T,>(use: (storage: SessionLike) => T): { ok: true; value: T } | { ok: false } => {
    try {
      const storage = session();
      return storage === undefined ? { ok: false } : { ok: true, value: use(storage) };
    } catch {
      return { ok: false };
    }
  };
  return {
    get: () => {
      const stored = attempt((storage) => storage.getItem(TOKEN_KEY));
      return stored.ok && stored.value !== null && stored.value !== "" ? stored.value : memory;
    },
    set: (token) => {
      memory = token;
      attempt((storage) => storage.setItem(TOKEN_KEY, token));
    },
    clear: () => {
      memory = null;
      attempt((storage) => storage.removeItem(TOKEN_KEY));
    },
  };
}

/** The parts of `location` {@link takeCode} reads. */
export type CodeLocation = { readonly hash: string; readonly pathname: string; readonly search: string };

/** The part of `history` {@link takeCode} uses. */
export type CodeHistory = { readonly state: unknown; replaceState(state: unknown, unused: string, url: string): void };

/**
 * The one-time code in the fragment, or null. When there is one, the
 * fragment loses its `code` parameter at once (`history.replaceState`, so
 * no history entry is added); the path, the query and any other fragment
 * parameters stay.
 */
export function takeCode(location: CodeLocation, history: CodeHistory): string | null {
  const params = new URLSearchParams(location.hash.replace(/^#/, ""));
  if (!params.has("code")) return null;
  const code = params.get("code");
  params.delete("code");
  const rest = params.toString();
  history.replaceState(history.state, "", `${location.pathname}${location.search}${rest === "" ? "" : `#${rest}`}`);
  return code === "" ? null : code;
}

/** The request {@link exchangeCode} makes; normally `fetch`. */
export type ExchangeRequest = (
  url: string,
  init: { readonly method: "POST"; readonly headers: Readonly<Record<string, string>>; readonly body: string },
) => Promise<{ readonly status: number; json(): Promise<unknown> }>;

/**
 * The exchange endpoint of the server at `serverUrl`. In the browser shell
 * that server is the one serving the page, so the request is same-origin,
 * and the URL is built from the shell's absolute `serverUrl`, never from
 * the page's location (`src/config.ts`).
 */
export function exchangeUrl(serverUrl: string): string {
  const url = new URL("/v1/client/exchange", serverUrl);
  if (url.protocol === "ws:") url.protocol = "http:";
  if (url.protocol === "wss:") url.protocol = "https:";
  return url.href;
}

/**
 * Trades `code` for a token, once: the token, or null for any failure.
 * The request carries no credentials beyond the defaults of a same-origin
 * request, and nothing about the reply or the error is kept.
 */
export async function exchangeCode(code: string, serverUrl: string, request: ExchangeRequest): Promise<string | null> {
  try {
    const reply = await request(exchangeUrl(serverUrl), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code }),
    });
    if (reply.status !== 200) return null;
    const body: unknown = await reply.json();
    if (typeof body !== "object" || body === null || Array.isArray(body)) return null;
    const token: unknown = (body as Record<string, unknown>)["token"];
    return typeof token === "string" && token !== "" ? token : null;
  } catch {
    return null;
  }
}
