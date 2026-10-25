import { describe, expect, test } from "bun:test";

import { pageSearch, readShellConfig, SHELL_CONFIG_GLOBAL } from "../src/config";
import { locationReads } from "./location-guard";
import { endpointUrl } from "../src/wire/env";
import { exchangeUrl } from "../src/wire/token";

function scope(config: unknown): object {
  return { [SHELL_CONFIG_GLOBAL]: config };
}

describe("readShellConfig", () => {
  test("with no config global and no page, the error is explicit and names the global", () => {
    const result = readShellConfig({});
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.message).toContain(SHELL_CONFIG_GLOBAL);
    }
  });

  test("a relative server URL is refused: no same-origin resolution", () => {
    for (const serverUrl of ["/v1/ws", "v1/ws", "//other/v1/ws", ""]) {
      expect(readShellConfig(scope({ transport: "wire", serverUrl, token: "t" })).ok).toBe(false);
    }
  });

  test("a non-network scheme and an empty or non-string token are refused", () => {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "file:///x", token: "t" })).ok).toBe(false);
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "wss://h/v1/ws", token: "" })).ok).toBe(false);
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "wss://h/v1/ws", token: 7 })).ok).toBe(false);
  });

  test("the browser shell's config carries no token: it comes from the code exchange", () => {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "https://h:7000" }))).toEqual({
      ok: true,
      config: { transport: "wire", serverUrl: "https://h:7000" },
    });
  });

  test("an unknown transport or a non-object is refused", () => {
    expect(readShellConfig(scope({ transport: "carrier-pigeon" })).ok).toBe(false);
    expect(readShellConfig(scope("wss://h/v1/ws")).ok).toBe(false);
    expect(readShellConfig(scope(null)).ok).toBe(false);
  });

  test("valid wire and fixture configs pass through unchanged", () => {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "wss://h:7000/v1/ws", token: "t" }))).toEqual({
      ok: true,
      config: { transport: "wire", serverUrl: "wss://h:7000/v1/ws", token: "t" },
    });
    expect(readShellConfig(scope({ transport: "fixture", fixtureJsonl: "{}", source: "s" }))).toEqual({
      ok: true,
      config: { transport: "fixture", fixtureJsonl: "{}", source: "s" },
    });
  });

  test("the source never reads the page origin or fetches relatively, outside the one allowed fallback", async () => {
    const glob = new Bun.Glob("src/**/*.{ts,tsx}");
    for await (const path of glob.scan({ cwd: `${import.meta.dir}/..` })) {
      if (path.startsWith("src/gen/")) continue;
      expect({ path, reads: forbiddenReads(path, await Bun.file(`${import.meta.dir}/../${path}`).text()) }).toEqual({ path, reads: [] });
    }
  });

  test("the guard catches every spelling of a location read outside the allowed functions", () => {
    const spellings = [
      "const here = window.location.origin;",
      "const s = location.search;",
      "const p = location.protocol;",
      "const { origin } = location;",
      "const { location: here } = window;",
      "const here = document.location;",
      "const here = globalThis['location'];",
      "const here = self?.location;",
      "export const read = (scope) => scope.location.origin;",
      "export function readShellConfig() { return location.host; }",
    ];
    for (const spelling of spellings) expect({ spelling, reads: locationReads("src/App.tsx", spelling).length }).toEqual({ spelling, reads: 1 });
    expect(locationReads("src/config.ts", "export function readShellConfig() { return location.host; }")).toHaveLength(1);
    // The old regex guard saw only `location.origin|host|href` and `document.location`.
    for (const spelling of spellings.slice(1, 5)) expect(oldGuardMisses(spelling)).toBe(true);
  });

  test("the guard allows the named functions, names the module declared itself, and ignores prose and types", () => {
    const fallback = "export function sameOriginDefaults(scope) {\n  return scope.location.origin;\n}\n";
    expect(locationReads("src/config.ts", fallback)).toEqual([]);
    expect(locationReads("src/config.ts", "export function pageSearch(scope) { return scope.location.search; }")).toEqual([]);
    expect(locationReads("src/wire/token.ts", fallback)).toHaveLength(1);
    expect(locationReads("src/capture-code.ts", "export function captureCode(page) { return page.location; }")).toEqual([]);
    expect(locationReads("src/x.ts", "export function takeCode(location: L) { return location.hash; }")).toEqual([]);
    expect(locationReads("src/x.ts", "// location.origin\n/** window.location */\nexport type P = { readonly location: L };\nconst s = 'location.href';")).toEqual([]);
  });

  test("the guard still finds a relative fetch", () => {
    expect(forbiddenReads("src/x.ts", "void fetch(url);")).toEqual(["fetch"]);
  });

  test("pageSearch is the page's query string, or empty when there is no page", () => {
    expect(pageSearch({ location: { search: "?preview=phone" } })).toBe("?preview=phone");
    expect(pageSearch({ location: {} })).toBe("");
    expect(pageSearch({})).toBe("");
  });

  test("with no config global, the browser shell takes same-origin defaults", () => {
    expect(readShellConfig({ location: { origin: "http://flk.invalid:7777" } })).toEqual({ ok: true, config: { transport: "wire", serverUrl: "http://flk.invalid:7777" } });
    expect(endpointUrl("http://flk.invalid:7777")).toBe("ws://flk.invalid:7777/v1/ws");
    expect(exchangeUrl("http://flk.invalid:7777")).toBe("http://flk.invalid:7777/v1/client/exchange");
    expect(readShellConfig({ location: { origin: "https://flk.invalid" } })).toEqual({ ok: true, config: { transport: "wire", serverUrl: "https://flk.invalid" } });
    expect(endpointUrl("https://flk.invalid")).toBe("wss://flk.invalid/v1/ws");
    expect(exchangeUrl("https://flk.invalid")).toBe("https://flk.invalid/v1/client/exchange");
  });

  test("a present config global wins over the page's origin", () => {
    expect(readShellConfig({ location: { origin: "http://page.invalid" }, [SHELL_CONFIG_GLOBAL]: { transport: "wire", serverUrl: "wss://shell.invalid/v1/ws", token: "t" } })).toEqual({
      ok: true,
      config: { transport: "wire", serverUrl: "wss://shell.invalid/v1/ws", token: "t" },
    });
  });

  test("with no config global, a page not served over http(s) is refused", () => {
    const result = readShellConfig({ location: { origin: "null" } });
    expect(result.ok).toBe(false);
  });
});

/** Whether the guard this test replaced would have let `source` through (it matched only these reads). */
function oldGuardMisses(source: string): boolean {
  return !/location\.(origin|host|href)|document\.location/.test(source);
}

/** Where a module reads the page's location (see `./location-guard`) or fetches relatively. */
function forbiddenReads(path: string, source: string): string[] {
  const text = source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  const reads: string[] = [];
  if (locationReads(path, source).length > 0) reads.push("location");
  if (/\bfetch\s*\(/.test(text)) reads.push("fetch");
  return reads;
}
