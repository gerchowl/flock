import { describe, expect, test } from "bun:test";

import { readShellConfig, SHELL_CONFIG_GLOBAL } from "../src/config";
import { endpointUrl } from "../src/wire/env";
import { exchangeUrl } from "../src/wire/token";

function scope(config: unknown): object {
  return { [SHELL_CONFIG_GLOBAL]: config };
}

describe("readShellConfig", () => {
  test("a missing config is an explicit error, not a default", () => {
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

  test("the guard still catches a location read anywhere but the allowed fallback", () => {
    expect(forbiddenReads("src/App.tsx", "const here = window.location.origin;")).toEqual(["location"]);
    expect(forbiddenReads("src/config.ts", "export function readShellConfig() { return location.host; }")).toEqual(["location"]);
    expect(forbiddenReads("src/config.ts", "export function sameOriginDefaults(scope) {\n  return scope.location.origin;\n}\n")).toEqual([]);
    expect(forbiddenReads("src/wire/token.ts", "export function sameOriginDefaults(scope) {\n  return scope.location.origin;\n}\n")).toEqual(["location"]);
    expect(forbiddenReads("src/x.ts", "void fetch(url);")).toEqual(["fetch"]);
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

/**
 * Where a module reads the page's location or fetches relatively. Exactly
 * one place may read `location`: `sameOriginDefaults` in `src/config.ts`,
 * the browser shell's fallback when no config global exists (main's
 * decision on #1880: `flk` serves no config script). Doc comments may name
 * the forbidden APIs; only code counts.
 */
function forbiddenReads(path: string, source: string): string[] {
  let text = source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  if (path === "src/config.ts") text = text.replace(/export function sameOriginDefaults\([\s\S]*?\n\}\n/, "");
  const reads: string[] = [];
  if (/location\.(origin|host|href)|document\.location/.test(text)) reads.push("location");
  if (/\bfetch\s*\(/.test(text)) reads.push("fetch");
  return reads;
}
