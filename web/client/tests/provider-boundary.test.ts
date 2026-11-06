import { expect, test } from "bun:test";
import { parseSync, traverse, types as t } from "@babel/core";
import { realpathSync } from "node:fs";
import { dirname, isAbsolute, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const connectionModule = realpathSync(resolve(root, "src/wire/connection.ts"));

/** CI's source scan enforces provider-only use of the public handoff helper. */
function forbiddenHandoffs(path: string, source: string): string[] {
  if (path === "src/wire/connection.ts" || path === "src/wire/provider.ts") return [];
  const ast = parseSync(source, { filename: path, babelrc: false, configFile: false, parserOpts: { plugins: ["typescript", "jsx"], createImportExpressions: true } });
  if (ast === null) throw new Error(`cannot parse ${path}`);
  const names = new Set(["replaceProviderSession", "providerSessionHandoff", "replaceSession"]);
  const uses: string[] = [];
  // Resolve extensions, dot segments and symlinks as the client's Bun gate does.
  // Resolution failure is an error, so an unrecognized path cannot bypass policy.
  const targetsConnection = (specifier: string): boolean => {
    const resolved = Bun.resolveSync(specifier, dirname(resolve(root, path)));
    return isAbsolute(resolved) && realpathSync(resolved) === connectionModule;
  };
  traverse(ast, {
    Identifier(id) {
      if (names.has(id.node.name)) uses.push(id.node.name);
    },
    StringLiteral(literal) {
      if (names.has(literal.node.value)) uses.push(literal.node.value);
    },
    // Whole-module access can hide the helper's name behind spread or Object.values.
    ImportDeclaration(declaration) {
      if (declaration.node.importKind !== "type" && declaration.node.specifiers.some((specifier) => t.isImportNamespaceSpecifier(specifier)) && targetsConnection(declaration.node.source.value)) {
        uses.push("connection namespace import");
      }
    },
    ImportExpression(expression) {
      const source = expression.node.source;
      const specifier = t.isStringLiteral(source) ? source.value
        : t.isTemplateLiteral(source) && source.expressions.length === 0 ? source.quasis[0]?.value.cooked : null;
      // Require static module paths so the scan can reject connection-module acquisition.
      if (specifier === null || specifier === undefined || targetsConnection(specifier)) uses.push("connection dynamic import");
    },
    ExportAllDeclaration(declaration) {
      if (targetsConnection(declaration.node.source.value)) uses.push("connection re-export");
    },
    ExportNamedDeclaration(declaration) {
      const source = declaration.node.source;
      if (source !== null && source !== undefined && declaration.node.specifiers.some((specifier) => t.isExportNamespaceSpecifier(specifier)) && targetsConnection(source.value)) {
        uses.push("connection namespace re-export");
      }
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

test.each([
  ["dynamic spread", 'const wire = { ...await import("./wire/connection") };'],
  ["dynamic values", 'Object.values(await import("./wire/connection"));'],
  ["computed module path", 'const path = "./wire/connection"; Object.values(await import(path));'],
  ["namespace spread", 'import * as wire from "./wire/connection"; const copy = { ...wire };'],
  ["namespace values", 'import * as wire from "./wire/connection"; Object.values(wire);'],
])("the scan rejects reflected connection access: %s", (_name, source) => {
  expect(forbiddenHandoffs("src/App.tsx", source).length).toBeGreaterThan(0);
});

test.each([
  ["src/App.tsx", 'export * from "./wire/connection.ts";'],
  ["src/App.tsx", 'export * from "./wire/connection.js";'],
  ["src/App.tsx", 'export * from "./wire/../wire/connection.ts";'],
  ["src/workbench/Boundary.tsx", 'export * from "../wire/connection.ts";'],
  ["src/App.tsx", 'export * as wire from "./wire/connection.ts";'],
])("the scan resolves connection re-exports from %s: %s", (path, source) => {
  expect(forbiddenHandoffs(path, source).length).toBeGreaterThan(0);
});

test("resolved unrelated modules and named connection imports remain available", () => {
  expect(forbiddenHandoffs("src/App.tsx", 'export * from "./wire/env"; const env = await import("./wire/env");')).toEqual([]);
  expect(forbiddenHandoffs("src/App.tsx", 'import { WireConnection } from "./wire/connection"; connection.connect();')).toEqual([]);
});

test("absolute paths and literal templates cannot hide the connection module", () => {
  const absolute = JSON.stringify(connectionModule);
  expect(forbiddenHandoffs("src/App.tsx", `export * from ${absolute};`).length).toBeGreaterThan(0);
  expect(forbiddenHandoffs("src/App.tsx", 'Object.values(await import(`./wire/connection.ts`));').length).toBeGreaterThan(0);
});

test("unresolved module paths fail closed", () => {
  expect(() => forbiddenHandoffs("src/App.tsx", 'export * from "./wire/missing-module";')).toThrow();
});
