/** Lint the CSS Vite actually loads, preserving each stylesheet's authority. */
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { basename, isAbsolute, relative, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { ESLint } from "eslint";
import stylelint from "stylelint";
import type { ChildNode, Root } from "postcss";
import { build } from "vite";

type VendorCSS = { readonly path: `node_modules/${string}.css`; readonly issue: `#${number}` };
const VENDOR_CSS: readonly VendorCSS[] = [];

const client = fileURLToPath(new URL("..", import.meta.url));
const eslint = new ESLint({ cwd: client, overrideConfigFile: resolve(client, "eslint.config.js") });
const auditDir = await mkdtemp(resolve(tmpdir(), "flock-css-"));
const ignorePath = resolve(auditDir, "empty-ignore");
await writeFile(ignorePath, "");
const rootDir = await realpath(resolve(process.argv[2] === "--fixture" ? client : process.argv[2] ?? client));

/** CSS identifiers are case insensitive except for custom property names. */
function unescape(value: string): string {
  return value.replace(/\\([\da-f]{1,6}\s?|[^\r\n\f])/gi, (_match, escape: string) => {
    const hex = escape.trim();
    if (!/^[\da-f]{1,6}$/i.test(hex)) return escape;
    const code = Number.parseInt(hex, 16);
    return code === 0 || code > 0x10ffff ? "\ufffd" : String.fromCodePoint(code);
  });
}

/** postcss-import has expanded imports by this point, with source locations intact. */
async function lintGraph(root: Root): Promise<void> {
  const normalized = root.clone();
  normalized.walkDecls((decl) => {
    decl.prop = unescape(decl.prop);
    if (!decl.prop.startsWith("--")) decl.prop = decl.prop.toLowerCase();
    if (["display", "position"].includes(decl.prop)) decl.value = unescape(decl.value).toLowerCase();
  });
  normalized.walkAtRules((rule) => {
    rule.name = unescape(rule.name).toLowerCase();
    if (rule.name === "import") throw rule.error("design CSS: unresolved imports cannot be audited");
  });
  const sources = new Map<string, Root>();
  function collect(node: ChildNode, parentFile?: string): void {
    const file = node.source?.input.file;
    if (!file) throw node.error("design CSS: missing stylesheet provenance");
    if (file !== parentFile) {
      let sheet = sources.get(file);
      if (!sheet) {
        sheet = normalized.clone({ nodes: [] });
        sources.set(file, sheet);
      }
      const fragment = node.clone();
      if ("walk" in fragment) fragment.walk((child) => {
        if (child.source?.input.file !== file) child.remove();
      });
      sheet.append(fragment);
    }
    if ("nodes" in node) for (const child of node.nodes ?? []) collect(child, file);
  }
  for (const node of normalized.nodes) collect(node);
  for (const [file, sheet] of sources) {
    if (VENDOR_CSS.some((entry) => entry.path === relative(rootDir, file))) continue;
    // Map fixture roots to the same source-relative policy as the real client.
    const codeFilename = resolve(client, relative(rootDir, file));
    const result = await stylelint.lint({
      code: sheet.toString(), codeFilename,
      configFile: resolve(client, "stylelint.config.js"),
      configBasedir: client, disableDefaultIgnores: true, ignorePath,
      ignoreDisables: true,
    });
    if (result.errored || result.results.some((result) => result.ignored)) {
      const warnings = result.results.flatMap((result) => result.warnings.map((warning) => `${warning.line}:${warning.column} ${warning.text}`));
      throw new Error(`design CSS: ${file}\n${warnings.join("\n")}`);
    }
  }
}

try {
await build({
  root: rootDir,
  mode: process.argv.includes("--fixture") ? "fixture" : "production",
  plugins: [{
    name: "flock-design-source",
    enforce: "pre",
    async transform(_code, id) {
      const file = id.split("?")[0]!;
      if (!isAbsolute(file) || !/\.[cm]?[jt]sx?$/.test(file) || file.includes("/node_modules/")) return;
      const path = relative(rootDir, file);
      const filePath = resolve(client, path.startsWith("src/") ? path : `src/__graph__/${basename(file)}`);
      const results = await eslint.lintText(await readFile(file, "utf8"), { filePath });
      if (results.some((result) => result.errorCount > 0 || result.fatalErrorCount > 0)) {
        throw new Error(`design source: ${file}\n${results.flatMap((result) => result.messages.map((message) => message.message)).join("\n")}`);
      }
    },
    generateBundle() {
      for (const id of this.getModuleIds()) {
        if (/\?(?:[^#]*&)?(?:raw|inline)(?:[&#]|$)/.test(id)) throw new Error(`design CSS: raw and inline imports cannot be audited: ${id}`);
      }
    },
  }],
  css: { postcss: { plugins: [{ postcssPlugin: "flock-design-css", OnceExit: lintGraph }] } },
});

} finally { await rm(auditDir, { recursive: true, force: true }); }
