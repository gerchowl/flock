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
const stylelint = (path: string, css: string): Promise<Run> => run(["x", "stylelint", "--stdin-filename", path], css);
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
