/**
 * Shell configuration: the contract between a shell and this bundle
 * (ADR-0038 §6).
 *
 * The bundle never issues a relative `fetch`, and reads the page's location
 * only in this module, in {@link sameOriginDefaults} and {@link pageSearch}
 * (plus the fragment capture in `src/capture-code.ts`). A shell that hosts it
 * may set one global, {@link SHELL_CONFIG_GLOBAL}, before the bundle's
 * module script runs, and a present global always wins:
 *
 * - **Browser shell** (`flk` with the `web` feature): `flk` serves no config
 *   script, so the global is absent and the bundle takes same-origin
 *   defaults: the server is the page's origin, `ws:` for `http:` and `wss:`
 *   for `https:` (main's decision on #1880). The served page enforces
 *   `script-src 'self'` with no inline script or eval (#1848), so nothing
 *   here relies on either. The token comes from the one-time code exchange
 *   (`src/wire/token.ts`).
 * - **Tauri v2 shell** (#1601): an initialization script assigns it before
 *   the page loads. On the same machine it is a {@link BridgeShellConfig}: the
 *   native side carries `/v1/ws` over the server's unix socket and keeps the
 *   bearer it mints over `flock.sock` (zero-touch, ADR-0038 §6, #2500). For a
 *   remote server it is a wire config with a {@link TokenProvider} (#2293).
 * - **Fixture build** (`vite build --mode fixture`, `just client-preview`):
 *   `src/fixture-shell.ts` runs first and assigns a fixture configuration
 *   with the recorded session bundled in, so the page needs no server.
 *
 * A malformed configuration, or no global on a page not served over
 * `http:`/`https:`, is an explicit {@link ConfigError}, which the app
 * renders as an error state. A shell that cannot build a configuration,
 * such as the Tauri shell reading a malformed config file, reports why with
 * {@link ShellReportedError}, which becomes a ConfigError with its message.
 *
 * @module
 */

/** Name of the global the shell assigns before the bundle runs. */
export const SHELL_CONFIG_GLOBAL = "__FLOCK_SHELL_CONFIG__";

/**
 * A token from the shell, and the remaining lifetime the server reported for
 * it at mint (`issued.info.expires_in_ms`), when it reported one.
 */
export type ShellToken = { readonly token: string; readonly expiresInMs?: number };

/**
 * The shell's source of tokens, pull-only: resolves to a fresh
 * {@link ShellToken}, or rejects with a message for the person. The client
 * asks it at connect, on the person's Connect, when the server ends a token
 * (close 4403, or 4401 at `hello`, bounded by §3's backoff) and, with
 * `expiresInMs`, before expiry. It keeps the token in memory only, never
 * shows the token prompt and never exchanges a link's code
 * (`src/wire/provider.ts`).
 */
export type TokenProvider = () => Promise<ShellToken>;

/** Live mode: connect to a `flk` server over ADR-0036 §5's WebSocket. */
export type WireShellConfig = {
  readonly transport: "wire";
  /** Absolute `ws:`, `wss:`, `http:` or `https:` URL of the server. */
  readonly serverUrl: string;
  /**
   * Client token (ADR-0036 §5), when the shell has a fixed one. The browser
   * shell has none: its token comes from the code exchange and is kept in
   * `sessionStorage` (ADR-0038 §3).
   */
  readonly token?: string;
  /** Where the shell's tokens come from (the Tauri shell's mint); never with `token`. */
  readonly tokenProvider?: TokenProvider;
};

/**
 * One bridged session's socket (ADR-0038 §6): the WebSocket subset the
 * client uses, carrying `/v1/ws`'s JSON text frames and close codes
 * verbatim. It is open when `openBridge` resolves it, so it has no `onopen`.
 * The client assigns the three handlers once, at once: `onmessage` receives
 * each text frame, `onclose` the close code and reason.
 */
export type BridgeSocket = {
  send(text: string): void;
  close(code?: number, reason?: string): void;
  onmessage: ((text: string) => void) | null;
  onclose: ((code: number, reason: string) => void) | null;
  onerror: (() => void) | null;
};

/**
 * A bridged session, and the remaining lifetime of the bearer the native
 * side used for it (the server's `expires_in_ms`), when it reported one.
 * `fresh: true` says the native side minted a new bearer for this open, so
 * the renewal is timed from it; absent or false, the bearer was kept.
 */
export type BridgeSession = { readonly socket: BridgeSocket; readonly expiresInMs?: number; readonly fresh?: boolean };

/**
 * The shell's source of bridged sessions, pull-only: each call is one
 * session. The client calls it at connect, on the person's Connect, on
 * every reconnect and, with `expiresInMs`, at ¾ of it, the one call with
 * `renew: true`. A rejection is a refusal (the native side could not obtain
 * a bearer), shown with its message and not retried; an unreachable server
 * resolves a socket that closes with 1006 (`src/wire/provider.ts`).
 */
export type OpenBridge = (request: { readonly renew: boolean }) => Promise<BridgeSession>;

/**
 * Bridge mode: the shell's native side carries `/v1/ws` itself, so the
 * bearer never enters the webview (#2500). Never beside `serverUrl`, `token`
 * or `tokenProvider`.
 */
export type BridgeShellConfig = {
  readonly transport: "bridge";
  readonly openBridge: OpenBridge;
};

/** Fixture mode: replay a recorded session; no server is contacted. */
export type FixtureShellConfig = {
  readonly transport: "fixture";
  /** The recorded session as JSON Lines, one adapter event per line. */
  readonly fixtureJsonl: string;
  /** Where the recording came from, shown next to the replay badge. */
  readonly source: string;
};

/** What a shell may inject. */
export type ShellConfig = WireShellConfig | BridgeShellConfig | FixtureShellConfig;

/**
 * Injected instead of a {@link ShellConfig} when the shell has none to give:
 * the client shows `message` as the reason in its ConfigError state.
 */
export type ShellReportedError = {
  readonly transport: "error";
  /** Why the shell has no configuration, for the person to act on. */
  readonly message: string;
};

/** Why the injected configuration was refused. */
export type ConfigError = { readonly message: string };

/** The outcome of {@link readShellConfig}: a config or an explicit error. */
export type ConfigResult =
  | { readonly ok: true; readonly config: ShellConfig }
  | { readonly ok: false; readonly error: ConfigError };

const ABSOLUTE_SCHEMES = new Set(["ws:", "wss:", "http:", "https:"]);

/** Why a bridge config with a server or token is refused: the bridge carries both. */
export const BRIDGE_ALONE = "openBridge carries the session and its bearer itself, so it is never beside serverUrl, token or tokenProvider.";

function refuse(message: string): ConfigResult {
  return { ok: false, error: { message } };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The browser shell's configuration when no global is set: the server is
 * the page's own origin (main's decision on #1880: `flk` serves no config
 * script, so the page it serves configures itself). With {@link pageSearch}
 * it is the only place the bundle reads the page's location;
 * `tests/location-guard.ts` allows it here and nowhere else.
 */
export function sameOriginDefaults(scope: object): ConfigResult {
  const page = scope as { readonly location?: { readonly origin?: unknown } };
  if (page.location === undefined) {
    return refuse(`No shell configuration: the shell did not set ${SHELL_CONFIG_GLOBAL}, and there is no page to take defaults from.`);
  }
  const origin = page.location.origin;
  if (typeof origin !== "string" || !URL.canParse(origin) || !["http:", "https:"].includes(new URL(origin).protocol)) {
    return refuse(`No shell configuration: the shell did not set ${SHELL_CONFIG_GLOBAL}, and the page is not served over http(s).`);
  }
  return { ok: true, config: { transport: "wire", serverUrl: origin } };
}

/**
 * The page's query string (`?preview=…`), or "" when there is no page. The
 * sibling of {@link sameOriginDefaults} for the one other thing the bundle
 * takes from the location, so no other module reads it.
 */
export function pageSearch(scope: object): string {
  const search = (scope as { readonly location?: { readonly search?: unknown } }).location?.search;
  return typeof search === "string" ? search : "";
}

/**
 * Reads and validates the shell-injected configuration from `scope`
 * (normally `globalThis`).
 *
 * The server URL must be absolute: a relative URL would resolve against the
 * page's origin, which is exactly the same-origin assumption this contract
 * forbids. Unknown `transport` values are refused rather than guessed.
 */
export function readShellConfig(scope: object): ConfigResult {
  const raw: unknown = Reflect.get(scope, SHELL_CONFIG_GLOBAL);
  if (raw === undefined) return sameOriginDefaults(scope);
  if (!isRecord(raw)) {
    return refuse(`${SHELL_CONFIG_GLOBAL} must be an object.`);
  }
  switch (raw["transport"]) {
    case "wire": {
      const { serverUrl, token, tokenProvider, openBridge } = raw;
      if (openBridge !== undefined) return refuse(BRIDGE_ALONE);
      if (typeof serverUrl !== "string" || !URL.canParse(serverUrl)) {
        return refuse("serverUrl must be an absolute URL.");
      }
      if (!ABSOLUTE_SCHEMES.has(new URL(serverUrl).protocol)) {
        return refuse("serverUrl must use ws:, wss:, http: or https:.");
      }
      if (token !== undefined && (typeof token !== "string" || token === "")) {
        return refuse("token, when given, must be a non-empty string.");
      }
      if (tokenProvider !== undefined && (typeof tokenProvider !== "function" || token !== undefined)) {
        return refuse("tokenProvider, when given, must be a function, and the only source of tokens.");
      }
      const provider = tokenProvider as TokenProvider | undefined;
      return { ok: true, config: { transport: "wire", serverUrl, ...(token === undefined ? {} : { token }), ...(provider === undefined ? {} : { tokenProvider: provider }) } };
    }
    case "bridge": {
      const { openBridge, serverUrl, token, tokenProvider } = raw;
      if (serverUrl !== undefined || token !== undefined || tokenProvider !== undefined) return refuse(BRIDGE_ALONE);
      if (typeof openBridge !== "function") return refuse("A bridge configuration needs an openBridge function.");
      return { ok: true, config: { transport: "bridge", openBridge: openBridge as OpenBridge } };
    }
    case "fixture": {
      const { fixtureJsonl, source } = raw;
      if (typeof fixtureJsonl !== "string" || typeof source !== "string") {
        return refuse("A fixture configuration needs fixtureJsonl and source strings.");
      }
      return { ok: true, config: { transport: "fixture", fixtureJsonl, source } };
    }
    case "error": {
      const { message } = raw;
      if (typeof message !== "string" || message === "") {
        return refuse("A shell-reported error needs a non-empty message string.");
      }
      return refuse(message);
    }
    default:
      return refuse(`Unknown transport ${JSON.stringify(raw["transport"]) ?? "undefined"}.`);
  }
}
