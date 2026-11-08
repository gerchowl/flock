/**
 * The exemplar crawl (#2899): loads each page in `PAGES` in Chromium, light
 * and dark, measures it (`collect.ts`) and runs Project Wallace's DTCG token
 * extraction on the CSS it loaded, then writes `results/baseline.json`.
 *
 * Run by hand, never in CI: `bun install && bun crawl.ts` here (needs
 * Playwright's Chromium, `just client-browsers`). `--only <site>` limits the
 * run. `--storage-state <file>` loads a Playwright storage state, so the owner
 * can measure the logged-in apps with their own test accounts on an
 * occasional manual run; such results are not committed. The CSS itself
 * stays in memory: only counts reach the results file.
 */
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

import { css_to_tokens } from "@projectwallace/css-design-tokens";
import { chromium, type Browser, type Page } from "playwright";

import { collect, focusStyle, summarise, type Summary } from "./collect";

/** Public pages only (the issue): marketing and docs pages, sign-in screens and public product pages. */
const PAGES: readonly { site: string; page: string; url: string }[] = [
  { site: "icloud", page: "home", url: "https://www.icloud.com/" },
  { site: "linear", page: "home", url: "https://linear.app/" },
  { site: "linear", page: "docs", url: "https://linear.app/docs" },
  { site: "linear", page: "login", url: "https://linear.app/login" },
  { site: "vercel", page: "docs", url: "https://vercel.com/docs" },
  { site: "vercel", page: "login", url: "https://vercel.com/login" },
  { site: "github", page: "repo", url: "https://github.com/gerchowl/flock" },
  { site: "github", page: "issues", url: "https://github.com/gerchowl/flock/issues" },
  { site: "primer", page: "button", url: "https://primer.style/product/components/button/" },
  { site: "stripe", page: "docs", url: "https://docs.stripe.com/" },
  { site: "stripe", page: "login", url: "https://dashboard.stripe.com/login" },
];

const MODES = ["light", "dark"] as const;
const VIEWPORT = { width: 1280, height: 800 } as const;
const ELEMENT_LIMIT = 20_000;
const TABS = 20;
const LOAD_LIMIT = 30_000;
const STEP_LIMIT = 15_000;
const CLIENT = join(import.meta.dir, "../client");

/** `work`, or a rejection naming `stage` once `limit` passes: a page that stalls is reported, never waited on. */
function bounded<T>(stage: string, work: Promise<T>, limit = STEP_LIMIT): Promise<T> {
  let timer: Timer | undefined;
  const late = new Promise<never>((_, reject) => { timer = setTimeout(() => reject(new Error(`${stage} took over ${limit} ms`)), limit); });
  return Promise.race([work, late]).finally(() => clearTimeout(timer));
}

/** Wallace's DTCG output for the page's CSS, as a count of tokens per type. */
export function wallace(css: string): Record<string, number> {
  const tokens = css_to_tokens(css);
  return Object.fromEntries(Object.entries(tokens).map(([type, entries]) => [type, Object.keys(entries).length]));
}

/** Builds the flock client in `mode` (the gallery or the fixture workbench) and returns its index page. */
async function buildFlock(mode: "gallery" | "fixture"): Promise<string> {
  const out = await mkdtemp(join(tmpdir(), `baseline-${mode}-`));
  const child = Bun.spawn([process.execPath, "x", "vite", "build", "--mode", mode, "--outDir", out, "--emptyOutDir", "--logLevel", "error"], { cwd: CLIENT, stderr: "inherit" });
  if ((await child.exited) !== 0) throw new Error(`vite build --mode ${mode} failed`);
  return pathToFileURL(join(out, mode === "gallery" ? "gallery.html" : "index.html")).href;
}

/** Every stylesheet the page loaded or carries inline, concatenated. */
async function stylesheets(page: Page, loaded: readonly Promise<string>[]): Promise<string> {
  const inline = await page.evaluate(() => [...document.querySelectorAll("style")].map((style) => style.textContent ?? ""));
  // A file: page's linked sheets raise no response events: read them from disk.
  const linked = page.url().startsWith("file:")
    ? await Promise.all((await page.evaluate(() => [...document.querySelectorAll<HTMLLinkElement>("link[rel=stylesheet]")].map((link) => link.href))).map((href) => readFile(new URL(href), "utf8")))
    : [];
  return [...(await Promise.all(loaded)), ...linked, ...inline].join("\n");
}

type Result = { site: string; page: string; url: string; mode: string; status: number | null; summary?: Summary; wallace?: Record<string, number>; error?: string };

const PAGE_LIMIT = 120_000;

/**
 * Measures `url` in a context of its own on the shared `browser`. Every step
 * has a deadline, and so does the whole page: a stalled page fails with the
 * step it reached, and its context is closed (or left to the browser's own
 * close at the end of the run).
 */
async function measure(browser: Browser, url: string, mode: (typeof MODES)[number], storageState: string | undefined): Promise<Omit<Result, "site" | "page" | "url" | "mode">> {
  let stage = "context";
  const context = await bounded(stage, browser.newContext({ viewport: VIEWPORT, colorScheme: mode, reducedMotion: "reduce", ...(storageState === undefined ? {} : { storageState }) }));
  const run = async () => {
    const page = await bounded((stage = "page"), context.newPage());
    const loaded: Promise<string>[] = [];
    page.on("response", (response) => {
      if (response.request().resourceType() === "stylesheet") loaded.push(bounded("stylesheet", response.text()).catch(() => ""));
    });
    stage = "load";
    const response = await page.goto(url, { waitUntil: "load", timeout: LOAD_LIMIT });
    // Bounded: a page that keeps polling never reaches network idle, and is measured as it stands.
    await page.waitForLoadState("networkidle", { timeout: 10_000 }).catch(() => undefined);
    await bounded((stage = "fonts"), page.evaluate(() => document.fonts.ready));
    const png = await bounded((stage = "screenshot"), page.screenshot({ timeout: STEP_LIMIT }));
    const samples = await bounded((stage = "collect"), page.evaluate(collect, ELEMENT_LIMIT), 60_000);
    const focus: (string | null)[] = [];
    for (let index = 0; index < TABS; index += 1) {
      await bounded((stage = "tab"), page.keyboard.press("Tab"));
      focus.push(await bounded((stage = "focus"), page.evaluate(focusStyle)));
    }
    const summary = summarise(samples, focus, { bytes: png.length, pixels: VIEWPORT.width * VIEWPORT.height });
    const css = await bounded((stage = "stylesheets"), stylesheets(page, loaded));
    stage = "wallace";
    return { status: response?.status() ?? null, summary, wallace: wallace(css) };
  };
  try {
    return await bounded("page", run(), PAGE_LIMIT).catch((error: unknown) => {
      throw new Error(`${String(error).split("\n")[0]} (at ${stage})`);
    });
  } finally {
    await bounded("context close", context.close()).catch((error: unknown) => process.stderr.write(`${String(error)}\n`));
  }
}

if (import.meta.main) {
  const { values } = parseArgs({ options: { only: { type: "string" }, "storage-state": { type: "string" } } });
  const flock = [
    { site: "flock", page: "gallery", url: await buildFlock("gallery") },
    { site: "flock", page: "workbench", url: await buildFlock("fixture") },
  ];
  const results: Result[] = [];
  let browser = await bounded("launch", chromium.launch());
  for (const target of [...flock, ...PAGES].filter((each) => values.only === undefined || each.site === values.only)) {
    for (const mode of MODES) {
      process.stderr.write(`${target.site}/${target.page} ${mode}\n`);
      if (!browser.isConnected()) browser = await bounded("launch", chromium.launch());
      // flock's pages are temporary builds: the results name them, not their paths.
      const named = { ...target, url: target.site === "flock" ? `flock:${target.page}` : target.url, mode };
      try {
        results.push({ ...named, ...(await measure(browser, target.url, mode, values["storage-state"])) });
      } catch (error) {
        results.push({ ...named, status: null, error: String(error).split("\n")[0]! });
        process.stderr.write(`  ${String(error).split("\n")[0]}\n`);
      }
    }
  }
  await bounded("close", browser.close()).catch((error: unknown) => process.stderr.write(`${String(error)}\n`));
  await writeFile(join(import.meta.dir, "results/baseline.json"), `${JSON.stringify({ measured: new Date().toISOString().slice(0, 10), viewport: VIEWPORT, results }, null, 1)}\n`);
}
