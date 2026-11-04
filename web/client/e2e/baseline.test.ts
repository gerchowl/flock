/**
 * The exemplar baseline's measurements (#2899, `web/baseline/`): `collect`
 * and `summarise` read a page whose every value is known, and the committed
 * results keep counts only, never a colour. The crawl itself runs by hand
 * (`web/baseline/crawl.ts`), never here.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";

import { chromium, type Browser, type Page } from "playwright";

import { collect, focusStyle, summarise, type Summary } from "../../baseline/collect";

const RESULTS = `${import.meta.dir}/../../baseline/results/baseline.json`;
const VIEWPORT = { width: 640, height: 400 } as const;

// Every value is chosen: two 13 px texts and one 17 px, 8 and 6 px padding,
// a 6 px radius, 28 and 44 px controls, 24 px rows, one text colour and
// one background.
const KNOWN = `<!doctype html><html><head><style>
  body { margin: 0; font: 13px/16px sans-serif; color: rgb(10, 10, 10); background: rgb(250, 250, 250); }
  p { margin: 0; padding: 8px; }
  h1 { margin: 0; padding: 6px; font-size: 17px; line-height: normal; }
  button { all: unset; display: block; block-size: 28px; border-radius: 6px; }
  #ring:focus { outline: 2px solid rgb(0, 0, 200); outline-offset: 1px; }
  #glow:focus { outline: none; box-shadow: 0 0 0 3px rgb(0, 0, 200); }
  #tall { block-size: 44px; }
  ul { margin: 0; padding: 0; list-style: none; }
  li { block-size: 24px; }
</style></head><body>
  <h1>Title</h1><p>One</p><p>Two</p>
  <button id="ring">Ring</button><button id="glow">Glow</button><button id="tall">Tall</button>
  <ul><li></li><li></li></ul>
</body></html>`;

let browser: Browser;
let page: Page;

beforeAll(async () => {
  browser = await chromium.launch();
  page = await browser.newPage({ viewport: VIEWPORT });
});

afterAll(async () => {
  await browser.close();
});

async function measure(html: string): Promise<Summary> {
  await page.setContent(html);
  const samples = await page.evaluate(collect, 1_000);
  const focus: (string | null)[] = [];
  for (let index = 0; index < 3; index += 1) {
    await page.keyboard.press("Tab");
    focus.push(await page.evaluate(focusStyle));
  }
  const png = await page.screenshot();
  return summarise(samples, focus, { bytes: png.length, pixels: VIEWPORT.width * VIEWPORT.height });
}

test("measures a page whose every value is known", async () => {
  const summary = await measure(KNOWN);
  expect(summary.fontSizes.values).toEqual([["13", 5], ["17", 1]]);
  expect(summary.lineHeights.values).toEqual([["16", 5], ["normal", 1]]);
  // Four sides of each padded element: 8 px on two paragraphs, 6 px on the title.
  expect(summary.spacing.values).toEqual([["8", 8], ["6", 4]]);
  expect(summary.spacing.onGrid4).toBe(0.67);
  expect(summary.spacing.onGrid8).toBe(0.67);
  expect(summary.colours).toEqual({ text: 1, fill: 1, border: 0, total: 2 });
  expect(summary.radii.values).toEqual([["6px", 12]]);
  expect(summary.controlHeights.values).toEqual([["28", 2], ["44", 1]]);
  expect(summary.rowHeights.values).toEqual([["24", 2]]);
  // The third button resets its outline and draws no ring.
  expect(summary.focus.values).toEqual([["box-shadow", 1], ["none", 1], ["outline 2px offset 1px", 1]]);
}, 20_000);

test("alignment counts distinct left edges and complexity grows with clutter", async () => {
  const aligned = await measure(`<body style="margin:0">${"<p style='margin:0'>Row</p>".repeat(10)}</body>`);
  const scattered = await measure(`<body style="margin:0">${[...Array(10).keys()].map((index) => `<p style="margin:0 0 0 ${index * 13}px">Row</p>`).join("")}</body>`);
  expect(aligned.alignment).toBe(10);
  expect(scattered.alignment).toBe(100);
  const blank = await measure("<body></body>");
  const cluttered = await measure(`<body style="margin:0;font:9px monospace">${"Lorem ipsum dolor sit amet ".repeat(400)}</body>`);
  expect(cluttered.complexity).toBeGreaterThan(blank.complexity * 5);
}, 20_000);

test("the committed results keep counts, never a colour", async () => {
  const text = await Bun.file(RESULTS).text();
  expect(text).not.toMatch(/#[0-9a-f]{3,8}\b|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\(/i);
  const { results } = JSON.parse(text) as { results: { site: string; page: string; mode: string }[] };
  for (const site of ["flock", "icloud", "linear", "vercel", "github", "primer", "stripe"]) {
    expect(results.filter((result) => result.site === site).map((result) => result.mode)).toContain("dark");
  }
});
