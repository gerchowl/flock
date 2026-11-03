/**
 * The token pipeline and the colour tokens' contrast (#2895, #2933;
 * web/DESIGN.md "Tokens" and "Colour").
 *
 * Contrast is checked on the full matrix, never on hand-picked pairs: every
 * colour that is not a surface is content, and each content colour is
 * checked on each surface in each of the four modes (light, dark, and each
 * with increased contrast). Text needs 4.5:1 (WCAG 2.2 SC 1.4.3); the
 * graphics in GRAPHICS need 3:1 (SC 1.4.11). The pairs that never render
 * together, or that WCAG exempts, are listed in EXEMPT with the reason.
 * A new colour is therefore checked on every surface until someone exempts
 * it on purpose.
 */
import { describe, expect, test } from "bun:test";
import { cp, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { generate } from "../scripts/tokens";
import { TOKENS, type Token } from "../src/tokens";
import { CLIENT_ROOT } from "./build";

type Colour = Extract<Token, `color-${string}`>;
type Rgba = readonly [number, number, number, number];
type Palette = Map<string, Rgba>;

/** The backgrounds content is drawn on. They must be opaque. */
const SURFACES = [
  "color-background-window",
  "color-background-sidebar",
  "color-selection-inactive",
  "color-selection-active",
  "color-accent-fill",
] as const satisfies readonly Colour[];
type Surface = (typeof SURFACES)[number];

/** The surfaces whose content switches to its alternate colours (HIG "emphasized" background style). */
const EMPHASISED = ["color-selection-active", "color-accent-fill"] as const satisfies readonly Surface[];
const PLAIN = SURFACES.filter((surface) => !(EMPHASISED as readonly Surface[]).includes(surface));

/** Non-text content (SC 1.4.11): 3:1. Every other content colour is text: 4.5:1. */
const GRAPHICS: readonly Colour[] = ["color-focus-ring"];

/** The colours drawn on an emphasised surface instead of the ordinary content colours. */
const ON_EMPHASISED: readonly Colour[] = ["color-accent-on", "color-label-on-selection", "color-label-on-selection-secondary"];

const LEGACY = TOKENS.filter((name): name is Extract<Colour, `color-legacy-${string}`> => name.startsWith("color-legacy-"));

type Exemption = { readonly content: readonly Colour[]; readonly on: readonly Surface[]; readonly why: string };

/**
 * Pairs that are not checked. Each needs a reason a reviewer can verify, and
 * each must exempt at least one pair that fails, or it is stale.
 */
const EXEMPT: readonly Exemption[] = [
  {
    content: ["color-label-tertiary"],
    on: PLAIN,
    why: "Disabled text only; SC 1.4.3 exempts inactive user interface components.",
  },
  {
    content: ["color-separator"],
    on: SURFACES,
    why: "Decorative hairlines between regions; they do not identify a component or its state (SC 1.4.11).",
  },
  {
    content: [
      "color-label-primary",
      "color-label-secondary",
      "color-label-tertiary",
      "color-link",
      "color-destructive",
      "color-status-green",
      "color-status-orange",
      "color-status-red",
    ],
    on: EMPHASISED,
    why: "HIG: content on an accent fill or an emphasised selection takes the alternate colours in ON_EMPHASISED (label.on-selection, accent.on), never the ordinary ones.",
  },
  {
    content: ON_EMPHASISED,
    on: PLAIN,
    why: "The alternate colours are drawn only on an accent fill or an emphasised selection; an unemphasised selection keeps the ordinary labels.",
  },
  {
    content: ["color-focus-ring"],
    on: EMPHASISED,
    why: "The ring is drawn outside the element (outline-offset, src/styles.css), on the surface around it, never on its own fill.",
  },
  {
    content: LEGACY,
    on: SURFACES,
    why: "The pre-redesign palette, drawn only on its own legacy backgrounds; removed as screens move to the semantic roles (#2895 follow-up).",
  },
];

/** Flattens a DTCG colour group into `color-label-primary` → sRGB components and alpha. */
function flatten(group: Record<string, unknown>, prefix = "color-", into: Palette = new Map()): Palette {
  for (const [key, node] of Object.entries(group)) {
    if (key.startsWith("$") || typeof node !== "object" || node === null) continue;
    const value = (node as { $value?: { colorSpace: string; components: number[]; alpha?: number } }).$value;
    if (value === undefined) {
      flatten(node as Record<string, unknown>, `${prefix}${key}-`, into);
      continue;
    }
    expect(value.colorSpace).toBe("srgb");
    const [r, g, b] = value.components as [number, number, number];
    into.set(`${prefix}${key}`, [r, g, b, value.alpha ?? 1]);
  }
  return into;
}

async function palette(...files: string[]): Promise<Palette> {
  const merged: Palette = new Map();
  for (const file of files) {
    const json = await Bun.file(`${CLIENT_ROOT}/tokens/color/${file}`).json();
    for (const [name, rgba] of flatten(json.color)) merged.set(name, rgba);
  }
  return merged;
}

const linear = (c: number): number => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const luminance = ([r, g, b]: Rgba): number => 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);

/** WCAG contrast of `fg` composited over the opaque `bg`. */
function contrast(fg: Rgba, bg: Rgba): number {
  const a = fg[3];
  const over: Rgba = [fg[0] * a + bg[0] * (1 - a), fg[1] * a + bg[1] * (1 - a), fg[2] * a + bg[2] * (1 - a), 1];
  const [hi, lo] = [luminance(over), luminance(bg)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

const exemptions = (content: string, surface: Surface): readonly Exemption[] =>
  EXEMPT.filter((entry) => (entry.content as readonly string[]).includes(content) && entry.on.includes(surface));

type Pair = { readonly content: string; readonly surface: Surface; readonly ratio: number; readonly minimum: number };

/** Every content colour on every surface: the full matrix, exempt pairs included. */
function matrix(colours: Palette): Pair[] {
  const pairs: Pair[] = [];
  for (const [content, rgba] of colours) {
    if ((SURFACES as readonly string[]).includes(content)) continue;
    const minimum = (GRAPHICS as readonly string[]).includes(content) ? 3 : 4.5;
    for (const surface of SURFACES) pairs.push({ content, surface, ratio: contrast(rgba, colours.get(surface)!), minimum });
  }
  return pairs;
}

const describePair = ({ content, surface, ratio, minimum }: Pair): string =>
  `${content} on ${surface}: ${Math.round(ratio * 100) / 100} < ${minimum}`;

/** The pairs below their minimum that no exemption covers. */
const violations = (colours: Palette): string[] =>
  matrix(colours)
    .filter((pair) => pair.ratio < pair.minimum && exemptions(pair.content, pair.surface).length === 0)
    .map(describePair);

const MODES: [string, string[]][] = [
  ["light", ["light.json"]],
  ["dark", ["dark.json"]],
  ["light, increased contrast", ["light.json", "light-contrast.json"]],
  ["dark, increased contrast", ["dark.json", "dark-contrast.json"]],
];

describe.each(MODES)("contrast, %s", (_mode, files) => {
  test("every content colour reaches its minimum on every surface it renders on", async () => {
    expect(violations(await palette(...files))).toEqual([]);
  });

  test("the surfaces are opaque", async () => {
    const colours = await palette(...files);
    for (const surface of SURFACES) expect({ surface, alpha: colours.get(surface)?.[3] }).toEqual({ surface, alpha: 1 });
  });
});

describe("the contrast matrix", () => {
  test("checks a new colour on every surface", async () => {
    const colours = await palette("light.json");
    colours.set("color-label-quaternary", [0.780392, 0.780392, 0.8, 1]);
    expect(violations(colours)).toContain("color-label-quaternary on color-background-window: 1.68 < 4.5");
  });

  test("checks secondary text on a selected row", async () => {
    const colours = await palette("light.json");
    colours.set("color-label-on-selection-secondary", colours.get("color-label-secondary")!);
    expect(violations(colours)).toContainEqual(expect.stringMatching(/^color-label-on-selection-secondary on color-selection-active: /));
  });

  test("every exemption names real colours, exempts a failing pair, and overlaps no other", async () => {
    const modes = await Promise.all(MODES.map(([, files]) => palette(...files)));
    for (const entry of EXEMPT) {
      for (const content of entry.content) expect(modes[0]!.has(content)).toBe(true);
      const fails = modes.some((colours) =>
        matrix(colours).some((pair) => pair.ratio < pair.minimum && exemptions(pair.content, pair.surface).includes(entry)),
      );
      expect({ why: entry.why, fails }).toEqual({ why: entry.why, fails: true });
    }
    for (const pair of matrix(modes[0]!)) expect(exemptions(pair.content, pair.surface).length).toBeLessThanOrEqual(1);
  });
});

test("dark mode defines every light colour, and contrast modes override only known ones", async () => {
  const light = [...(await palette("light.json")).keys()].sort();
  expect([...(await palette("dark.json")).keys()].sort()).toEqual(light);
  for (const file of ["light-contrast.json", "dark-contrast.json"]) {
    for (const name of (await palette(file)).keys()) expect(light).toContain(name);
  }
});

describe("the spacing scale", () => {
  /** Generates the tokens from a copy of `tokens/` with `space` replaced. */
  async function generateWith(space: Record<string, unknown>): Promise<Awaited<ReturnType<typeof generate>>> {
    const root = await mkdtemp(join(tmpdir(), "tokens-"));
    try {
      await cp(`${CLIENT_ROOT}/tokens`, `${root}/tokens`, { recursive: true });
      const base = await Bun.file(`${root}/tokens/base.json`).json();
      base.space = { ...base.space, ...space };
      await Bun.write(`${root}/tokens/base.json`, JSON.stringify(base));
      return await generate(root);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
  const px = (value: number) => ({ $value: { value, unit: "px" } });

  test.each([
    ["an off-scale value", { "13": px(13) }],
    ["a step that is not a whole number", { "3-5": px(14) }],
    ["a named step", { gutter: px(16) }],
  ])("rejects %s", async (_name, space) => {
    await expect(generateWith(space)).rejects.toThrow(/a spacing step is the step times 4 px/);
  });

  test("accepts an on-scale step and adds it to SpaceStep", async () => {
    const { ts } = await generateWith({ "16": px(64) });
    expect(ts).toContain('export type SpaceStep = "1" | "2" | "3" | "4" | "5" | "6" | "8" | "10" | "12" | "16";');
  });
});

describe("reserved token names", () => {
  async function stylelint(path: string, css: string): Promise<{ code: number; output: string }> {
    const child = Bun.spawn([process.execPath, "x", "stylelint", "--stdin-filename", path], {
      cwd: CLIENT_ROOT,
      stdin: new TextEncoder().encode(css),
      stdout: "pipe",
      stderr: "pipe",
    });
    const [code, stdout, stderr] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    return { code, output: stdout + stderr };
  }

  test.each([
    ["a colour rebinding", "src/workbench/seeded.css", ".a { --color-label-primary: var(--color-background-window); }"],
    ["a spacing rebinding", "src/workbench/seeded.css", ".a { --space-1: var(--space-2); }"],
    ["a new name in a token category", "src/workbench/seeded.css", ".a { --font-size-14: var(--font-size-13); }"],
    ["an escaped name", "src/workbench/seeded.css", ".a { --colo\\72-label-primary: var(--color-background-window); }"],
    ["an escaped dash", "src/workbench/seeded.css", ".a { -\\2d color-label-primary: var(--color-background-window); }"],
    ["an @property", "src/workbench/seeded.css", '@property --color-x { syntax: "<color>"; inherits: true; initial-value: transparent; }'],
    ["a legacy file", "src/styles.css", ".a { --color-legacy-fg: var(--color-legacy-bg); }"],
    ["a layout primitive", "src/layout/primitives/seeded.css", ".l-x { --space-2: var(--space-1); }"],
  ])("rejects %s", async (_name, path, css) => {
    const { code, output } = await stylelint(path, css);
    expect({ code, rule: output.includes("flock/no-token-redeclaration") }).toEqual({ code: 2, rule: true });
  });

  test("allows other custom properties, and tokens in tokens.css", async () => {
    expect(await stylelint("src/layout/primitives/seeded.css", ".l-x { --l-space: var(--space-2); }")).toEqual({ code: 0, output: "" });
    expect((await stylelint("src/tokens.css", ":root { --color-label-primary: #000; }")).code).toBe(0);
  });
});
