/**
 * The owner preview: the §2a workbench at desktop and phone sizes (#1594).
 *
 * Builds the fixture page, opens it from a `file:` URL in Chromium (no
 * server of any kind), and checks: no request leaves the file, the replay
 * badge shows, the regions sit side by side, the page never scrolls
 * sideways, body text is 14 px, and every button and summary is a touch
 * target of at least 44 px. Screenshots land in
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
const DESKTOP = { width: 1280, height: 800 } as const;
const PHONE = { width: 390, height: 844 } as const;

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

/** The page's open failures: requests leaving the file, and page errors. */
function watch(page: Page): { requests: string[]; errors: string[] } {
  const seen = { requests: [] as string[], errors: [] as string[] };
  page.on("request", (request) => seen.requests.push(request.url()));
  page.on("pageerror", (error) => seen.errors.push(error.message));
  return seen;
}

/** Every button and summary is a touch target of at least 44 px. */
async function expectTouchTargets(page: Page): Promise<void> {
  const sizes = await page.locator("button:visible, summary:visible").evaluateAll((nodes) =>
    nodes.map((node) => node.getBoundingClientRect()).map((box) => Math.min(box.width, box.height)),
  );
  expect(sizes.length).toBeGreaterThan(0);
  expect(Math.min(...sizes)).toBeGreaterThanOrEqual(44);
}

async function bodyTextSize(page: Page): Promise<number> {
  return page.evaluate(() => parseFloat(getComputedStyle(document.body).fontSize));
}

describe("fixture preview", () => {
  test("desktop 1280x800: top bar, sidebar, focus pane and inbox side by side", async () => {
    const page = await browser.newPage({ viewport: DESKTOP });
    const seen = watch(page);
    await page.goto(pathToFileURL(`${DIST}/index.html`).href);
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });

    await expect(page.getByTestId("replay-badge").textContent()).resolves.toContain("Fixture replay");
    const boxes = await Promise.all(["sidebar", "focus-pane", "inbox"].map((id) => page.getByTestId(id).boundingBox()));
    const [side, focus, inbox] = boxes.map((box) => box ?? { x: 0, y: 0, width: 0, height: 0 });
    expect(side!.width).toBeGreaterThan(0);
    expect(side!.x + side!.width).toBeLessThanOrEqual(focus!.x + 1);
    expect(focus!.x + focus!.width).toBeLessThanOrEqual(inbox!.x + 1);
    await expectNoHorizontalScroll(page);
    await expectTouchTargets(page);
    expect(await bodyTextSize(page)).toBe(14);
    await page.screenshot({ path: `${SHOTS}/workbench-desktop-1280x800.png` });

    expect(seen.requests.filter((url) => !url.startsWith("file:") && !url.startsWith("data:"))).toEqual([]);
    expect(seen.errors).toEqual([]);
    await page.close();
  });

  test("phone 390x844: the Inbox tab first, then an agent's screen", async () => {
    const page = await browser.newPage({ viewport: PHONE });
    const seen = watch(page);
    await page.goto(pathToFileURL(`${DIST}/index.html`).href);
    await page.getByTestId("inbox-headline").waitFor({ timeout: 10_000 });

    await expectNoHorizontalScroll(page);
    await expectTouchTargets(page);
    expect(await bodyTextSize(page)).toBe(15);
    const choice = await page.getByTestId("inbox-card").first().locator("button").first().boundingBox();
    expect(choice?.height).toBeGreaterThanOrEqual(48);
    expect(choice?.width).toBeGreaterThan(PHONE.width / 2);
    await page.screenshot({ path: `${SHOTS}/workbench-phone-inbox-390x844.png` });

    await page.getByRole("button", { name: "Agents" }).click();
    await page.getByTestId("agent-row").first().click();
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    await expectNoHorizontalScroll(page);
    await expectTouchTargets(page);
    // The page itself never scrolls: the back control and the tab bar stay on screen.
    expect(await page.evaluate(() => document.documentElement.scrollHeight)).toBeLessThanOrEqual(PHONE.height);
    expect(await page.getByRole("button", { name: "‹ Back" }).isVisible()).toBe(true);
    await page.screenshot({ path: `${SHOTS}/workbench-phone-agent-390x844.png` });

    expect(seen.requests.filter((url) => !url.startsWith("file:") && !url.startsWith("data:"))).toEqual([]);
    expect(seen.errors).toEqual([]);
    await page.close();
  });
});
