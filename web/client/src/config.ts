/**
 * Shell configuration: the contract between a shell and this bundle
 * (ADR-0038 §6).
 *
 * The bundle never issues a relative `fetch`, and reads the page's location
 * in exactly one place, {@link sameOriginDefaults}. A shell that hosts it
 * may set one global, {@link SHELL_CONFIG_GLOBAL}, before the bundle's
 * module script runs, and a present global always wins:
 *
 * - **Browser shell** (`flk` with the `web` feature): `flk` serves no config
 *   script, so the global is absent and the bundle takes same-origin
 *   defaults: the server is the page's origin, `ws:` for `http:` and `wss:`
 *   for `https:` (main's decision on #1880). The served page enforces
 *   `script-src 'self'` with no inline script or eval (#1875), so nothing
 *   here relies on either. The token comes from the one-time code exchange
 *   (`src/wire/token.ts`).
 * - **Tauri v2 shell** (#1601): an initialization script assigns it before
 *   the page loads, with the token read from the keychain.
 * - **Fixture build** (`vite build --mode fixture`, `just client-preview`):
 *   `src/fixture-shell.ts` runs first and assigns a fixture configuration
 *   with the recorded session bundled in, so the page needs no server.
 *
 * A malformed configuration, or no global on a page not served over
 * `http:`/`https:`, is an explicit {@link ConfigError}, which the app
 * renders as an error state.
 *
 * @module
 */

/** Name of the global the shell assigns before the bundle runs. */
export const SHELL_CONFIG_GLOBAL = "__FLOCK_SHELL_CONFIG__";

/** Live mode: connect to a `flk` server over ADR-0036 §5's WebSocket. */
export type WireShellConfig = {
  readonly transport: "wire";
  /** Absolute `ws:`, `wss:`, `http:` or `https:` URL of the server. */
  readonly serverUrl: string;
  /**
   * Client token (ADR-0036 §5), when the shell has one (Tauri, from the
   * keychain). The browser shell has none: its token comes from the code
   * exchange and is kept in `sessionStorage` (ADR-0038 §3).
   */
  readonly token?: string;
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
export type ShellConfig = WireShellConfig | FixtureShellConfig;

/** Why the injected configuration was refused. */
export type ConfigError = { readonly message: string };

/** The outcome of {@link readShellConfig}: a config or an explicit error. */
export type ConfigResult =
  | { readonly ok: true; readonly config: ShellConfig }
  | { readonly ok: false; readonly error: ConfigError };

const ABSOLUTE_SCHEMES = new Set(["ws:", "wss:", "http:", "https:"]);

function refuse(message: string): ConfigResult {
  return { ok: false, error: { message } };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The browser shell's configuration when no global is set: the server is
 * the page's own origin (main's decision on #1880: `flk` serves no config
 * script, so the page it serves configures itself). This is the only place
 * the bundle reads the page's location; `tests/config.test.ts` allows it
 * here and nowhere else.
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
      const { serverUrl, token } = raw;
      if (typeof serverUrl !== "string" || !URL.canParse(serverUrl)) {
        return refuse("serverUrl must be an absolute URL.");
      }
      if (!ABSOLUTE_SCHEMES.has(new URL(serverUrl).protocol)) {
        return refuse("serverUrl must use ws:, wss:, http: or https:.");
      }
      if (token !== undefined && (typeof token !== "string" || token === "")) {
        return refuse("token, when given, must be a non-empty string.");
      }
      return { ok: true, config: { transport: "wire", serverUrl, ...(token === undefined ? {} : { token }) } };
    }
    case "fixture": {
      const { fixtureJsonl, source } = raw;
      if (typeof fixtureJsonl !== "string" || typeof source !== "string") {
        return refuse("A fixture configuration needs fixtureJsonl and source strings.");
      }
      return { ok: true, config: { transport: "fixture", fixtureJsonl, source } };
    }
    default:
      return refuse(`Unknown transport ${JSON.stringify(raw["transport"]) ?? "undefined"}.`);
  }
}
