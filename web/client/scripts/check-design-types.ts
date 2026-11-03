/** Check source against DOM capabilities unavailable to application code. */
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const client = fileURLToPath(new URL("..", import.meta.url));
const native = dirname(fileURLToPath(import.meta.resolve(`@typescript/typescript-${process.platform}-${process.arch}/package.json`)));
const cache = join(client, "node_modules/.cache");
await mkdir(cache, { recursive: true });
const root = await mkdtemp(join(cache, "design-types-"));
try {
  let dom = await readFile(join(native, "lib/lib.dom.d.ts"), "utf8");
  function replace(pattern: RegExp, replacement: string): void {
    if (!pattern.test(dom)) throw new Error(`design types: DOM library shape changed: ${pattern}`);
    dom = dom.replace(pattern, replacement);
  }
  replace(/get style\(\): CSSStyleDeclaration;/g, "readonly style: unknown;");
  replace(/set style\(cssText: string\);/g, "");
  replace(/readonly attributeStyleMap: StylePropertyMap;/g, "readonly attributeStyleMap: unknown;");
  replace(/readonly styleMap: StylePropertyMap;/g, "readonly styleMap: unknown;");
  replace(/adoptedStyleSheets: CSSStyleSheet\[\];/g, "readonly adoptedStyleSheets: unknown;");
  replace(/readonly styleSheets: StyleSheetList;/g, "readonly styleSheets: unknown;");
  replace(/animate\(keyframes:[^\n]+;/g, "readonly animate: unknown;");
  replace(/declare var CSSStyleSheet: \{[\s\S]*?\n\};/, "declare const CSSStyleSheet: unknown;");
  replace(/interface (CSSStyleSheet|CSSGroupingRule|CSSKeyframesRule) extends [\s\S]*?\n\}/g, "$&");
  dom = dom.replace(/interface (CSSStyleSheet|CSSGroupingRule|CSSKeyframesRule) extends [\s\S]*?\n\}/g, (block) =>
    block.replace(/(?:insertRule|deleteRule|addRule|removeRule|replace|replaceSync|appendRule)\([^\n]+;/g,
      (method) => `readonly ${method.slice(0, method.indexOf("("))}: unknown;`));
  const lib = join(root, "lib.dom.d.ts");
  await writeFile(lib, dom);
  const seed = process.argv[2];
  await writeFile(join(root, "tsconfig.json"), JSON.stringify({
    extends: join(client, "tsconfig.json"),
    compilerOptions: { lib: ["es2022", "dom.iterable"] },
    files: [lib, join(client, "scripts/design-types.d.cts"), ...(seed ? [resolve(seed)] : [])],
    include: seed ? [] : [join(client, "src/**/*.ts"), join(client, "src/**/*.tsx")],
  }));
  const child = Bun.spawn([process.execPath, "x", "tsc", "--noEmit", "--listFiles", "-p", join(root, "tsconfig.json")], {
    cwd: client, stdout: "pipe", stderr: "inherit",
  });
  const output = await new Response(child.stdout).text();
  const lines = output.split("\n");
  if (lines.includes(join(native, "lib/lib.dom.d.ts"))) throw new Error("design types: unrestricted DOM library was loaded");
  const diagnostics = lines.filter((line) => !line.startsWith("/") || !/\.[cm]?tsx?$/.test(line));
  if (diagnostics.join("\n").trim()) process.stdout.write(diagnostics.join("\n"));
  process.exitCode = await child.exited;
} finally {
  await rm(root, { recursive: true, force: true });
}
