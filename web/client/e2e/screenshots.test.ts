/**
 * The owner preview at desktop and phone sizes (#1593, criterion 7).
 *
 * Builds the fixture page, opens it from a `file:` URL in Chromium (no
 * server of any kind), and checks: no request leaves the file, the replay
 * badge shows, the page never scrolls sideways, and every collapsible
 * summary is a touch target of at least 44 px. Screenshots land in
 * `artifacts/screenshots/` (uploaded by CI as `client-screenshots`).
 *
 * Needs Playwright's Chromium: `just client-browsers`. Runs in
 * `just client-check` (`bun test ./e2e`), not in the unit `bun test`.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdir } from "node:fs/promises";
import { pathToFileURL } from "node:url";

import { chromium, type Browser, type Page } from "playwright";

import { CLIENT_ROOT, buildClient } from "../tests/build";

const DIST = `${CLIENT_ROOT}/artifacts/fixture-dist`;
const SHOTS = `${CLIENT_ROOT}/artifacts/screenshots`;
const VIEWPORTS = [
  { name: "desktop-1280x800", width: 1280, height: 800 },
  { name: "phone-390x844", width: 390, height: 844 },
] as const;

let browser: Browser;

beforeAll(async () => {
  await buildClient("fixture", DIST);
  await mkdir(SHOTS, { recursive: true });
  browser = await chromium.launch();
});

afterAll(async () => {
  await browser?.close();
});

/** Fails if the page is wider than its viewport (a horizontal page scroll). */
async function expectNoHorizontalScroll(page: Page): Promise<void> {
  const widths = await page.evaluate(() => ({
    scroll: document.documentElement.scrollWidth,
    viewport: document.documentElement.clientWidth,
  }));
  expect(widths.scroll).toBeLessThanOrEqual(widths.viewport);
}

describe("fixture preview", () => {
  for (const viewport of VIEWPORTS) {
    test(`${viewport.name}: self-contained, readable, no sideways scroll`, async () => {
      const page = await browser.newPage({ viewport: { width: viewport.width, height: viewport.height } });
      const requests: string[] = [];
      page.on("request", (request) => requests.push(request.url()));
      const errors: string[] = [];
      page.on("pageerror", (error) => errors.push(error.message));

      await page.goto(pathToFileURL(`${DIST}/index.html`).href);
      await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });

      await expect(page.getByTestId("replay-badge").textContent()).resolves.toContain("Fixture replay");
      await expectNoHorizontalScroll(page);
      await page.screenshot({ path: `${SHOTS}/${viewport.name}.png`, fullPage: true });

      // Expanded, the JSON blocks hold the longest lines; they must scroll
      // inside their own block, not widen the page.
      await page.evaluate(() => document.querySelectorAll("details").forEach((details) => (details.open = true)));
      await expectNoHorizontalScroll(page);
      await page.screenshot({ path: `${SHOTS}/${viewport.name}-expanded.png`, fullPage: true });

      const summaries = await page.locator("summary").evaluateAll((nodes) =>
        nodes.map((node) => node.getBoundingClientRect()).map((box) => Math.min(box.width, box.height)),
      );
      expect(summaries.length).toBeGreaterThan(0);
      expect(Math.min(...summaries)).toBeGreaterThanOrEqual(44);

      const textSize = await page.locator(".text").first().evaluate((node) => parseFloat(getComputedStyle(node).fontSize));
      expect(textSize).toBeGreaterThanOrEqual(16);

      expect(requests.filter((url) => !url.startsWith("file:") && !url.startsWith("data:"))).toEqual([]);
      expect(errors).toEqual([]);
      await page.close();
    });
  }
});
