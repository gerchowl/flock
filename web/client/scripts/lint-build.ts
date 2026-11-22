/** Lint the CSS Vite actually loads, preserving each stylesheet's authority. */
import { mkdtemp, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { basename, isAbsolute, relative, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { ESLint } from "eslint";
import stylelint from "stylelint";
import type { ChildNode, Root } from "postcss";
import { build } from "vite";

type VendorCSS = { readonly path: `node_modules/${string}.css`; readonly issue: `#${number}` };
const VENDOR_CSS: readonly VendorCSS[] = [];

// Icons belong to the checked JSX catalogue, not independently styled images.
// The existing standalone brand mark is immutable here, including its paints.
const BRAND_SVG_SHA256 = "5bb51f73c83259ae8ca12a66390b70e125656d4ed77b61c15219a88718f1fec5";
function lintSVG(source: string | Uint8Array): void {
  if (createHash("sha256").update(source).digest("hex") !== BRAND_SVG_SHA256) {
    throw new Error("design SVG: use the checked JSX icon catalogue; only the reviewed brand asset may ship as SVG");
  }
}

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

const modules = new Set<string>();
const svgAssets: Uint8Array[] = [];
let publicDir: string | false = false;
try {
await build({
  root: rootDir,
  mode: process.argv.includes("--fixture") ? "fixture" : "production",
  build: { assetsInlineLimit(file, content) {
    // Vite catches errors thrown while resolving new URL() and leaves the URL
    // for runtime. Retain the bytes and reject in generateBundle instead.
    if (/\.svg$/i.test(file)) svgAssets.push(content);
    return false; // Preserve the production CSP's same-origin asset policy.
  } },
  plugins: [{
    name: "flock-design-source",
    enforce: "pre",
    configResolved(config) { publicDir = config.publicDir; },
    async transform(_code, id) {
      const file = id.split("?")[0]!;
      if (isAbsolute(file) && /\.svg$/i.test(file)) lintSVG(await readFile(file));
      if (!isAbsolute(file) || !/\.[cm]?[jt]sx?$/.test(file) || file.includes("/node_modules/")) return;
      modules.add(file);
      const path = relative(rootDir, file);
      const filePath = resolve(client, path.startsWith("src/") ? path : `src/__graph__/${basename(file)}`);
      const results = await eslint.lintText(await readFile(file, "utf8"), { filePath });
      if (results.some((result) => result.errorCount > 0 || result.fatalErrorCount > 0)) {
        throw new Error(`design source: ${file}\n${results.flatMap((result) => result.messages.map((message) => message.message)).join("\n")}`);
      }
    },
    async generateBundle(_options, bundle) {
      for (const source of svgAssets) lintSVG(source);
      for (const asset of Object.values(bundle)) {
        if (asset.type === "asset" && /\.svg$/i.test(asset.fileName)) lintSVG(asset.source);
      }
      // Vite copies public files without adding them to its module/asset graph.
      if (publicDir) for (const file of await readdir(publicDir, { recursive: true }).catch((error: NodeJS.ErrnoException) => {
        if (error.code === "ENOENT") return [];
        throw error;
      })) {
        if (/\.svg$/i.test(file)) lintSVG(await readFile(resolve(publicDir, file)));
      }
      if (modules.size) {
        const child = Bun.spawn([process.execPath, resolve(client, "scripts/check-design-types.ts"), ...modules], { cwd: client, stdout: "pipe", stderr: "pipe" });
        const output = await new Response(child.stdout).text() + await new Response(child.stderr).text();
        if (await child.exited !== 0) throw new Error(`design source types: ${output}`);
      }
      for (const id of this.getModuleIds()) {
        if (/\?(?:[^#]*&)?(?:raw|inline)(?:[&#]|$)/.test(id)) throw new Error(`design CSS: raw and inline imports cannot be audited: ${id}`);
      }
    },
  }],
  css: { postcss: { plugins: [{ postcssPlugin: "flock-design-css", OnceExit: lintGraph }] } },
});

} finally { await rm(auditDir, { recursive: true, force: true }); }
