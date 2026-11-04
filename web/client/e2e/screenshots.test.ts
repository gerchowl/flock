/**
 * The owner preview: the §2a workbench at desktop and phone sizes (#1594).
 *
 * Builds the fixture page, opens it from a `file:` URL in Chromium (no
 * server of any kind), and checks: no request leaves the file, the replay
 * badge shows, the regions sit side by side, the page never scrolls
 * sideways, desktop text and controls are dense (13 px body, 28 to 32 px
 * single-line controls, #2903) while a phone's buttons and summaries are
 * touch targets of at least 44 px, the selection is emphasised only in the
 * active window, and the composer is one field. Screenshots land in
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
import { capturePreview } from "./capture";

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

/**
 * Desktop density (#2903): every button and summary is at least a 28 px
 * regular control, and the single-line ones, agent rows included, are no
 * taller than 32 px. A node's heading wraps its status line (#2930
 * flattens it), so it is only held to 28.
 */
async function expectDesktopDensity(page: Page): Promise<void> {
  const boxes = await page.locator("button:visible, summary:visible").evaluateAll((nodes) =>
    nodes.map((node) => ({ row: node.matches('[data-testid="group-node"] > .group__head'), box: node.getBoundingClientRect() })),
  );
  expect(boxes.length).toBeGreaterThan(0);
  expect(Math.min(...boxes.map(({ box }) => Math.min(box.width, box.height)))).toBeGreaterThanOrEqual(28);
  const single = boxes.filter(({ row }) => !row).map(({ box }) => box.height);
  expect(single.length).toBeGreaterThan(0);
  expect(Math.max(...single)).toBeLessThanOrEqual(32);
}

/** The colour a token resolves to in the page, as computed styles report it. */
async function tokenColour(page: Page, token: string): Promise<string> {
  return page.evaluate((name) => {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    const canvas = document.createElement("canvas").getContext("2d")!;
    canvas.fillStyle = value;
    const hex = canvas.fillStyle;
    const [r, g, b] = [1, 3, 5].map((at) => parseInt(hex.slice(at, at + 2), 16));
    return `rgb(${r}, ${g}, ${b})`;
  }, token);
}

async function bodyTextSize(page: Page): Promise<number> {
  return page.evaluate(() => parseFloat(getComputedStyle(document.body).fontSize));
}

/** Pins the artifact to the requested viewport, not just valid DOM geometry. */
async function capture(page: Page, name: string): Promise<void> {
  const image = await capturePreview(page, `${SHOTS}/${name}.png`);
  expect(image.subarray(0, 8)).toEqual(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  expect({ width: image.readUInt32BE(16), height: image.readUInt32BE(20) }).toEqual(page.viewportSize()!);
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
    await expectDesktopDensity(page);
    expect(await bodyTextSize(page)).toBe(13);
    await capture(page, "workbench-desktop-1280x800");

    expect(seen.requests.filter((url) => !url.startsWith("file:") && !url.startsWith("data:"))).toEqual([]);
    expect(seen.errors).toEqual([]);
    await page.close();
  });

  test("desktop: the selected agent is the accent in the active window, unemphasised in an inactive one", async () => {
    const page = await browser.newPage({ viewport: DESKTOP });
    await page.goto(pathToFileURL(`${DIST}/index.html`).href);
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    const selected = page.locator('[data-testid="agent-row"][aria-current="true"]');
    const colours = (): Promise<{ background: string; color: string }> =>
      selected.evaluate((row) => ({ background: getComputedStyle(row).backgroundColor, color: getComputedStyle(row).color }));
    await page.evaluate(() => window.dispatchEvent(new FocusEvent("focus")));
    expect(await colours()).toEqual({ background: await tokenColour(page, "--color-selection-active"), color: await tokenColour(page, "--color-label-on-selection") });
    await page.evaluate(() => window.dispatchEvent(new FocusEvent("blur")));
    expect(await colours()).toEqual({ background: await tokenColour(page, "--color-selection-inactive"), color: await tokenColour(page, "--color-label-primary") });
    await page.close();
  });

  test("desktop: the composer is one field, the send icon inside it at the trailing edge, and no button below it", async () => {
    const page = await browser.newPage({ viewport: DESKTOP });
    await page.goto(pathToFileURL(`${DIST}/index.html`).href);
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    const [field, input, send] = await Promise.all(["composer-field", "composer", "send"].map((id) => page.getByTestId(id).boundingBox()));
    const text = await page.getByTestId("composer").locator("textarea").boundingBox();
    expect([field, input, send, text].every((box) => box !== null)).toBe(true);
    // The send button is inside the field, after the text area on the same line, and nothing is a button below the field.
    expect(send!.x).toBeGreaterThanOrEqual(text!.x + text!.width);
    expect(send!.x + send!.width).toBeLessThanOrEqual(field!.x + field!.width);
    expect(send!.y + send!.height).toBeLessThanOrEqual(field!.y + field!.height);
    expect(send!.y).toBeGreaterThanOrEqual(field!.y);
    await expect(page.getByTestId("composer").locator("button").count()).resolves.toBe(1);
    await expect(page.getByRole("button", { name: "Send" }).count()).resolves.toBe(1);
    await page.close();
  });

  test("desktop: an agent row keeps its name and activity visible, however long either is, and describes the full activity", async () => {
    const page = await browser.newPage({ viewport: DESKTOP });
    await page.goto(pathToFileURL(`${DIST}/index.html`).href);
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    const row = page.getByTestId("agent-row").first();
    const activity = row.locator(".agent-row__activity");
    const full = (await activity.textContent()) ?? "";
    expect(full.length).toBeGreaterThan(0);
    // The row's accessible description, as Chromium's accessibility tree has it, holds the full activity.
    const cdp = await page.context().newCDPSession(page);
    const { root } = await cdp.send("DOM.getDocument", { depth: 0 });
    const { nodeId } = await cdp.send("DOM.querySelector", { nodeId: root.nodeId, selector: '[data-testid="agent-row"]' });
    const { nodes } = await cdp.send("Accessibility.getPartialAXTree", { nodeId, fetchRelatives: false });
    expect(String(nodes[0]?.description?.value ?? "")).toContain(full);
    await cdp.detach();
    const widths = (): Promise<{ name: number; activity: number; row: number }> =>
      row.evaluate((button) => ({
        name: button.querySelector(".agent-row__name")!.getBoundingClientRect().width,
        activity: button.querySelector(".agent-row__activity")!.getBoundingClientRect().width,
        row: button.getBoundingClientRect().height,
      }));
    const long = "a-very-long-agent-name-that-never-ends-".repeat(4);
    await row.locator(".agent-row__name").evaluate((el, text) => void (el.textContent = text), long);
    const longName = await widths();
    expect([longName.name > 0, longName.activity > 0, longName.row <= 32]).toEqual([true, true, true]);
    await row.locator(".agent-row__name").evaluate((el) => void (el.textContent = "fixture"));
    await activity.evaluate((el, text) => void (el.textContent = text), long);
    const longActivity = await widths();
    expect([longActivity.name > 0, longActivity.activity > 0, longActivity.row <= 32]).toEqual([true, true, true]);
    await page.close();
  });

  test("?layout=phone on a wide window keeps 44 px touch targets on the agent screen", async () => {
    const page = await browser.newPage({ viewport: DESKTOP });
    await page.goto(`${pathToFileURL(`${DIST}/index.html`).href}?layout=phone`);
    await page.getByTestId("inbox-headline").waitFor({ timeout: 10_000 });
    await page.getByRole("button", { name: "Agents" }).click();
    await expectTouchTargets(page);
    await page.getByTestId("agent-row").first().click();
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    await expectTouchTargets(page);
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
    // A sample card's answers are not built, so they show only with ?preview.
    await expect(page.getByTestId("inbox-card").locator("button").count()).resolves.toBe(0);
    await capture(page, "workbench-phone-inbox-390x844");

    await page.getByRole("button", { name: "Agents" }).click();
    await page.getByTestId("agent-row").first().click();
    await page.getByTestId("transcript-synced").waitFor({ timeout: 10_000 });
    await expectNoHorizontalScroll(page);
    await expectTouchTargets(page);
    // The page itself never scrolls: the back control and the tab bar stay on screen.
    expect(await page.evaluate(() => document.documentElement.scrollHeight)).toBeLessThanOrEqual(PHONE.height);
    expect(await page.getByRole("button", { name: "Back" }).isVisible()).toBe(true);
    await capture(page, "workbench-phone-agent-390x844");

    // A production build ignores ?preview: unbuilt controls are only for development builds (ADR-0048 D7).
    await page.goto(`${pathToFileURL(`${DIST}/index.html`).href}?preview`);
    await page.getByTestId("inbox-headline").waitFor({ timeout: 10_000 });
    await expect(page.getByTestId("inbox-card").locator("button").count()).resolves.toBe(0);
    await expect(page.locator(".waits, [aria-disabled]").count()).resolves.toBe(0);
    await expect(page.getByRole("button", { name: "Activity" }).count()).resolves.toBe(0);

    expect(seen.requests.filter((url) => !url.startsWith("file:") && !url.startsWith("data:"))).toEqual([]);
    expect(seen.errors).toEqual([]);
    await page.close();
  });
});
