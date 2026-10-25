/**
 * The source guard behind `tests/config.test.ts`: which modules read the
 * page's location, found in the syntax tree (ADR-0038 §3).
 *
 * The browser shell derives its same-origin defaults in `src/config.ts`
 * and nowhere else, so any other access to the page's `location` is a
 * finding, however it is spelled: `location.search`, `window.location`,
 * `document.location`, `globalThis["location"]`, or a destructuring such as
 * `const { origin } = location`. A name the module declares itself (the
 * `location` parameter of `takeCode`) is not the page's.
 *
 * Babel parses it (already a dev dependency for the Solid transform): the
 * `typescript` package at 7 ships no JavaScript syntax-tree API.
 */
import { parseSync, traverse, types as t } from "@babel/core";

/** The only functions that may read the page's location, by file. */
const ALLOWED: Readonly<Record<string, readonly string[]>> = {
  "src/config.ts": ["sameOriginDefaults", "pageSearch"],
  // Takes the one-time code from the fragment of a page it is handed.
  "src/capture-code.ts": ["captureCode"],
};

const isLocationKey = (node: t.Node, computed: boolean): boolean =>
  (!computed && t.isIdentifier(node, { name: "location" })) || (computed && t.isStringLiteral(node, { value: "location" }));

/** The `line:column` of every access to the page's location in `source`, outside the allowed functions. */
export function locationReads(path: string, source: string): string[] {
  const ast = parseSync(source, { filename: path, babelrc: false, configFile: false, parserOpts: { plugins: ["typescript", "jsx"] } });
  if (ast === null) throw new Error(`cannot parse ${path}`);
  const allowed = new Set(ALLOWED[path] ?? []);
  const reads: string[] = [];
  const found = (node: t.Node): void => void reads.push(`${node.loc?.start.line ?? 0}:${node.loc?.start.column ?? 0}`);
  traverse(ast, {
    FunctionDeclaration(fn) {
      if (fn.node.id !== null && fn.node.id !== undefined && allowed.has(fn.node.id.name)) fn.skip();
    },
    // `location` on its own: the page's, unless this module declared the name.
    Identifier(id) {
      if (id.node.name === "location" && id.isReferencedIdentifier() && !id.scope.hasBinding("location", true)) found(id.node);
    },
    // `anything.location`, `anything["location"]`
    MemberExpression: (member) => isLocationKey(member.node.property, member.node.computed) && found(member.node.property),
    OptionalMemberExpression: (member) => isLocationKey(member.node.property, member.node.computed) && found(member.node.property),
    // `const { location } = window`, and `{ origin }` taken from it
    ObjectProperty(property) {
      if (t.isObjectPattern(property.parent) && isLocationKey(property.node.key, property.node.computed)) found(property.node.key);
    },
  });
  return reads;
}
