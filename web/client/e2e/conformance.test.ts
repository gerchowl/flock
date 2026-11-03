/**
 * The design-conformance suite (#2896, web/DESIGN.md "Conformance"): the
 * measurable part of Apple's HIG and WCAG 2.2, checked on every rendered
 * element of the gallery (`gallery.html`) and of the real workbench screens
 * (the fixture build), in light, dark and both increased-contrast modes,
 * all under reduced motion. One test per rule; `e2e/conformance-checks.ts`
 * holds the in-page checks and `e2e/conformance-allow.ts` the known
 * violations.
 *
 * Each rule also has a seeded violation (`SEEDS`) that its check must catch.
 * `CONFORMANCE_SEED=<rule> bun test ./e2e/conformance.test.ts` injects that
 * seed into every page instead, so the rule's own test fails.
 *
 * Needs Playwright's Chromium: `just client-browsers`. Runs in
 * `just client-check` (`bun test ./e2e`).
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

import { chromium, type Browser, type Page } from "playwright";

import { CLIENT_ROOT, buildClient } from "../tests/build";
import { ALLOWED, type PageName } from "./conformance-allow";
import { installChecks, type CheckOptions, type Finding, type Rule } from "./conformance-checks";

const FIXTURE = `${CLIENT_ROOT}/artifacts/conformance-fixture`;
const GALLERY = `${CLIENT_ROOT}/artifacts/conformance-gallery`;
const AXE = createRequire(import.meta.url).resolve("axe-core/axe.min.js");
const DESKTOP = { width: 1280, height: 800 } as const;
const PHONE = { width: 390, height: 844 } as const;
const LIMIT = 60_000;
const AXE_LIMIT = 3_000;

type PageSpec = CheckOptions & { readonly url: () => string; readonly viewport: { width: number; height: number }; readonly palette: boolean };

const PAGES: Readonly<Record<PageName, PageSpec>> = {
  gallery: { url: () => pathToFileURL(`${GALLERY}/gallery.html`).href, viewport: DESKTOP, controlMin: 28, desktop: true, palette: false },
  workbench: { url: () => pathToFileURL(`${FIXTURE}/index.html`).href, viewport: DESKTOP, controlMin: 28, desktop: true, palette: true },
  "workbench-phone": { url: () => pathToFileURL(`${FIXTURE}/index.html`).href, viewport: PHONE, controlMin: 44, desktop: false, palette: false },
  "workbench-forced-phone": { url: () => `${pathToFileURL(`${FIXTURE}/index.html`).href}?layout=phone`, viewport: DESKTOP, controlMin: 44, desktop: false, palette: false },
};

const MODES = {
  light: { colorScheme: "light", contrast: "no-preference" },
  dark: { colorScheme: "dark", contrast: "no-preference" },
  "light-more": { colorScheme: "light", contrast: "more" },
  "dark-more": { colorScheme: "dark", contrast: "more" },
} as const;
type ModeName = keyof typeof MODES;

/** A violation the check must catch: markup appended to `into` on `page`. */
type Seed = { readonly page: PageName; readonly into: string; readonly html: string };

const SEEDS: Readonly<Record<Rule, Seed>> = {
  palette: { page: "gallery", into: "main", html: '<div data-testid=\"seed\" style="color: rgb(1, 2, 3)">Off-palette text</div>' },
  typography: { page: "gallery", into: "main", html: '<div data-testid=\"seed\" style="font-size: 9px">Tiny text</div>' },
  contrast: { page: "gallery", into: "main", html: '<div data-testid=\"seed\" style="color: var(--color-label-tertiary)">Faint text</div>' },
  "hit-size": { page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button" style="block-size: 16px">Small</button>' },
  grid: { page: "gallery", into: "main", html: '<div data-testid="seed" style="padding: 6px">Off the grid</div>' },
  materials: { page: "gallery", into: "main", html: '<div data-testid="seed" style="backdrop-filter: blur(8px)">Blurred content</div>' },
  motion: { page: "gallery", into: "main", html: '<div data-testid="seed" style="transition: transform 1s">Moving</div>' },
  keyboard: { page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button">×</button>' },
  structure: { page: "gallery", into: "nav section section", html: '<section data-testid="seed"><button class="g-button" aria-expanded="true">Level three</button></section>' },
};

const SEEDED = process.env.CONFORMANCE_SEED as Rule | undefined;
if (SEEDED !== undefined && !(SEEDED in SEEDS)) throw new Error(`CONFORMANCE_SEED=${SEEDED} names no rule`);

let browser: Browser;
const pages = new Map<string, Page>();
const usedAllowances = new Set<number>();

async function inject(page: Page, seed: Seed): Promise<void> {
  await page.evaluate(({ into, html }) => document.querySelector(into)!.insertAdjacentHTML("beforeend", html), seed);
}

/** A fresh page at `name` in `mode`, with the checks installed (and the env seed, if any). */
async function open(name: PageName, mode: ModeName): Promise<Page> {
  const spec = PAGES[name];
  const page = await browser.newPage({ viewport: spec.viewport });
  await page.emulateMedia({ ...MODES[mode], reducedMotion: "reduce" });
  await page.goto(spec.url());
  // The desktop workbench replays its transcript first: check it once the replay has settled.
  const ready = name === "gallery" ? "main" : name === "workbench" ? "[data-testid=transcript-synced]" : "[data-testid=phone]";
  await page.locator(ready).first().waitFor({ timeout: 10_000 });
  if (SEEDED !== undefined && SEEDS[SEEDED].page === name) await inject(page, SEEDS[SEEDED]);
  await page.evaluate(installChecks);
  return page;
}

/** The shared, untouched page for `name` in `mode`. */
async function shared(name: PageName, mode: ModeName): Promise<Page> {
  const key = `${name}/${mode}`;
  if (!pages.has(key)) pages.set(key, await open(name, mode));
  return pages.get(key)!;
}

/** Runs `rule`'s check on `page`, returning findings the allow-list does not cover. */
async function check(page: Page, name: PageName, rule: Rule): Promise<Finding[]> {
  const entries = ALLOWED.map((entry) => (entry.pages.includes(name) ? entry : { rule: "none", selector: "*" }));
  const result = await page.evaluate(
    ([rule, options, entries]) => (globalThis as unknown as { __hig: { run: Function } }).__hig.run(rule, options, entries),
    [rule, { controlMin: PAGES[name].controlMin, desktop: PAGES[name].desktop }, entries] as const,
  );
  for (const index of result.used as number[]) usedAllowances.add(index);
  return result.findings as Finding[];
}

/** `rule` on every page in every mode: one line per finding, naming where it was seen (a page alone: in every mode). */
async function everywhere(rule: Rule, extra?: (page: Page, name: PageName, mode: ModeName) => Promise<string[]>): Promise<string[]> {
  const seen = new Map<string, string[]>();
  const add = (line: string, at: string): void => void seen.set(line, [...new Set([...(seen.get(line) ?? []), at])]);
  for (const name of Object.keys(PAGES) as PageName[]) {
    for (const mode of Object.keys(MODES) as ModeName[]) {
      const page = await shared(name, mode);
      for (const finding of await check(page, name, rule)) add(`${finding.where}: ${finding.detail}`, `${name}/${mode}`);
      if (extra) for (const line of await extra(page, name, mode)) add(line, `${name}/${mode}`);
    }
  }
  const modes = Object.keys(MODES).length;
  return [...seen].map(([line, at]) => {
    const names = [...new Set(at.map((each) => each.split("/")[0]!))];
    const short = names.flatMap((name) => (at.filter((each) => each.startsWith(`${name}/`)).length === modes ? [name] : at.filter((each) => each.startsWith(`${name}/`))));
    return `${line} [${[...new Set(short)].join(", ")}]`;
  });
}

type Hig = { __hig: Record<string, (...args: never[]) => unknown> };
const hig = <T,>(page: Page, fn: string, arg?: unknown): Promise<T> =>
  page.evaluate(([fn, arg]) => ((globalThis as unknown as Hig).__hig[fn as string] as (a: unknown) => T)(arg), [fn, arg] as const);

/** Tab through `page`: every enabled control is reached, and each shows a focus ring. */
async function tabWalk(page: Page): Promise<string[]> {
  const controls = await hig<string[]>(page, "interactive");
  const seen = new Set<string>();
  const out: string[] = [];
  for (let step = 0; step < controls.length * 2 + 4; step++) {
    await page.keyboard.press("Tab");
    const focus = await hig<{ where: string; ring: boolean } | null>(page, "focusRing");
    if (focus === null) continue;
    if (!focus.ring && !seen.has(focus.where)) out.push(`${focus.where}: focused with no visible focus ring`);
    seen.add(focus.where);
  }
  for (const control of controls) if (!seen.has(control)) out.push(`${control}: not reachable with Tab`);
  return out;
}

/** Arrow keys move the selection of every list widget on screen. */
async function arrows(page: Page): Promise<string[]> {
  const out: string[] = [];
  for (const list of await hig<string[]>(page, "lists")) {
    if (!(await hig<boolean>(page, "focusList", list))) continue;
    const before = await hig<string>(page, "selection");
    await page.keyboard.press("ArrowDown");
    if ((await hig<string>(page, "selection")) === before) out.push(`${list}: ArrowDown does not move the selection`);
  }
  return out;
}

/** ⌘K opens the palette (its arrows work, it lists every toolbar action) and Esc restores focus. */
async function palette(page: Page): Promise<string[]> {
  const out: string[] = [];
  const actions = await hig<string[]>(page, "toolbarActions");
  await page.locator("[data-testid=agent-row]").first().focus();
  const before = await hig<string>(page, "where");
  await page.keyboard.press("ControlOrMeta+k");
  const dialog = page.getByRole("dialog");
  if (!(await dialog.isVisible())) return ["⌘K does not open the palette"];
  out.push(...(await arrows(page)));
  const commands = await hig<string[]>(page, "commands");
  for (const action of actions) if (!commands.some((command) => command.includes(action))) out.push(`toolbar action "${action}" has no ⌘K command`);
  await page.keyboard.press("Escape");
  const after = await hig<string>(page, "where");
  if (after !== before) out.push(`Esc moves focus to ${after}, not back to ${before}`);
  return out;
}

beforeAll(async () => {
  await Promise.all([buildClient("fixture", FIXTURE), buildClient("gallery", GALLERY)]);
  browser = await chromium.launch();
}, LIMIT);

afterAll(async () => {
  await browser?.close();
});

describe("design conformance", () => {
  test("1. palette: every colour is a token of the active mode", async () => {
    expect(await everywhere("palette")).toEqual([]);
  }, LIMIT);

  test("2. typography: 13 px body, nothing under 10 px, no thin small text, the system font", async () => {
    expect(await everywhere("typography")).toEqual([]);
  }, LIMIT);

  test("3. contrast: 4.5:1 for text, 3:1 for large text, and axe-core agrees", async () => {
    const source = await Bun.file(AXE).text();
    // Each axe run is bounded: a stalled run is a finding naming the stage, never a hung suite.
    const axe = async (page: Page, name: PageName, mode: ModeName): Promise<string[]> => {
      let stage = "inject";
      const run = async (): Promise<string[]> => {
        await page.bringToFront();
        await page.evaluate(source);
        stage = "run";
        return page.evaluate(async () => {
          const { axe } = globalThis as unknown as { axe: { run: (o: object) => Promise<{ violations: { id: string; nodes: { target: string[] }[] }[] }> } };
          const result = await axe.run({ runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"] } });
          return result.violations.flatMap((v) => v.nodes.map((node) => `axe ${v.id}: ${node.target.join(" ")}`));
        });
      };
      const started = performance.now();
      const stalled = new Promise<string[]>((resolve) => setTimeout(() => resolve([`axe did not finish (${stage}) within ${AXE_LIMIT} ms`]), AXE_LIMIT));
      const out = await Promise.race([run(), stalled]);
      console.error(`axe ${name}/${mode}: ${Math.round(performance.now() - started)} ms`);
      return out;
    };
    expect(await everywhere("contrast", axe)).toEqual([]);
  }, LIMIT);

  test("4. hit size: 28 px controls on desktop, 44 px on a phone, 4 px apart", async () => {
    expect(await everywhere("hit-size")).toEqual([]);
  }, LIMIT);

  test("5. spacing grid: margins, padding and gaps on the 4 px grid", async () => {
    expect(await everywhere("grid")).toEqual([]);
  }, LIMIT);

  test("6. materials: blur only in a toolbar, sidebar or popover", async () => {
    expect(await everywhere("materials")).toEqual([]);
  }, LIMIT);

  test("7. motion: nothing but opacity moves under reduced motion", async () => {
    expect(await everywhere("motion")).toEqual([]);
  }, LIMIT);

  test("8. keyboard: Tab reaches every control with a ring, controls are named, arrows and ⌘K work", async () => {
    const out = await everywhere("keyboard");
    for (const name of Object.keys(PAGES) as PageName[]) {
      const page = await open(name, "light");
      for (const line of [...(await tabWalk(page)), ...(await arrows(page)), ...(PAGES[name].palette ? await palette(page) : [])]) out.push(`${name}/light: ${line}`);
      await page.close();
    }
    expect(out).toEqual([]);
  }, LIMIT);

  test("9. structure: a sidebar two levels deep, toolbar actions in ⌘K, no appearance switch", async () => {
    expect(await everywhere("structure")).toEqual([]);
  }, LIMIT);

  test("each rule catches its seeded violation", async () => {
    const missed: string[] = [];
    for (const [rule, seed] of Object.entries(SEEDS) as [Rule, Seed][]) {
      const page = await open(seed.page, "light");
      await inject(page, seed);
      const findings = await check(page, seed.page, rule);
      if (!findings.some((finding) => finding.where.includes('[data-testid="seed"]'))) missed.push(rule);
      await page.close();
    }
    expect(missed).toEqual([]);
  }, LIMIT);

  test("the allow-list only shrinks: every entry is frozen and still matches", () => {
    // Remove a key when its entry goes; never add one.
    const FROZEN: readonly string[] = [
      "typography body",
      "typography .mark__count",
      "grid #root, #root *",
      'structure [data-testid="group-project"] > .group__head',
    ];
    expect(ALLOWED.map((entry) => `${entry.rule} ${entry.selector}`).filter((key) => !FROZEN.includes(key))).toEqual([]);
    if (SEEDED === undefined) expect(ALLOWED.filter((_, index) => !usedAllowances.has(index)).map((entry) => `${entry.rule} ${entry.selector}`)).toEqual([]);
  });
});
