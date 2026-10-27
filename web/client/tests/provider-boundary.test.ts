import { expect, test } from "bun:test";
import { parseSync, traverse } from "@babel/core";

/** Only the provider may use the handoff capability that preserves its streak. */
function forbiddenHandoffs(path: string, source: string): string[] {
  if (path === "src/wire/connection.ts" || path === "src/wire/provider.ts") return [];
  const ast = parseSync(source, { filename: path, babelrc: false, configFile: false, parserOpts: { plugins: ["typescript", "jsx"] } });
  if (ast === null) throw new Error(`cannot parse ${path}`);
  const names = new Set(["replaceProviderSession", "providerSessionHandoff", "replaceSession"]);
  const uses: string[] = [];
  traverse(ast, {
    Identifier(id) {
      if (names.has(id.node.name)) uses.push(id.node.name);
    },
    StringLiteral(literal) {
      if (names.has(literal.node.value)) uses.push(literal.node.value);
    },
    // A namespace re-export would expose the capability under another module path.
    ExportAllDeclaration(declaration) {
      if (declaration.node.source.value.endsWith("/connection")) uses.push("connection re-export");
    },
  });
  return uses;
}

test("only the provider uses the connection's internal session handoff", async () => {
  for await (const path of new Bun.Glob("src/**/*.{ts,tsx}").scan({ cwd: `${import.meta.dir}/..` })) {
    if (path.startsWith("src/gen/")) continue;
    expect({ path, uses: forbiddenHandoffs(path, await Bun.file(`${import.meta.dir}/../${path}`).text()) }).toEqual({ path, uses: [] });
  }
});

test("UI imports, aliases, namespace calls and computed calls cannot acquire the handoff", () => {
  for (const source of [
    'import { replaceProviderSession } from "./wire/connection";',
    'import { replaceProviderSession as handoff } from "./wire/connection"; handoff(connection, install);',
    'import * as wire from "./wire/connection"; wire.replaceProviderSession(connection, install);',
    'wire["replaceProviderSession"](connection, install);',
    'const { replaceProviderSession: handoff } = wire;',
    'export { replaceProviderSession as handoff } from "./wire/connection";',
    'export * from "./wire/connection";',
    'connection.replaceSession(install);',
    'import { providerSessionHandoff } from "./wire/connection";',
  ]) {
    expect(forbiddenHandoffs("src/App.tsx", source).length).toBeGreaterThan(0);
  }
  expect(forbiddenHandoffs("src/App.tsx", 'connection.connect(); connection.disconnect();')).toEqual([]);
});
