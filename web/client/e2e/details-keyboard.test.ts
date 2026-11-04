/**
 * The Details disclosure behind plain-language refusals (#2902) works from
 * the keyboard in a real browser: Tab reaches its summary from the control
 * before it, Enter opens it and Space closes it. happy-dom does not activate
 * `<summary>` from the keyboard, so this runs on the built gallery.
 */
import { afterAll, beforeAll, expect, test } from "bun:test";
import { pathToFileURL } from "node:url";

import { chromium, type Browser } from "playwright";

import { CLIENT_ROOT, buildClient } from "../tests/build";

const OUT = `${CLIENT_ROOT}/artifacts/details-gallery`;

let browser: Browser;

beforeAll(async () => {
  await buildClient("gallery", OUT);
  browser = await chromium.launch();
}, 120_000);

afterAll(async () => {
  await browser.close();
});

test("Tab reaches the error state's Details after Retry; Enter opens it and Space closes it", async () => {
  const page = await browser.newPage();
  await page.goto(pathToFileURL(`${OUT}/gallery.html`).href);
  const state = page.getByTestId("error-state");
  const details = state.getByTestId("details");
  const open = (): Promise<boolean> => details.evaluate((element) => (element as HTMLDetailsElement).open);
  await state.getByTestId("retry").focus();
  await page.keyboard.press("Tab");
  expect(await details.locator("summary").evaluate((summary) => summary === document.activeElement)).toBe(true);
  expect(await open()).toBe(false);
  await page.keyboard.press("Enter");
  expect(await open()).toBe(true);
  expect(await details.locator(".details__body").isVisible()).toBe(true);
  await page.keyboard.press("Space");
  expect(await open()).toBe(false);
  await page.close();
}, 30_000);
