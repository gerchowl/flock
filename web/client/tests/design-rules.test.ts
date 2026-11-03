/**
 * The design rules fail on a seeded violation each, and the tree passes
 * them (#2895, web/DESIGN.md "Rules"):
 *
 * - stylelint strict values: a raw value on a strict property;
 * - stylelint custom properties: a raw value hidden in `--x`;
 * - stylelint layout fencing: flex, gap, margin, absolute and `@media`
 *   outside `src/layout/`;
 * - ESLint: `style=` in TSX outside `src/layout/`;
 * - tsc: an off-scale `space` on a primitive (`test/seeded/off-scale.tsx`).
 *
 * The linters run as child processes, as `bun run lint` runs them: the
 * test process carries happy-dom's globals (tests/setup.ts).
 */
import { describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { CLIENT_ROOT } from "./build";

type Run = { code: number; output: string };

async function run(args: string[], stdin?: string): Promise<Run> {
  const child = Bun.spawn([process.execPath, ...args], {
    cwd: CLIENT_ROOT,
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  return { code, output: stdout + stderr };
}

/** Lints `css` as if it were the file at `path`. */
const stylelint = (path: string, css: string): Promise<Run> => run(["x", "stylelint", "--ignore-disables", "--stdin-filename", path], css);
const eslint = (path: string, tsx: string): Promise<Run> => run(["x", "eslint", "--stdin", "--stdin-filename", path], tsx);

describe("stylelint strict values", () => {
  test.each([
    ["color", ".a { color: #fff; }"],
    ["font-size", ".a { font-size: 13px; }"],
    ["padding", ".a { padding: 0 var(--space-2) 3px; }"],
    ["border shorthand", ".a { border: 1px solid var(--color-separator); }"],
    ["colour function", ".a { background: rgb(0 0 0 / 0.3); }"],
    ["z-index", ".a { z-index: 3; }"],
    ["custom property", ".a { --x: 13px; }"],
  ])("rejects a raw %s", async (_name, css) => {
    const { code, output } = await stylelint("src/workbench/seeded.css", css);
    expect(code).toBe(2);
    expect(output).toMatch(/declaration-strict-value|declaration-property-value-allowed-list/);
  });

  test("accepts tokens and keywords", async () => {
    const css = ".a { color: var(--color-label-primary); border: var(--border-width-thin) solid var(--color-separator); padding: 0 var(--space-2); font: inherit; --x: var(--space-1); }";
    expect(await stylelint("src/workbench/seeded.css", css)).toEqual({ code: 0, output: "" });
  });

  test("holds the legacy ignore list to strict colours", async () => {
    expect((await stylelint("src/styles.css", ".a { color: #fff; }")).code).toBe(2);
    expect((await stylelint("src/styles.css", ".a { padding: 3px; }")).code).toBe(0);
  });

  test("allows raw values in tokens.css only", async () => {
    expect((await stylelint("src/tokens.css", ":root { --space-1: 4px; }")).code).toBe(0);
    expect((await stylelint("src/layout/primitives/seeded.css", ":root { --space-1: 4px; }")).code).toBe(2);
  });
});

describe("stylelint layout fencing", () => {
  test.each([
    ["display: flex", ".a { display: flex; }"],
    ["display: inline-grid", ".a { display: inline-grid; }"],
    ["gap", ".a { gap: var(--space-2); }"],
    ["margin", ".a { margin-left: auto; }"],
    ["position: absolute", ".a { position: absolute; }"],
    ["@media", "@media (max-width: 40rem) { .a { color: inherit; } }"],
  ])("rejects %s outside src/layout/", async (_name, css) => {
    const { code, output } = await stylelint("src/workbench/seeded.css", css);
    expect(code).toBe(2);
    expect(output).toMatch(/disallowed/i);
    expect((await stylelint("src/layout/primitives/seeded.css", css)).code).toBe(0);
  });
});

describe("ESLint style ban", () => {
  test.each([
    ["style={…}", 'export const A = () => <div style={{ color: "red" }} />;'],
    ["attr:style", 'export const A = () => <div attr:style="color: red" />;'],
  ])("rejects %s outside src/layout/", async (_name, tsx) => {
    const { code, output } = await eslint("src/workbench/Seeded.tsx", tsx);
    expect(code).toBe(1);
    expect(output).toContain("Inline style bypasses the design tokens");
    expect((await eslint("src/layout/primitives/Seeded.tsx", tsx)).code).toBe(0);
  });
});

describe("typed spacing", () => {
  test("an off-scale space is a type error", async () => {
    const { code, output } = await run(["x", "tsc", "--noEmit", "-p", "test/seeded/tsconfig.json"]);
    expect(code).not.toBe(0);
    expect(output).toContain("off-scale.tsx");
    expect(output).toContain('"14"');
  });
});

test("the tree passes every rule, and the generated tokens are current", async () => {
  const { code, output } = await run(["run", "lint"]);
  expect({ code, output: code === 0 ? "" : output }).toEqual({ code: 0, output: "" });
}, 60_000);


describe("lint what ships (#2933)", () => {
  test.each([
    ["jsx scope", "src/probe.jsx", 'export const A = () => <div style={{color:"red"}} />'],
    ["js scope", "src/probe.js", 'el.style.color = "red"'],
    ["runtime member", "src/probe.ts", 'el.style.setProperty("margin", "13px")'],
    ["runtime assign", "src/probe.ts", 'Object.assign(el.style, {color:"red"})'],
    ["spread", "src/probe.tsx", 'export const A = () => <div {...{style:{color:"red"}}} />'],
    ["sheet", "src/probe.ts", 'new CSSStyleSheet()'],
    ["adoption", "src/probe.ts", 'document.adoptedStyleSheets = []'],
    ["computed style", "src/probe.ts", 'el["style"].color = "red"'],
    ["raw import", "src/probe.ts", 'import css from "./probe.txt?raw"'],
    ["inline import", "src/probe.ts", 'import css from "./probe.css?inline"'],
    ["disable", "src/probe.tsx", '// eslint-disable-next-line\nexport const A = () => <div style={{color:"red"}} />'],
    ["cast", "src/probe.tsx", 'export const A = () => <Stack space={"14" as any} />'],
    ["aliased cast", "src/probe.tsx", 'const value = "14" as "4"; export const A = () => <Stack space={value} />'],
    ["spread cast", "src/probe.tsx", 'const props = {space: "14" as "4"}; export const A = () => <Stack {...props} />'],
    ["typed cast", "src/probe.tsx", 'export const A = () => <Stack space={"14" as SpaceStep} />'],
    ["type suppression", "src/probe.tsx", '// @ts-expect-error\nexport const A = () => <Stack space="14" />'],
  ])("rejects %s", async (_name, file, source) => {
    const result = await eslint(file, source);
    expect(result.code).toBe(1);
  });
  test("CSS disables cannot suppress strict values", async () => {
    expect((await stylelint("src/probe.css", "/* stylelint-disable */ .a { color: red; }")).code).toBe(2);
  });
});


describe("shipped CSS graph (#2933)", () => {
  test.each([
    ["pcss", "src/probe.pcss", ".a { color: red; }", false],
    ["raw graph import", "src/probe.txt?raw", ".a { color: inherit; }", false],
    ["inline graph import", "src/probe.css?inline", ".a { color: inherit; }", false],
    ["external CSS", "vendor/probe.css", ".a { padding: 13px; }", false],
    ["CSS import", "vendor/probe.css", ".a { display: flex; }", true],
    ["upper property", "src/probe.css", ".a { COLOR: red; }", false],
    ["escaped property", "src/probe.css", String.raw`.a { colo\72: red; }`, false],
    ["escaped layout", "src/probe.css", String.raw`.a { displa\79: FLEX; }`, false],
    ["upper position", "src/probe.css", ".a { position: ABSOLUTE; }", false],
    ["upper media", "src/probe.css", "@MEDIA (width: 40rem) { .a { color: inherit; } }", false],
    ["CSS disable", "src/probe.css", "/* stylelint-disable */ .a { color: red; }", false],
    ["layout import authority", "src/layout/probe.css", ".a { display: flex; }", true],
    ["token authority", "src/tokens.css", ":root { --space-1: 4px; }", false],
    ["legacy authority", "src/styles.css", ".a { padding: 13px; }", false],
  ])("audits %s in a real Vite build", async (_name, file, css, cssImport) => {
    const root = await mkdtemp(join(tmpdir(), "design-"));
    try {
      await mkdir(join(root, "src/layout"), { recursive: true });
      await mkdir(join(root, "vendor"));
      await writeFile(join(root, "index.html"), '<script type="module" src="/src/main.js"></script>');
      await writeFile(join(root, "src/main.js"), `import "${cssImport ? "./entry.css" : `../${file}`}"`);
      if (cssImport) await writeFile(join(root, "src/entry.css"), `@import "../${file}" layer(components);`);
      await writeFile(join(root, file.split("?")[0]!), css);
      const result = await run(["run", "build", root]);
      const accepted = ["layout import authority", "token authority", "legacy authority"].includes(_name);
      expect({ code: result.code, output: result.code === 0 ? "" : result.output }).toMatchObject({ code: accepted ? 0 : 1 });
      if (!accepted) expect(result.output).toMatch(/design CSS/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }, 30_000);
});


test("the legacy list can only shrink (#2933)", async () => {
  const config = await Bun.file(`${CLIENT_ROOT}/stylelint.config.js`).text();
  const legacy = JSON.parse(config.match(/const LEGACY = (\[[^;]*\]);/)![1]!) as string[];
  for (const file of legacy) expect(["src/styles.css", "src/workbench/workbench.css"]).toContain(file);
});
