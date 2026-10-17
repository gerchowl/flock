import { describe, expect, test } from "bun:test";

import { readShellConfig, SHELL_CONFIG_GLOBAL } from "../src/config";

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

  test("a non-network scheme and an empty token are refused", () => {
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "file:///x", token: "t" })).ok).toBe(false);
    expect(readShellConfig(scope({ transport: "wire", serverUrl: "wss://h/v1/ws", token: "" })).ok).toBe(false);
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

  test("the source never reads the page origin or fetches relatively", async () => {
    const glob = new Bun.Glob("src/**/*.{ts,tsx}");
    for await (const path of glob.scan({ cwd: `${import.meta.dir}/..` })) {
      if (path.startsWith("src/gen/")) continue;
      // Doc comments may name the forbidden APIs; only code counts.
      const text = (await Bun.file(`${import.meta.dir}/../${path}`).text())
        .replace(/\/\*[\s\S]*?\*\//g, "")
        .replace(/^\s*\/\/.*$/gm, "");
      expect({ path, origin: /location\.(origin|host|href)|document\.location/.test(text) }).toEqual({ path, origin: false });
      expect({ path, fetch: /\bfetch\s*\(/.test(text) }).toEqual({ path, fetch: false });
    }
  });
});
