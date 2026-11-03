/**
 * The design-conformance suite (#2896, web/DESIGN.md "Conformance"): the
 * measurable part of Apple's HIG and WCAG 2.2, checked on every rendered
 * element of the gallery (`gallery.html`) and of the real workbench screens
 * (the fixture build), in light, dark and both increased-contrast modes,
 * all under reduced motion. One test per rule; `e2e/conformance-checks.ts`
 * holds the in-page checks and `e2e/conformance-allow.ts` the known
 * violations.
 *
 * Every check, and every part of a multi-part rule (rules 4, 8 and 9), has
 * a seeded violation (`SEEDS`): a test runs each check on its page with and
 * without the seed and fails unless the seed adds a finding.
 * `CONFORMANCE_SEED=<seed> bun test ./e2e/conformance.test.ts` injects that
 * seed into its page for the whole run instead, so the rule's own test fails.
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
const SEED_LIMIT = 10_000;
const PROBE_LIMIT = 10_000;
const DOCUMENTS_PER_BROWSER = 8;

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

/** The keyboard and structure checks that drive the page from the test, beside the in-page rules. */
type Driven = "tab-reach" | "focus-ring" | "arrows" | "shortcut" | "parity";
type Check = Rule | Driven;

/** A violation `check` must catch: markup appended to `into` on `page`, and a script run there. */
type Seed = { readonly check: Check; readonly page: PageName; readonly into: string; readonly html: string; readonly script?: string };

const SEEDS: Readonly<Record<string, Seed>> = {
  palette: { check: "palette", page: "gallery", into: "main", html: '<div data-testid="seed" style="color: rgb(1, 2, 3)">Off-palette text</div>' },
  typography: { check: "typography", page: "gallery", into: "main", html: '<div data-testid="seed" style="font-size: 9px">Tiny text</div>' },
  contrast: { check: "contrast", page: "gallery", into: "main", html: '<div data-testid="seed" style="color: var(--color-label-tertiary)">Faint text</div>' },
  "hit-size": { check: "hit-size", page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button" style="block-size: 16px">Small</button>' },
  "hit-size-below-fold": {
    check: "hit-size",
    page: "gallery",
    into: "main",
    html: '<div style="block-size: 2400px"></div><button data-testid="seed" class="g-button" style="block-size: 16px">Below the fold</button>',
  },
  grid: { check: "grid", page: "gallery", into: "main", html: '<div data-testid="seed" style="padding: 6px">Off the grid</div>' },
  materials: { check: "materials", page: "gallery", into: "main", html: '<div data-testid="seed" style="backdrop-filter: blur(8px)">Blurred content</div>' },
  motion: { check: "motion", page: "gallery", into: "main", html: '<div data-testid="seed" style="transition: transform 1s">Moving</div>' },
  names: { check: "keyboard", page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button">×</button>' },
  "tab-reach": { check: "tab-reach", page: "gallery", into: "main", html: '<span data-testid="seed" class="g-button" role="button">Unreachable</span>' },
  "focus-ring": { check: "focus-ring", page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button" style="outline: none">Ringless</button>' },
  "focus-ring-resting-shadow": {
    check: "focus-ring",
    page: "gallery",
    into: "main",
    html: '<button data-testid="seed" class="g-button" style="outline: none; box-shadow: 0 0 0 1px var(--color-separator)">Always shadowed</button>',
  },
  arrows: {
    check: "arrows",
    page: "gallery",
    into: "main",
    html: '<div data-testid="seed" role="listbox" tabindex="0" aria-label="Inert list"><div role="option" aria-selected="true">One</div><div role="option">Two</div></div>',
  },
  "arrows-no-target": {
    check: "arrows",
    page: "gallery",
    into: "main",
    html: '<div data-testid="seed" role="listbox" aria-label="Unfocusable list"><div role="option">One</div><div role="option">Two</div></div>',
  },
  shortcut: {
    check: "shortcut",
    page: "workbench",
    into: "body",
    html: "",
    // Swallows Esc before the palette sees it: the palette stays open and focus never returns.
    script: 'window.addEventListener("keydown", (event) => { if (event.key === "Escape") event.stopImmediatePropagation(); }, true)',
  },
  depth: {
    check: "structure",
    page: "gallery",
    into: "nav section section",
    html: '<section data-testid="seed"><button class="g-button" aria-expanded="true">Level three</button></section>',
  },
  appearance: { check: "structure", page: "gallery", into: "main", html: '<button data-testid="seed" class="g-button">Dark mode</button>' },
  parity: { check: "parity", page: "workbench", into: "header.topbar", html: '<button data-testid="seed" type="button" class="action">Archive</button>' },
};

const SEEDED = process.env.CONFORMANCE_SEED;
if (SEEDED !== undefined && !(SEEDED in SEEDS)) throw new Error(`CONFORMANCE_SEED=${SEEDED} names no seed`);

let browser: Browser | undefined;
const usedAllowances = new Set<number>();

async function inject(page: Page, seed: Seed): Promise<void> {
  await page.evaluate(({ into, html }) => document.querySelector(into)!.insertAdjacentHTML("beforeend", html), seed);
  if (seed.script !== undefined) await page.evaluate(seed.script);
}

let tab: Page | undefined;
let documents = 0;

/**
 * Chromium on the Linux CI runner wedges after a few dozen documents in one
 * browser (pages or navigations alike), so the suite starts a fresh browser
 * every `DOCUMENTS_PER_BROWSER` documents and after any stall. The old one is
 * closed without waiting: a wedged browser may never answer.
 */
async function relaunch(): Promise<void> {
  const old = browser;
  const fresh = await chromium.launch();
  fresh.on("disconnected", () => fresh === browser && console.error("conformance: the browser disconnected"));
  browser = fresh;
  tab = await fresh.newPage();
  tab.on("crash", () => console.error("conformance: the page crashed"));
  void old?.close().catch(() => undefined);
}

/** A browser lost under a check (a crashed page or a dropped connection): infrastructure, not a finding. */
const lost = (error: unknown): boolean => error instanceof Error && /has been closed|crash/i.test(error.message);

/** Runs `work`; if the browser is lost under it, relaunches once and runs it again. */
async function retryLost<T>(label: string, work: () => Promise<T>): Promise<T> {
  try {
    return await work();
  } catch (error) {
    if (!lost(error)) throw error;
    console.error(`conformance: browser lost during ${label}, retrying once on a fresh one`);
    tab = undefined;
    return work();
  }
}

/** A fresh document at `name` in `mode`, with the checks installed (and the env seed, if any). */
async function open(name: PageName, mode: ModeName): Promise<Page> {
  const spec = PAGES[name];
  if (tab === undefined || tab.isClosed() || !browser?.isConnected() || documents % DOCUMENTS_PER_BROWSER === 0) await relaunch();
  documents += 1;
  const page = tab!;
  await page.setViewportSize(spec.viewport);
  await page.emulateMedia({ ...MODES[mode], reducedMotion: "reduce" });
  await page.goto(spec.url());
  // The desktop workbench replays its transcript first: check it once the replay has settled.
  const ready = name === "gallery" ? "main" : name === "workbench" ? "[data-testid=transcript-synced]" : "[data-testid=phone]";
  await page.locator(ready).first().waitFor({ timeout: 10_000 });
  if (SEEDED !== undefined && SEEDS[SEEDED]!.page === name) await inject(page, SEEDS[SEEDED]!);
  await page.evaluate(installChecks);
  return page;
}

/** Runs `rule`'s check on `page`, returning findings the allow-list does not cover. */
async function check_(page: Page, name: PageName, rule: Rule): Promise<Finding[]> {
  const entries = ALLOWED.map((entry) => (entry.pages.includes(name) ? entry : { rule: "none", selector: "*" }));
  const result = await page.evaluate(
    ([rule, options, entries]) => (globalThis as unknown as { __hig: { run: Function } }).__hig.run(rule, options, entries),
    [rule, { controlMin: PAGES[name].controlMin, desktop: PAGES[name].desktop }, entries] as const,
  );
  for (const index of result.used as number[]) usedAllowances.add(index);
  return result.findings as Finding[];
}

const RULES: readonly Rule[] = ["palette", "typography", "contrast", "hit-size", "grid", "materials", "motion", "keyboard", "structure"];

/** Each rule's findings on every page in every mode, as `line` -> where it was seen. */
const survey = new Map<Rule, Map<string, Set<string>>>(RULES.map((rule) => [rule, new Map()]));

/**
 * Opens each page in each mode in turn and runs every rule's check (and
 * axe-core, under contrast) on it.
 */
async function surveyAll(): Promise<void> {
  for (const name of Object.keys(PAGES) as PageName[]) {
    for (const mode of Object.keys(MODES) as ModeName[]) {
      const page = await open(name, mode);
      const add = (rule: Rule, line: string): void => {
        const lines = survey.get(rule)!;
        lines.set(line, (lines.get(line) ?? new Set()).add(`${name}/${mode}`));
      };
      // Each finding names the selector an allow-list entry would use.
      for (const rule of RULES) for (const finding of await check_(page, name, rule)) add(rule, `${finding.where}: ${finding.detail} (${finding.selector})`);
      for (const line of await axe(page, name, mode)) add("contrast", line);
    }
  }
}

/** `rule`'s findings: one line each, naming where it was seen (a page alone: in every mode). */
function everywhere(rule: Rule): string[] {
  const modes = Object.keys(MODES).length;
  return [...survey.get(rule)!].map(([line, seen]) => {
    const at = [...seen];
    const names = [...new Set(at.map((each) => each.split("/")[0]!))];
    const short = names.flatMap((name) => (at.filter((each) => each.startsWith(`${name}/`)).length === modes ? [name] : at.filter((each) => each.startsWith(`${name}/`))));
    return `${line} [${short.join(", ")}]`;
  });
}

let axeSource = "";

const STALLED = Symbol("stalled");

/**
 * Runs `work` for at most `ms`. On a stall the browser is discarded (the
 * next `open` launches a new one), and the losing promise's rejection is
 * swallowed, so it can never surface in a later navigation.
 */
async function bounded<T>(ms: number, work: () => Promise<T>): Promise<T | typeof STALLED> {
  const running = work();
  running.catch(() => undefined);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const stalled = new Promise<typeof STALLED>((resolve) => {
    timer = setTimeout(() => resolve(STALLED), ms);
  });
  const outcome = await Promise.race([running, stalled]);
  clearTimeout(timer);
  if (outcome === STALLED) {
    // The next `open` starts a fresh browser.
    tab = undefined;
  }
  return outcome;
}

/** axe-core's WCAG 2.x A and AA rules on `page`; a stalled run is a finding. */
async function axe(page: Page, name: PageName, mode: ModeName): Promise<string[]> {
  const outcome = await bounded(AXE_LIMIT, async () => {
    await page.evaluate(axeSource);
    return page.evaluate(async () => {
      const { axe } = globalThis as unknown as { axe: { run: (o: object) => Promise<{ violations: { id: string; nodes: { target: string[] }[] }[] }> } };
      const result = await axe.run({ runOnly: { type: "tag", values: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"] } });
      return result.violations.flatMap((v) => v.nodes.map((node) => `axe ${v.id}: ${node.target.join(" ")}`));
    });
  });
  return outcome === STALLED ? [`axe did not finish within ${AXE_LIMIT} ms [${name}/${mode}]`] : outcome;
}

type Hig = { __hig: Record<string, (...args: never[]) => unknown> };
const hig = <T,>(page: Page, fn: string, arg?: unknown): Promise<T> =>
  page.evaluate(([fn, arg]) => ((globalThis as unknown as Hig).__hig[fn as string] as (a: unknown) => T)(arg), [fn, arg] as const);

type Target = { readonly id: number; readonly where: string };

/** Tab through `page`: the controls never reached, and the ones focused without a ring. */
async function tabWalk(page: Page): Promise<{ unreached: string[]; ringless: string[] }> {
  const targets = await hig<Target[]>(page, "tabTargets");
  const seen = new Set<number>();
  const ringless: string[] = [];
  for (let step = 0; step < targets.length * 2 + 4; step++) {
    await page.keyboard.press("Tab");
    const focus = await hig<(Target & { ring: boolean }) | null>(page, "focused");
    if (focus === null) continue;
    if (!focus.ring && !seen.has(focus.id)) ringless.push(`${focus.where}: focused with no visible focus ring`);
    seen.add(focus.id);
  }
  return { unreached: targets.filter((target) => !seen.has(target.id)).map((target) => `${target.where}: not reachable with Tab`), ringless };
}

/** ArrowDown moves the selection of every list widget on screen; one with no focusable target fails. */
async function listArrows(page: Page): Promise<string[]> {
  const out: string[] = [];
  for (const list of await hig<Target[]>(page, "lists")) {
    if (!(await hig<boolean>(page, "focusList", list.id))) {
      out.push(`${list.where}: no focusable target for the arrow keys`);
      continue;
    }
    const before = await hig<string>(page, "selection");
    await page.keyboard.press("ArrowDown");
    if ((await hig<string>(page, "selection")) === before) out.push(`${list.where}: ArrowDown does not move the selection`);
  }
  return out;
}

/** Opens the palette with ⌘K; false when no dialog took focus. */
async function openPalette(page: Page): Promise<boolean> {
  await page.keyboard.press("ControlOrMeta+k");
  return hig<boolean>(page, "dialogOpen");
}

/** The checks that drive the page, each on a fresh document. */
const DRIVEN: Readonly<Record<Driven, (page: Page, name: PageName) => Promise<string[]>>> = {
  "tab-reach": async (page) => (await tabWalk(page)).unreached,
  "focus-ring": async (page) => (await tabWalk(page)).ringless,
  // The lists on screen, and the palette's once ⌘K opens it.
  arrows: async (page, name) => {
    const out = await listArrows(page);
    if (PAGES[name].palette && (await openPalette(page))) out.push(...(await listArrows(page)));
    return out;
  },
  // ⌘K opens the palette from wherever focus is, and Esc returns focus there.
  shortcut: async (page, name) => {
    if (!PAGES[name].palette) return [];
    const before = await hig<number | null>(page, "focusFirst");
    if (before === null) return ["no control to focus before ⌘K"];
    if (!(await openPalette(page))) return ["⌘K does not open the palette"];
    await page.keyboard.press("Escape");
    if (await hig<boolean>(page, "dialogOpen")) return ["Esc does not close the palette"];
    return (await hig<number | null>(page, "focusedId")) === before ? [] : ["Esc does not return focus to the control that had it"];
  },
  // Every toolbar has actions, and each one, disabled or not, is a ⌘K command.
  parity: async (page) => {
    const bars = await hig<{ where: string; actions: string[] }[]>(page, "toolbars");
    if (bars.length === 0) return [];
    const out = bars.filter((bar) => bar.actions.length === 0).map((bar) => `${bar.where}: a toolbar with no actions`);
    const actions = [...new Set(bars.flatMap((bar) => bar.actions))];
    if (!(await openPalette(page))) return [...out, ...actions.map((action) => `toolbar action "${action}": no ⌘K palette on this screen`)];
    const commands = await hig<string[]>(page, "commands");
    for (const action of actions) if (!commands.some((command) => command.startsWith(action))) out.push(`toolbar action "${action}" has no ⌘K command`);
    return out;
  },
};

/** `check` on a fresh document at `name` in light mode, with `seed` injected if given. */
async function probe(check: Check, name: PageName, seed?: Seed): Promise<string[]> {
  const page = await open(name, "light");
  if (seed !== undefined) await inject(page, seed);
  if (check in DRIVEN) return DRIVEN[check as Driven](page, name);
  return (await check_(page, name, check as Rule)).map((finding) => `${finding.where}: ${finding.detail}`);
}

beforeAll(async () => {
  await Promise.all([buildClient("fixture", FIXTURE), buildClient("gallery", GALLERY)]);
  axeSource = await Bun.file(AXE).text();
  await surveyAll();
}, LIMIT * 2);

afterAll(async () => {
  const last = browser;
  browser = undefined;
  await bounded(AXE_LIMIT, async () => last?.close());
});

describe("design conformance", () => {
  test("1. palette: every colour is a token of the active mode", async () => {
    expect(everywhere("palette")).toEqual([]);
  }, LIMIT);

  test("2. typography: 13 px body, nothing under 10 px, no thin small text, the system font", async () => {
    expect(everywhere("typography")).toEqual([]);
  }, LIMIT);

  test("3. contrast: 4.5:1 for text, 3:1 for large text, and axe-core agrees", async () => {
    expect(everywhere("contrast")).toEqual([]);
  }, LIMIT);

  test("4. hit size: 28 px controls on desktop, 44 px on a phone, 4 px apart", async () => {
    expect(everywhere("hit-size")).toEqual([]);
  }, LIMIT);

  test("5. spacing grid: margins, padding and gaps on the 4 px grid", async () => {
    expect(everywhere("grid")).toEqual([]);
  }, LIMIT);

  test("6. materials: blur only in a toolbar, sidebar or popover", async () => {
    expect(everywhere("materials")).toEqual([]);
  }, LIMIT);

  test("7. motion: nothing but opacity moves under reduced motion", async () => {
    expect(everywhere("motion")).toEqual([]);
  }, LIMIT);

  /** `checks` on every page, each on a fresh light-mode document, labelled with the page. */
  // Each bounded: a stall is a finding naming the check and page, and the next check gets a fresh browser.
  const driven = async (checks: readonly Driven[]): Promise<string[]> => {
    const out: string[] = [];
    for (const name of Object.keys(PAGES) as PageName[]) {
      for (const each of checks) {
        const started = performance.now();
        const outcome = await bounded(PROBE_LIMIT, () => retryLost(`${each} on ${name}`, () => probe(each, name)));
        console.error(`conformance: ${each} on ${name} took ${Math.round(performance.now() - started)} ms`);
        if (outcome === STALLED) out.push(`${each} ${name}/light: stalled after ${PROBE_LIMIT} ms`);
        else for (const line of outcome) out.push(`${each} ${name}/light: ${line}`);
      }
    }
    return out;
  };

  test("8. keyboard: Tab reaches every control with a ring, controls are named, arrows and ⌘K work", async () => {
    expect([...everywhere("keyboard"), ...(await driven(["tab-reach", "focus-ring", "arrows", "shortcut"]))]).toEqual([]);
  }, LIMIT);

  test("9. structure: a sidebar two levels deep, toolbar actions in ⌘K, no appearance switch", async () => {
    expect([...everywhere("structure"), ...(await driven(["parity"]))]).toEqual([]);
  }, LIMIT);

  test("every check catches its seeded violation", async () => {
    const missed: string[] = [];
    for (const [label, seed] of Object.entries(SEEDS)) {
      // Bounded: a wedged page fails this seed and discards the tab instead of hanging the suite.
      const outcome = await bounded(SEED_LIMIT, () =>
        retryLost(`seed ${label}`, async () => {
          const clean = new Set(await probe(seed.check, seed.page));
          return (await probe(seed.check, seed.page, seed)).filter((line) => !clean.has(line));
        }),
      );
      console.error(`conformance: seed ${label} done`);
      if (outcome === STALLED) missed.push(`${label}: stalled after ${SEED_LIMIT} ms`);
      else if (outcome.length === 0) missed.push(`${label}: ${seed.check} found nothing new`);
    }
    expect(missed).toEqual([]);
  }, LIMIT * 2);

  test("the allow-list names one element selector per entry", () => {
    expect(ALLOWED.filter((entry) => /[*,]/.test(entry.selector) || /^(html|:root|#root)$/.test(entry.selector.trim())).map((entry) => entry.selector)).toEqual([]);
  });

  test("the allow-list only shrinks: nothing beyond origin/dev's copy, and every entry still matches", async () => {
    // The base is origin/dev; CONFORMANCE_ALLOW_BASE names another commit to prove the guard against.
    const ref = process.env.CONFORMANCE_ALLOW_BASE ?? "origin/dev";
    const base = Bun.spawnSync(["git", "show", `${ref}:web/client/e2e/conformance-allow.ts`], { cwd: CLIENT_ROOT });
    const stderr = base.stderr.toString();
    if (base.exitCode !== 0 && !/exists on disk, but not in|does not exist in/.test(stderr)) {
      // CI fetches origin/dev for this; a local checkout without it skips the comparison only.
      if (process.env.CI) throw new Error(`cannot read ${ref}'s allow-list: ${stderr.trim()}`);
      console.warn(`conformance: no ${ref} ref, the allow-list is not compared`);
    } else if (base.exitCode === 0) {
      const path = `${CLIENT_ROOT}/artifacts/conformance-allow-dev.ts`;
      await Bun.write(path, base.stdout);
      const { ALLOWED: before } = (await import(path)) as { ALLOWED: typeof ALLOWED };
      const grown = ALLOWED.filter((entry) => {
        const old = before.find((each) => each.rule === entry.rule && each.selector === entry.selector);
        return old === undefined || entry.pages.some((page) => !old.pages.includes(page));
      });
      expect(grown.map((entry) => `${entry.rule} ${entry.selector} [${entry.pages.join(", ")}]`)).toEqual([]);
    }
    if (SEEDED === undefined) expect(ALLOWED.filter((_, index) => !usedAllowances.has(index)).map((entry) => `${entry.rule} ${entry.selector}`)).toEqual([]);
  });
});
