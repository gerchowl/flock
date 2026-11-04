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
import { dirname, join } from "node:path";

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
const stylelint = (path: string, css: string): Promise<Run> => run(["x", "stylelint", "--config", "stylelint.config.js", "--config-basedir", ".", "--ignore-disables", "--stdin-filename", path], css);
const eslint = (path: string, tsx: string): Promise<Run> => run(["x", "eslint", "--config", "eslint.config.js", "--stdin", "--stdin-filename", path], tsx);

async function designTypes(source: string): Promise<Run> {
  const root = await mkdtemp(join(CLIENT_ROOT, "node_modules/.cache/source-types-"));
  try {
    const file = join(root, "probe.ts");
    await writeFile(file, `declare const el: HTMLElement;\n${source}`);
    return await run(["scripts/check-design-types.ts", file]);
  } finally { await rm(root, { recursive: true, force: true }); }
}

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
    const typed = ["runtime member", "runtime assign", "sheet", "adoption", "computed style"].includes(_name);
    const result = typed ? await designTypes(source) : await eslint(file, source);
    expect(result.code).toBe(1);
  });
  test("CSS disables cannot suppress strict values", async () => {
    expect((await stylelint("src/probe.css", "/* stylelint-disable */ .a { color: red; }")).code).toBe(2);
  });
});


describe("shipped CSS graph (#2933)", () => {
  test.each([
    ["pcss", "src/probe.pcss", ".a { color: red; }", false],
    ["paint declaration", "vendor/probe.css", ".a { fill: red; }", false],
    ["registered property", "vendor/probe.css", '@property --p { syntax: "*"; inherits: false; initial-value: flex; } .a { display: var(--p); }', false],
    ["resolved fence", "vendor/probe.css", ".a { --p: var(--font-family-system); display: var(--p); }", false],
    ["transitive category", "vendor/probe.css", ".a { --x: var(--y); --y: var(--font-size-13); padding: var(--x); }", false],
    ["raw graph import", "src/probe.txt?raw", ".a { color: inherit; }", false],
    ["inline graph import", "src/probe.css?inline", ".a { color: inherit; }", false],
    ["dependency CSS", "node_modules/evilpkg/style.css", ".nm{color:red;margin:13px;display:flex;position:absolute}", false],
    ["outside source graph", "lib/outside.ts", 'document.body.style.margin = "13px"', false],
    ["cross-module graph cast", "lib/outside.ts", 'export const v = "14" as "4"', false],
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
      await writeFile(join(root, "index.html"), '<script type="module" src="/src/main.ts"></script>');
      await writeFile(join(root, "src/main.ts"), `import "${cssImport ? "./entry.css" : `../${file}`}"`);
      if (cssImport) await writeFile(join(root, "src/entry.css"), `@import "../${file}" layer(components);`);
      if (_name === "dependency CSS") {
        await mkdir(join(root, "node_modules/evilpkg"), { recursive: true });
        await writeFile(join(root, "node_modules/evilpkg/package.json"), '{"name":"evilpkg","version":"1.0.0"}');
        await writeFile(join(root, "src/main.ts"), 'import "evilpkg/style.css"');
      }
      await mkdir(dirname(join(root, file)), { recursive: true });
      await writeFile(join(root, file.split("?")[0]!), css);
      const result = await run(["run", "build", root]);
      const accepted = ["layout import authority", "token authority", "legacy authority"].includes(_name);
      expect({ code: result.code, output: result.code === 0 ? "" : result.output }).toMatchObject({ code: accepted ? 0 : 1 });
      if (!accepted) expect(result.output).toMatch(/design (CSS|source)/);
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


describe("fix round 1 source gates", () => {
  test.each([
    ["cross-module cast", "src/exported.ts", 'export const v = "14" as "4"'],
    ["parenthesized cast", "src/probe.ts", 'export const v = "14" as ("4")'],
    ["aliased design type", "src/probe.ts", 'import type { SpaceStep as S } from "./tokens"; export const v = "14" as S'],
    ["local design alias", "src/probe.ts", 'type S = "4"; export const v = "14" as S'],
    ["concatenated key with any", "src/probe.ts", 'declare const el: any; el["st"+"yle"].margin = "13px"'],
    ["explicit any", "src/probe.ts", 'const w: any = "14"'],
    ["generic any", "src/probe.ts", 'function id<T>(x: any): T { return x }'],
    ["outside source", "lib/outside.ts", 'document.body.style.margin = "13px"'],
    ["layout runtime exemption", "src/layout/probe.ts", 'document.body.style.margin = "13px"'],
  ])("rejects %s", async (_name, file, source) => {
    const typed = ["outside source", "layout runtime exemption"].includes(_name);
    expect((await (typed ? designTypes(source) : eslint(file, source))).code).toBe(1);
  });
  test("pins source stylelint configuration", async () => {
    const root = await mkdtemp(join(CLIENT_ROOT, "style-seed-"));
    try {
      await writeFile(join(root, ".stylelintrc.json"), JSON.stringify({ rules: { "color-named": "never" } }));
      expect((await stylelint(join(root, "probe.css"), ".a { padding: 13px; }")).code).toBe(2);
    } finally { await rm(root, { recursive: true, force: true }); }
  });
});

describe("reserved token writes everywhere", () => {
  const primitive = "src/layout/primitives/Seeded.tsx";
  test.each(["color", "space", "font", "size", "radius", "border", "layer"])("inline %s token", async (prefix) => {
    const result = await eslint(primitive, `<div style={{"--${prefix}-seed":"13px"}} />`);
    expect(result.code).toBe(1);
    expect(result.output).toContain("Reserved design tokens");
  });
  test.each([
    ["setProperty", 'el.style.setProperty("--color-label-primary", "red")'],
    ["style map", 'el.attributeStyleMap.set("--space-1", value)'],
    ["assignment", 'el.style["--font-body"] = "13px"'],
    ["computed object", 'const style = {["--radius-small"]:"13px"}'],
    ["concatenated property", 'el.style.setProperty("--space-" + "1", "13px")'],
    ["template property", 'el.style.setProperty(`--border-thin`, "13px")'],
    ["outside layout", 'const values = {"--layer-dialog":1}'],
  ])("%s", async (label, code) => {
    const result = await eslint(label === "outside layout" ? "src/probe.ts" : primitive, code);
    expect(result.code).toBe(1);
    expect(result.output).toContain("Reserved design tokens");
  });
  test("primitive local properties remain available", async () => {
    expect((await eslint(primitive, '<div style={{"--l-space":"var(--space-1)"}} />')).code).toBe(0);
  });
});

describe("fix round 1 typed capabilities", () => {
  test.each([
    ["computed generic assign", 'const k = "style"; Object.assign(el[k], {margin:"13px"})'],
    ["Object create any", 'const space: SpaceStep = Object.create(null).space'],
    ["Object values any", 'const space: SpaceStep = Object.values({} as object)[0]'],
    ["Object entries any", 'const space: SpaceStep = Object.entries({} as object)[0]![1]'],
    ["computed key", 'const k = "style"; el[k].margin = "13px"'],
    ["typed style map", 'el.attributeStyleMap.set("margin", CSS.px(13))'],
    ["sheet insertion", 'document.styleSheets[0]!.insertRule(".a{margin:13px}")'],
    ["animation", 'el.animate([{margin:"13px"}], {fill:"forwards"})'],
    ["constructor alias", 'const S = CSSStyleSheet; new S()'],
    ["computed adoption", 'document["adoptedStyleSheets"] = []'],
    ["concatenated key", 'el["st" + "yle"].margin = "13px"'],
    ["reflection", 'Reflect.get(el,"style").margin = "13px"'],
    ["JSON any", `const space: SpaceStep = JSON.parse('"14"')`],
    ["response any", 'const space: SpaceStep = await new Response().json()'],
  ])("rejects %s", async (_name, code) => {
    await mkdir(join(CLIENT_ROOT, "node_modules/.cache"), { recursive: true });
    const root = await mkdtemp(join(CLIENT_ROOT, "node_modules/.cache/design-seed-"));
    try {
      const file = join(root, "probe.ts");
      await writeFile(file, `import type { SpaceStep } from "${CLIENT_ROOT}/src/tokens";\ndeclare const el: HTMLElement;\n${code}\nexport {};`);
      await writeFile(join(root, "tsconfig.json"), JSON.stringify({ extends: `${CLIENT_ROOT}/tsconfig.json`, include: [file] }));
      const args = process.env.DESIGN_SEED_BASELINE
        ? ["x", "tsc", "--noEmit", "-p", join(root, "tsconfig.json")]
        : ["scripts/check-design-types.ts", file];
      const result = await run(args);
      expect(result.code).toBe(1);
      expect(result.output).toMatch(/error TS/);
      expect(result.output).toContain("probe.ts");
    } finally { await rm(root, { recursive: true, force: true }); }
  }, 30_000);
});


describe("declaration and fence seeds (#2937)", () => {
  test.each([
    ["5 fill", ".a { fill: #ff0000; }"],
    ["5 stroke", ".a { stroke: red; }"],
    ["5 text shadow", ".a { text-shadow: 1px 2px red; }"],
    ["5 filter", ".a { filter: drop-shadow(1px 2px red); }"],
    ["5 text stroke", ".a { -webkit-text-stroke: 1px red; }"],
    ["6 font", ".a { font: 14px/1.3 Comic Sans MS; }"],
    ["7 fallback", ".a { padding: var(--space-1, 13px); }"],
    ["7 undefined", ".a { color: var(--nope); }"],
    ["7 undefined token", ".a { padding: var(--space-999); }"],
    ["8 fixed", ".a { position: fixed; }"],
    ["8 sticky", ".a { position: sticky; }"],
    ["8 table", ".a { display: table; }"],
    ["8 contents", ".a { display: contents; }"],
    ["8 vendor box", ".a { display: -webkit-box; }"],
    ["8 float", ".a { float: left; }"],
    ["8 columns", ".a { columns: 2; }"],
    ["8 inset", ".a { inset: 0; }"],
    ["8 top", ".a { top: 0; }"],
    ["8 translate", ".a { translate: 1px; }"],
    ["8 container", "@container (width > 1px) { .a { color: inherit; } }"],
    ["8 supports", "@supports (display: grid) { .a { color: inherit; } }"],
    ["8 import", '@import "other.css" screen and (max-width: 10px);'],
    ["12 property", '@property --p { syntax: "<length>"; inherits: false; initial-value: 13px; } .a { padding: var(--p); }'],
    ["12 resolved display", ".a { --d: var(--font-family-system); display: var(--d); }"],
    ["12 keyword display", ".a { --d: auto; display: var(--d); }"],
    ["14 padding", ".a { padding: var(--font-size-13); }"],
    ["14 font size", ".a { font-size: var(--space-2); }"],
    ["14 transitive", ".a { --a: var(--b); --b: var(--font-size-13); padding: var(--a); }"],
    ["14 colour", ".a { color: var(--space-2); }"],
    ["14 radius", ".a { border-radius: var(--space-2); }"],
    ["14 layer", ".a { z-index: var(--space-2); }"],
    ["legacy colour", ".a { color: var(--color-legacy-fg); }"],
    ["legacy alias", ".a { --x: var(--color-legacy-fg); color: var(--x); }"],
  ])("rejects bypass %s", async (_name, css) => {
    const result = await stylelint("src/probe.css", css);
    expect({ code: result.code, output: result.output }).toMatchObject({ code: 2 });
  });

  test.each([
    ["cyclic aliases", ".a { --a: var(--b); --b: var(--a); padding: var(--a); }"],
    ["alternative alias category", ".a { --x: var(--space-2); padding: var(--x); } .b { --x: var(--font-size-13); }"],
    ["logical spacing", ".a { padding-inline-start: var(--font-size-13); }"],
    ["font family category", ".a { font-family: var(--color-label-primary); }"],
    ["font weight category", ".a { font-weight: var(--space-2); }"],
    ["border width category", ".a { border-width: var(--space-2); }"],
    ["border colour category", ".a { border-color: var(--border-width-thin); }"],
    ["shadow colour category", ".a { text-shadow: var(--font-size-13); }"],
    ["filter fallback", ".a { filter: drop-shadow(var(--space-1, 13px) 0 red); }"],
    ["resolved fixed", ".a { --p: var(--x); --x: auto; position: var(--p); }"],
    ["case normalized", ".a { DISPLAY: TABLE; }"],
    ["escaped display", String.raw`.a { displa\79: table; }`],
  ])("rejects %s", async (_name, css) => {
    expect((await stylelint("src/probe.css", css)).code).toBe(2);
  });
  test("validates references inside LEGACY and layout files too", async () => {
    for (const file of ["src/styles.css", "src/layout/primitives/seeded.css"]) {
      expect((await stylelint(file, ".a { color: var(--missing); }")).code).toBe(2);
      expect((await stylelint(file, '@property --x { syntax: "*"; initial-value: flex; }')).code).toBe(2);
    }
    expect((await stylelint("src/styles.css", ".a { color: var(--color-legacy-fg); }")).code).toBe(0);
    expect((await stylelint("src/layout/primitives/seeded.css", ".a { gap: var(--font-size-13); }")).code).toBe(2);
  });

  test.each(["fill", "stroke", "color"])("rejects JSX %s", async (prop) => {
    expect((await eslint("src/Probe.tsx", `<svg><rect ${prop}="#ff0000" /></svg>`)).code).toBe(1);
  });

  test.each([
    '<rect fill={"red"} />',
    '<rect attr:fill="red" />',
    'const paint = "red"; <rect stroke={paint} />',
    '<rect {...{color: "red"}} />',
    'const props = {["fi" + "ll"]: "red"}; <rect {...props} />',
  ])("rejects indirect JSX paint %s", async (source) => {
    expect((await eslint("src/Probe.tsx", source)).code).toBe(1);
  });
  test.each([
    '<rect fill="none" stroke="currentColor" />',
    '<rect fill={"currentColor"} />',
    '<rect {...{fill: "none"}} />',
    'const data = {color: "red"}; export {data}',
  ])("accepts paint keywords and ordinary data %s", async (source) => {
    expect((await eslint("src/Probe.tsx", source)).code).toBe(0);
  });
  test.each(["gap", "margin", "padding"])("checks %s category inside layout", async (prop) => {
    expect((await stylelint("src/layout/primitives/seeded.css", `.a { ${prop}: var(--font-size-13); }`)).code).toBe(2);
    expect((await stylelint("src/layout/primitives/seeded.css", `.a { ${prop}: var(--space-2); }`)).code).toBe(0);
  });

  test.each([
    ".a { --a: var(--b); --b: var(--space-2); padding: var(--a); }",
    ".a { display: block; position: relative; }",
    ".a { --d: none; display: var(--d); }",
    ".a { backdrop-filter: blur(var(--space-5)); filter: drop-shadow(var(--space-1) 0 var(--color-label-primary)); }",
    ".a { fill: currentColor; stroke: var(--color-label-primary); text-shadow: none; filter: none; }",
  ])("accepts valid declaration %s", async (css) => {
    expect(await stylelint("src/probe.css", css)).toEqual({ code: 0, output: "" });
  });
});
