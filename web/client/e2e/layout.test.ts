/** Reuses G1's Playwright screenshot tool and fixture-build helper. */
import { beforeAll, afterAll, expect, test } from "bun:test";
import { mkdir } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { chromium, type Browser, type Page } from "playwright";
import { buildClient, CLIENT_ROOT } from "../tests/build";

const dist = `${CLIENT_ROOT}/artifacts/layout-dist`, shots = `${CLIENT_ROOT}/artifacts/screenshots`;
let browser: Browser;
beforeAll(async () => { await buildClient("fixture", dist); await mkdir(shots, { recursive: true }); browser = await chromium.launch(); });
afterAll(async () => { await browser?.close(); });
async function seed(page: Page) {
  await page.getByLabel("Tile kind", { exact: true }).selectOption("artifact");
  await page.getByLabel("Subject", { exact: true }).fill("artifact/example");
  await page.getByLabel("Placement", { exact: true }).selectOption("split_right");
  await page.getByRole("button", { name: "Open", exact: true }).click();
  await page.getByLabel("Tile kind", { exact: true }).selectOption("inbox");
  await page.getByLabel("Subject", { exact: true }).fill("inbox/example");
  await page.getByLabel("Placement", { exact: true }).selectOption("tab");
  await page.getByRole("button", { name: "Open", exact: true }).click();
  await page.getByRole("region", { name: "inbox: inbox/example", exact: true }).getByRole("button", { name: "Collapse", exact: true }).click();
}
async function geometry(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);
  const sizes = await page.locator("button, input, select").evaluateAll(nodes => nodes.filter(n => n.getBoundingClientRect().width > 0).map(n => { const r = n.getBoundingClientRect(); return Math.min(r.width, r.height); }));
  expect(Math.min(...sizes)).toBeGreaterThanOrEqual(44);
}
for (const [name, width, height] of [["desktop", 1280, 800], ["phone", 390, 844]] as const) {
  test(`${name}: layout, rail, geometry and push/back`, async () => {
    const page = await browser.newPage({ viewport: { width, height } });
    const errors: string[] = []; page.on("pageerror", e => errors.push(e.message));
    await page.goto(pathToFileURL(`${dist}/index.html`).href);
    await page.getByTestId("transcript-synced").waitFor();
    expect(await page.getByTestId("replay-badge").textContent()).toContain("Fixture replay");
    await seed(page); await geometry(page);
    expect(await page.locator(".wb-tile").count()).toBe(name === "phone" ? 1 : 2);
    await page.screenshot({ path: `${shots}/layout-${name}-${width}x${height}.png`, fullPage: false });
    if (name === "phone") {
      expect(await page.getByRole("navigation", { name: "Tile switcher" }).count()).toBe(1);
      await page.getByRole("button", { name: "Rail (1)" }).click();
      expect(await page.getByRole("dialog", { name: "Rail" }).count()).toBe(1);
      await geometry(page);
      await page.screenshot({ path: `${shots}/layout-phone-390x844-sheet.png`, fullPage: false });
      await page.getByRole("button", { name: "Close rail", exact: true }).click();
      const origin = await page.locator(".wb-tile").getAttribute("id");
      await page.getByRole("button", { name: "Expand into second", exact: true }).click();
      expect(await page.locator(".wb-tile").getAttribute("id")).not.toBe(origin);
      await page.getByRole("button", { name: "Back", exact: true }).click();
      await page.locator(`#${origin}`).waitFor();
    } else expect(await page.locator(".wb-desktop > aside .wb-chip").count()).toBe(1);
    await page.reload();
    expect(await page.locator(".wb-chip").count()).toBe(name === "phone" ? 0 : 1);
    expect(errors).toEqual([]);
    await page.close();
  });
}
