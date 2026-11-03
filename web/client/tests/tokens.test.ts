/**
 * The colour tokens meet WCAG 2.2 AA contrast in all four colour modes:
 * light, dark, and each with increased contrast (#2895, web/DESIGN.md
 * "Colour"). Text pairs need 4.5:1 (SC 1.4.3); focus rings and status
 * marks, which are not text, need 3:1 (SC 1.4.11).
 *
 * Not checked: `label.tertiary` (disabled text, exempt under SC 1.4.3),
 * the separator (decorative) and `legacy.*`, the pre-redesign palette kept
 * only until its screens move to the semantic roles.
 */
import { describe, expect, test } from "bun:test";

import { CLIENT_ROOT } from "./build";

type Rgba = readonly [number, number, number, number];
type Palette = Map<string, Rgba>;

/** Flattens a DTCG colour group into `label.primary` → sRGB components and alpha. */
function flatten(group: Record<string, unknown>, prefix = "", into: Palette = new Map()): Palette {
  for (const [key, node] of Object.entries(group)) {
    if (key.startsWith("$") || typeof node !== "object" || node === null) continue;
    const value = (node as { $value?: { colorSpace: string; components: number[]; alpha?: number } }).$value;
    if (value === undefined) {
      flatten(node as Record<string, unknown>, `${prefix}${key}.`, into);
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

const SURFACES = ["background.window", "background.sidebar"];
const TEXT: [string, string[], number][] = [
  ["label.primary", [...SURFACES, "selection.inactive"], 4.5],
  ["label.secondary", [...SURFACES, "selection.inactive"], 4.5],
  ["link", SURFACES, 4.5],
  ["accent.on", ["accent.fill", "selection.active"], 4.5],
  ["destructive", ["background.window"], 4.5],
  ["status.green", ["background.window"], 4.5],
  ["status.orange", ["background.window"], 4.5],
  ["status.red", ["background.window"], 4.5],
  ["focus-ring", SURFACES, 3],
  ["destructive", SURFACES, 3],
  ["status.green", SURFACES, 3],
  ["status.orange", SURFACES, 3],
  ["status.red", SURFACES, 3],
];

const MODES: [string, string[]][] = [
  ["light", ["light.json"]],
  ["dark", ["dark.json"]],
  ["light, increased contrast", ["light.json", "light-contrast.json"]],
  ["dark, increased contrast", ["dark.json", "dark-contrast.json"]],
];

describe.each(MODES)("%s", (_mode, files) => {
  test.each(TEXT)("%s on %p reaches %d:1", async (fg, backgrounds, minimum) => {
    const colours = await palette(...files);
    for (const bg of backgrounds) {
      const ratio = contrast(colours.get(fg)!, colours.get(bg)!);
      expect({ fg, bg, passes: ratio >= minimum, ratio: Math.round(ratio * 100) / 100 }).toMatchObject({ passes: true });
    }
  });
});

test("dark mode defines every light colour, and contrast modes override only known ones", async () => {
  const light = [...(await palette("light.json")).keys()].sort();
  expect([...(await palette("dark.json")).keys()].sort()).toEqual(light);
  for (const file of ["light-contrast.json", "dark-contrast.json"]) {
    for (const name of (await palette(file)).keys()) expect(light).toContain(name);
  }
});
