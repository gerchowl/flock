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
 * `SUITE_SEEDS` break the suite itself (a broad or unbaselined allow-list
 * entry, a dropped mode, a cleanup that never settles), and each fails the run.
 *
 * Nothing passes by running less: the coverage test fails unless every page
 * ran in every required mode, as the page's own media queries report it, and
 * each walk covered the page's minimum of elements and controls. The driven
 * keyboard and structure checks run in every mode too. A cleanup that times
 * out fails, and the browser is killed and reaped either way.
 *
 * Needs Playwright's Chromium: `just client-browsers`. Runs in
 * `just client-check` (`bun test ./e2e`).
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

import { chromium, type Browser, type CDPSession, type Page } from "playwright";

import { CLIENT_ROOT, buildClient } from "../tests/build";
import { ALLOWED as LISTED, type Allowance, type PageName } from "./conformance-allow";
import { BASELINE } from "./conformance-allow-baseline";
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
const CLOSE_LIMIT = 5_000;

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

/**
 * The page × mode matrix that must have run, stated apart from `PAGES` and
 * `MODES` and compared against the media each page itself reports, so a
 * dropped page or mode fails instead of shrinking the run.
 */
const REQUIRED_PAGES: readonly PageName[] = ["gallery", "workbench", "workbench-phone", "workbench-forced-phone"];
const REQUIRED_MEDIA: readonly string[] = ["light/no-preference/reduce", "dark/no-preference/reduce", "light/more/reduce", "dark/more/reduce"];

/**
 * The fewest rendered elements and controls a page's walk may cover in any
 * mode: about nine tenths of what each renders today (43 and 14, 330 and
 * 68, 41 and 10), so hiding part of a page fails. Raise them as pages grow.
 */
const MIN_COVERAGE: Readonly<Record<PageName, { elements: number; controls: number }>> = {
  gallery: { elements: 39, controls: 12 },
  workbench: { elements: 300, controls: 60 },
  "workbench-phone": { elements: 37, controls: 9 },
  "workbench-forced-phone": { elements: 37, controls: 9 },
};

/** The keyboard and structure checks that drive the page from the test, beside the in-page rules. */
type Driven = "tab-reach" | "focus-ring" | "arrows" | "shortcut" | "parity" | "coverage";
type Check = Rule | Driven;

/**
 * A violation `check` must catch: markup appended to `into` on `page`, and a
 * script run there, in `mode` (light when not given).
 */
type Seed = { readonly check: Check; readonly page: PageName; readonly into: string; readonly html: string; readonly script?: string; readonly mode?: ModeName };

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
  "focus-ring-dark": {
    check: "focus-ring",
    page: "gallery",
    mode: "dark",
    into: "main",
    html: '<style data-testid="seed">@media (prefers-color-scheme: dark) { :focus { outline: none !important; box-shadow: none !important; } }</style>',
  },
  "coverage-hidden": {
    check: "coverage",
    page: "gallery",
    into: "body",
    html: "",
    script: 'for (const child of document.body.children) child.style.display = "none"',
  },
};

/**
 * Violations of the suite itself rather than of a page, proven only through
 * `CONFORMANCE_SEED`: each makes the run fail.
 */
const SUITE_SEEDS = {
  // A grid entry whose selector exempts every element of its pages.
  "allow-broad": "the selector guard refuses it",
  // An entry gains a page while origin/dev has no allow-list to compare against.
  "allow-unbaselined": "the shrink guard compares against the bootstrap baseline",
  // Dark mode emulates light, so the dark half of the matrix never runs.
  "mode-dropped": "the coverage test misses the dark media",
  // Closing the browser never settles.
  "teardown-stall": "the cleanup fails and the browser is killed and reaped",
} as const;

const SEEDED = process.env.CONFORMANCE_SEED;
if (SEEDED !== undefined && !(SEEDED in SEEDS) && !(SEEDED in SUITE_SEEDS)) throw new Error(`CONFORMANCE_SEED=${SEEDED} names no seed`);

/** The allow-list this run applies: `conformance-allow.ts`, unless a suite seed alters it. */
const ALLOWED: readonly Allowance[] = LISTED.map((entry, index) => {
  if (SEEDED === "allow-broad" && index === LISTED.findIndex((each) => each.rule === "grid")) return { ...entry, selector: ":is(body) :not(nonexistent-review-tag)" };
  if (SEEDED === "allow-unbaselined" && index === 0) return { ...entry, pages: [...entry.pages, "gallery"] };
  return entry;
});

// One compound (a tag, classes, ids or an exact test id) and at most two more down a descendant chain.
const COMPOUND = String.raw`(?:[a-z][a-z0-9-]*)?(?:\.[A-Za-z_][\w-]*|#[A-Za-z_][\w-]*|\[data-testid="[\w-]+"\])*`;
const SIMPLE = new RegExp(`^${COMPOUND}(?:(?: > | )${COMPOUND}){0,2}$`);

/** Why an allow-list selector is refused, or null when it names specific elements. */
function refusal(selector: string): string | null {
  if (!SIMPLE.test(selector)) return "not one simple compound and a short descendant chain";
  const parts = selector.split(/ > | /);
  if (parts.some((part) => part === "")) return "an empty compound";
  // `body` alone names the one body element (the body text size); as a root it would name every element.
  if (parts.some((part) => /^(html|:root)\b|#root\b/.test(part))) return "rooted at html or #root";
  if (parts.some((part) => /^body\b/.test(part)) && selector !== "body") return "rooted at body";
  return null;
}

let browser: Browser | undefined;
const usedAllowances = new Set<number>();

async function inject(page: Page, seed: Seed): Promise<void> {
  await page.evaluate(({ into, html }) => document.querySelector(into)!.insertAdjacentHTML("beforeend", html), seed);
  if (seed.script !== undefined) await page.evaluate(seed.script);
}

let tab: Page | undefined;

/**
 * What the suite is doing now, and why the tab became unusable, if it did.
 * There is one tab for the whole run (Chromium on the Linux CI runner wedged
 * when pages were opened and closed, #2931). A stall, a crash or a lost
 * connection is a failure naming the check and stage; nothing relaunches or
 * retries, and every later check fails at once with the same reason.
 */
let stage = "launch";
let broken: string | null = null;
const at = (now: string): void => void (stage = now);

async function launch(): Promise<void> {
  browser = await chromium.launch();
  browser.on("disconnected", () => (broken ??= `the browser disconnected during ${stage}`));
  tab = await browser.newPage();
  // axe-core and the checks reach the browser once, as init scripts every document runs, instead
  // of half a megabyte through the pipe per document (the #2931 stalls followed that traffic).
  await tab.addInitScript({ content: `${axeSource}\n;globalThis.__installHig = ${installChecks.toString()};` });
  tab.on("crash", () => (broken ??= `the page crashed during ${stage}`));
  tab.on("close", () => (broken ??= `the tab closed during ${stage}`));
  // Diagnosis only (#2931): with the debugger on, a stall can ask the renderer where it is.
  debug = await tab.context().newCDPSession(tab);
  await debug.send("Debugger.enable");
  host = await browser.newBrowserCDPSession();
}

let debug: CDPSession | undefined;
let host: CDPSession | undefined;

/**
 * After a stall, asks the browser process (its own CDP session) for its
 * targets and each process's CPU time, twice a second apart: an answer means
 * the browser and the pipe to it are alive, and the deltas show which
 * process is busy or blocked.
 */
async function processes(): Promise<string> {
  const session = host;
  if (session === undefined) return "no browser session";
  type Info = { processInfo: { type: string; id: number; cpuTime: number }[] };
  const sample = async (): Promise<Info> => (await session.send("SystemInfo.getProcessInfo")) as unknown as Info;
  const first = await sample();
  await new Promise((resolve) => setTimeout(resolve, 1_000));
  const second = await sample();
  const { targetInfos } = (await session.send("Target.getTargets")) as unknown as { targetInfos: { type: string; url: string; attached: boolean }[] };
  const cpu = second.processInfo.map((now) => {
    const before = first.processInfo.find((each) => each.id === now.id);
    return `${now.type}#${now.id} +${before === undefined ? "new" : (now.cpuTime - before.cpuTime).toFixed(2)}s`;
  });
  return `processes ${cpu.join(", ")}; targets ${targetInfos.map((t) => `${t.type} ${t.url.split("/").pop()}${t.attached ? "" : " (detached)"}`).join(", ")}`;
}

type Paused = { callFrames: { functionName: string; url: string; location: { scriptId: string; lineNumber: number; columnNumber: number } }[] };

/**
 * After a stall, pauses the renderer and logs where its main thread is: the
 * top frames and the code around the innermost one (the bundle is minified).
 * No answer within 5 s means the renderer, or the pipe to it, is not the one
 * looping in script. It only reports; the run has already failed.
 */
async function diagnose(): Promise<void> {
  const session = debug;
  if (session === undefined) return;
  const report = (async (): Promise<string> => {
    const paused = new Promise<Paused>((resolve) => session.once("Debugger.paused", (event) => resolve(event as unknown as Paused)));
    await session.send("Debugger.pause");
    const { callFrames } = await paused;
    const top = callFrames[0];
    let around = "";
    if (top !== undefined) {
      const { scriptSource } = (await session.send("Debugger.getScriptSource", { scriptId: top.location.scriptId })) as { scriptSource: string };
      const line = scriptSource.split("\n")[top.location.lineNumber] ?? "";
      around = line.slice(Math.max(0, top.location.columnNumber - 160), top.location.columnNumber + 160);
    }
    const frames = callFrames.slice(0, 10).map((frame) => `${frame.functionName || "(anonymous)"}@${frame.location.lineNumber + 1}:${frame.location.columnNumber}`);
    return `stack ${frames.join(" <- ")}; code: ${around}`;
  })();
  report.catch(() => undefined);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const silent = new Promise<string>((resolve) => {
    timer = setTimeout(() => resolve("no answer from the renderer within 5 s"), 5_000);
  });
  const found = await Promise.race([report.catch((error: Error) => `the pause failed: ${error.message}`), silent]);
  clearTimeout(timer);
  console.error(`conformance: stalled during ${stage}; renderer: ${found}`);
  const browserSide = processes().catch((error: Error) => `the browser query failed: ${error.message}`);
  const quiet = new Promise<string>((resolve) => {
    timer = setTimeout(() => resolve("no answer from the browser process within 5 s"), 5_000);
  });
  console.error(`conformance: browser: ${await Promise.race([browserSide, quiet])}`);
  clearTimeout(timer);
  // The OS view: each Chromium and Bun process's state and the kernel call it waits in
  // (a browser parked in pipe_write would mean this side stopped reading the pipe).
  if (process.platform === "linux") {
    const ps = Bun.spawnSync(["ps", "-eo", "pid,ppid,stat,pcpu,wchan:24,args"], { stdout: "pipe" }).stdout.toString();
    const rows = ps.split("\n").filter((row, index) => index === 0 || /chrom|bun/.test(row)).map((row) => row.slice(0, 160));
    // The browser process is the Chromium child of this test process.
    const browserPid = ps.split("\n").map((row) => row.trim().split(/\s+/)).find((cols) => cols[1] === String(process.pid) && /chrom/.test(cols.slice(5).join(" ")))?.[0];
    console.error(`conformance: processes (test pid ${process.pid}, browser pid ${browserPid ?? "?"}):\n${rows.join("\n")}`);
    for (const pid of [String(process.pid), browserPid]) {
      if (pid === undefined) continue;
      const fds = Bun.spawnSync(["sh", "-c", `for f in /proc/${pid}/fd/*; do printf '%s -> %s\n' "$f" "$(readlink "$f")"; done | grep -E '/fd/[0-4] |pipe|socket'`], { stdout: "pipe" }).stdout.toString();
      console.error(`conformance: stdio, pipes and sockets of ${pid}:\n${fds}`);
    }
  }
}

/** A fresh document at `name` in `mode`, with the checks installed (and the env seed, if any). */
async function open(name: PageName, mode: ModeName): Promise<Page> {
  const spec = PAGES[name];
  if (broken !== null) throw new Error(`the tab is unusable: ${broken}`);
  const page = tab!;
  at(`opening ${name}/${mode}: viewport and media`);
  await page.setViewportSize(spec.viewport);
  await page.emulateMedia({ ...MODES[SEEDED === "mode-dropped" && mode === "dark" ? "light" : mode], reducedMotion: "reduce" });
  at(`opening ${name}/${mode}: navigating`);
  await page.goto(spec.url());
  at(`opening ${name}/${mode}: waiting for the page`);
  // The desktop workbench replays its transcript first: check it once the replay has settled.
  const ready = name === "gallery" ? "main" : name === "workbench" ? "[data-testid=transcript-synced]" : "[data-testid=phone]";
  await page.locator(ready).first().waitFor({ timeout: 10_000 });
  at(`opening ${name}/${mode}: installing the checks`);
  const seed = SEEDED === undefined ? undefined : SEEDS[SEEDED];
  if (seed !== undefined && seed.page === name) await inject(page, seed);
  await page.evaluate(() => (globalThis as unknown as { __installHig: () => void }).__installHig());
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

type Coverage = { elements: number; controls: number; scheme: string; contrast: string; reduced: boolean };

/** What each page's walk covered in each mode, and the media the page reported. */
const walked: { name: PageName; mode: ModeName; coverage: Coverage }[] = [];

const mediaOf = (seen: Coverage): string => `${seen.scheme}/${seen.contrast}/${seen.reduced ? "reduce" : "no-preference"}`;

/** Where a walk of `name` covered fewer elements or controls than the page has. */
function shortfall(name: PageName, seen: Coverage): string[] {
  const min = MIN_COVERAGE[name];
  const out: string[] = [];
  if (seen.elements < min.elements) out.push(`walked ${seen.elements} elements, under ${min.elements}`);
  if (seen.controls < min.controls) out.push(`walked ${seen.controls} controls, under ${min.controls}`);
  return out;
}

/**
 * Opens each page in each mode in turn and runs every rule's check (and
 * axe-core, under contrast) on it.
 */
async function surveyAll(): Promise<void> {
  for (const name of Object.keys(PAGES) as PageName[]) {
    for (const mode of Object.keys(MODES) as ModeName[]) {
      const page = await open(name, mode);
      const coverage = await hig<Coverage>(page, "coverage");
      walked.push({ name, mode, coverage });
      console.error(`conformance: ${name}/${mode} (${mediaOf(coverage)}) walks ${coverage.elements} elements, ${coverage.controls} controls`);
      const add = (rule: Rule, line: string): void => {
        const lines = survey.get(rule)!;
        lines.set(line, (lines.get(line) ?? new Set()).add(`${name}/${mode}`));
      };
      // Each finding names the selector an allow-list entry would use.
      for (const rule of RULES) for (const finding of (at(`${rule} on ${name}/${mode}`), await check_(page, name, rule))) add(rule, `${finding.where}: ${finding.detail} (${finding.selector})`);
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
 * Runs `work` for at most `ms`. A stall marks the tab unusable (every later
 * check then fails at once with the stage it stalled at), and the losing
 * promise's rejection is swallowed, so it can never surface later.
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
  if (outcome === STALLED && broken === null) {
    broken = `stalled after ${ms} ms during ${stage}`;
    await diagnose();
  }
  return outcome;
}

/** axe-core's WCAG 2.x A and AA rules on `page`; a stalled run is a finding. */
async function axe(page: Page, name: PageName, mode: ModeName): Promise<string[]> {
  at(`axe on ${name}/${mode}`);
  const outcome = await bounded(AXE_LIMIT, async () => {
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
  const base = stage;
  let last = "the page";
  for (let step = 0; step < targets.length * 2 + 4; step++) {
    at(`${base}: Tab ${step + 1} from ${last}`);
    await page.keyboard.press("Tab");
    const focus = await hig<(Target & { ring: boolean }) | null>(page, "focused");
    if (focus === null) continue;
    last = focus.where;
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
  // The walk covers the page: it renders at least its minimum of elements and controls.
  coverage: async (page, name) => shortfall(name, await hig<Coverage>(page, "coverage")),
};

/** `check` on a fresh document at `name` in `mode`, with `seed` injected if given. */
async function probe(check: Check, name: PageName, mode: ModeName, seed?: Seed): Promise<string[]> {
  const page = await open(name, mode);
  if (seed !== undefined) await inject(page, seed);
  at(`${check} on ${name}/${mode}${seed === undefined ? "" : " (seeded)"}`);
  if (check in DRIVEN) return DRIVEN[check as Driven](page, name);
  return (await check_(page, name, check as Rule)).map((finding) => `${finding.where}: ${finding.detail}`);
}

beforeAll(async () => {
  await Promise.all([buildClient("fixture", FIXTURE), buildClient("gallery", GALLERY)]);
  axeSource = await Bun.file(AXE).text();
  await launch();
  await surveyAll();
}, LIMIT * 2);

/**
 * Kills every process this test process started that is still alive (the
 * browser and its helpers) and waits until each is gone, reaped by its
 * parent. Returns what it had to kill; throws if a process outlives it.
 */
async function reap(): Promise<string[]> {
  const tree = (): { pid: number; ppid: number; state: string; args: string }[] =>
    Bun.spawnSync(["ps", "-A", "-o", "pid=,ppid=,stat=,args="], { stdout: "pipe" })
      .stdout.toString()
      .split("\n")
      .map((row) => row.trim().match(/^(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/))
      .flatMap((cols) => (cols === null ? [] : [{ pid: Number(cols[1]), ppid: Number(cols[2]), state: cols[3]!, args: cols[4]! }]));
  // The browsers this process launched, and everything under them.
  const ours = (): { pid: number; state: string; args: string }[] => {
    const rows = tree();
    const mine = new Set(rows.filter((row) => row.ppid === process.pid && /chrom|headless_shell/i.test(row.args)).map((row) => row.pid));
    // Parents precede children only by chance, so repeat until the set stops growing.
    for (let size = 0; size !== mine.size; ) {
      size = mine.size;
      for (const row of rows) if (mine.has(row.ppid)) mine.add(row.pid);
    }
    return rows.filter((row) => mine.has(row.pid));
  };
  const found = ours();
  for (const row of found) {
    try {
      process.kill(row.pid, "SIGKILL");
    } catch {
      // It exited between the listing and the kill.
    }
  }
  // Gone means reaped: a zombie still has its row until its parent collects it.
  const pids = new Set(found.map((row) => row.pid));
  const alive = (): { pid: number; state: string }[] => tree().filter((row) => pids.has(row.pid));
  for (const started = performance.now(); alive().length > 0; ) {
    if (performance.now() - started > CLOSE_LIMIT) throw new Error(`processes outlived the run: ${alive().map((row) => `${row.pid} (${row.state})`).join(", ")}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return found.map((row) => `${row.pid} ${row.args.slice(0, 60)}`);
}

afterAll(async () => {
  const last = browser;
  browser = undefined;
  at("closing the browser");
  // A cleanup that never settles fails the run, and nothing the run started outlives it.
  const closing = SEEDED === "teardown-stall" ? new Promise<void>(() => undefined) : last?.close();
  const outcome = await bounded(CLOSE_LIMIT, async () => closing);
  const killed = await reap();
  if (outcome === STALLED) throw new Error(`closing the browser stalled after ${CLOSE_LIMIT} ms; killed and reaped ${killed.length} processes: ${killed.join("; ")}`);
  if (killed.length > 0) console.error(`conformance: killed ${killed.length} processes left after the browser closed: ${killed.join("; ")}`);
  // Room for the close, the stall diagnosis and the reaping, so the hook's own timeout never preempts them.
}, LIMIT);

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

  /** `checks` on every page in every mode, each on a fresh document, labelled with the page and mode. */
  // Each bounded: a stall, a crash or an error is a finding naming the check, page and stage.
  const driven = async (checks: readonly Driven[]): Promise<string[]> => {
    const out: string[] = [];
    for (const name of Object.keys(PAGES) as PageName[]) {
      for (const mode of Object.keys(MODES) as ModeName[]) {
        for (const each of checks) {
          const started = performance.now();
          const outcome = await bounded(PROBE_LIMIT, () => probe(each, name, mode)).catch((error: Error) => new Error(`${error.message} (during ${stage})`));
          console.error(`conformance: ${each} on ${name}/${mode} took ${Math.round(performance.now() - started)} ms`);
          if (outcome === STALLED) out.push(`${each} ${name}/${mode}: stalled after ${PROBE_LIMIT} ms during ${stage}`);
          else if (outcome instanceof Error) out.push(`${each} ${name}/${mode}: ${outcome.message}`);
          else for (const line of outcome) out.push(`${each} ${name}/${mode}: ${line}`);
        }
      }
    }
    return out;
  };

  test("8. keyboard: Tab reaches every control with a ring, controls are named, arrows and ⌘K work", async () => {
    expect([...everywhere("keyboard"), ...(await driven(["tab-reach", "focus-ring", "arrows", "shortcut"]))]).toEqual([]);
  }, LIMIT * 4);

  test("9. structure: a sidebar two levels deep, toolbar actions in ⌘K, no appearance switch", async () => {
    expect([...everywhere("structure"), ...(await driven(["parity"]))]).toEqual([]);
  }, LIMIT * 2);

  test("coverage: every page ran in every mode, and each walk covered its page", () => {
    const ran = new Set(walked.map((each) => `${each.name} ${mediaOf(each.coverage)}`));
    const missing = REQUIRED_PAGES.flatMap((name) => REQUIRED_MEDIA.filter((media) => !ran.has(`${name} ${media}`)).map((media) => `${name} ${media}: never ran`));
    const thin = walked.flatMap((each) => shortfall(each.name, each.coverage).map((line) => `${each.name}/${each.mode}: ${line}`));
    expect([...missing, ...thin]).toEqual([]);
  });

  test("every check catches its seeded violation", async () => {
    const missed: string[] = [];
    for (const [label, seed] of Object.entries(SEEDS)) {
      const mode = seed.mode ?? "light";
      // Bounded: a wedged page fails this seed and discards the tab instead of hanging the suite.
      const outcome = await bounded(SEED_LIMIT, async () => {
        const clean = new Set(await probe(seed.check, seed.page, mode));
        return (await probe(seed.check, seed.page, mode, seed)).filter((line) => !clean.has(line));
      }).catch((error: Error) => new Error(`${error.message} (during ${stage})`));
      console.error(`conformance: seed ${label} done`);
      if (outcome === STALLED) missed.push(`${label}: stalled after ${SEED_LIMIT} ms during ${stage}`);
      else if (outcome instanceof Error) missed.push(`${label}: ${outcome.message}`);
      else if (outcome.length === 0) missed.push(`${label}: ${seed.check} found nothing new`);
    }
    expect(missed).toEqual([]);
  }, LIMIT * 2);

  test("the allow-list names specific elements: one simple selector per entry", () => {
    expect(ALLOWED.flatMap((entry) => (refusal(entry.selector) === null ? [] : [`${entry.rule} ${entry.selector}: ${refusal(entry.selector)}`]))).toEqual([]);
  });

  test("the selector guard refuses broad selectors", () => {
    const broad = [":is(body) :not(nonexistent-review-tag)", "*", "div *", ".a, .b", ':where(.a)', ".a:not(.b)", "[class]", '[class*="a"]', '[data-testid^="a"]', "body .a", "body > .a", "html", ":root", "#root", "#root .a", ".a .b .c .d"];
    expect(broad.filter((selector) => refusal(selector) === null)).toEqual([]);
    expect(["body", ".a", '[data-testid="a"] > .b', "header .a .b", "li.a#b"].filter((selector) => refusal(selector) !== null)).toEqual([]);
  });

  test("the allow-list only shrinks: nothing beyond origin/dev's copy or the bootstrap baseline, and every entry still matches", async () => {
    const key = (entry: { rule: string; selector: string }): string => `${entry.rule} ${entry.selector}`;
    const grownFrom = (before: readonly { rule: string; selector: string; pages: readonly string[] }[]): string[] =>
      ALLOWED.filter((entry) => {
        const old = before.find((each) => key(each) === key(entry));
        return old === undefined || entry.pages.some((page) => !old.pages.includes(page));
      }).map((entry) => `${key(entry)} [${entry.pages.join(", ")}]`);
    // The checked-in baseline always applies, so the guard compares even before dev has an allow-list.
    expect(grownFrom(BASELINE).map((line) => `beyond the bootstrap baseline: ${line}`)).toEqual([]);
    // The base is origin/dev; CONFORMANCE_ALLOW_BASE names another commit to prove the guard against.
    const ref = process.env.CONFORMANCE_ALLOW_BASE ?? "origin/dev";
    const base = Bun.spawnSync(["git", "show", `${ref}:web/client/e2e/conformance-allow.ts`], { cwd: CLIENT_ROOT });
    const stderr = base.stderr.toString();
    if (base.exitCode === 0) {
      const path = `${CLIENT_ROOT}/artifacts/conformance-allow-dev.ts`;
      await Bun.write(path, base.stdout);
      const { ALLOWED: before } = (await import(path)) as { ALLOWED: typeof ALLOWED };
      expect(grownFrom(before).map((line) => `beyond ${ref}: ${line}`)).toEqual([]);
    } else if (!/exists on disk, but not in|does not exist in/.test(stderr)) {
      // CI fetches origin/dev for this; a local checkout without it compares with the baseline only.
      if (process.env.CI) throw new Error(`cannot read ${ref}'s allow-list: ${stderr.trim()}`);
      console.warn(`conformance: no ${ref} ref, the allow-list is compared with the bootstrap baseline only`);
    }
    if (SEEDED === undefined) expect(ALLOWED.filter((_, index) => !usedAllowances.has(index)).map(key)).toEqual([]);
  });
});
