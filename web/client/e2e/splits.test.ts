import { afterAll, beforeAll, expect, test } from "bun:test";
import { chromium, type Browser, type Page } from "playwright";
import { buildClient, CLIENT_ROOT } from "../tests/build";
import { guardKeys, expectNoKeyListeners } from "./key-guard";

const OUT = `${CLIENT_ROOT}/artifacts/splits-dist`;
let browser: Browser;
const errors = new WeakMap<Page, string[]>();
beforeAll(async () => { await buildClient("fixture", OUT); browser = await chromium.launch(); }, 120_000);
afterAll(async () => { await browser?.close(); });
async function open(): Promise<Page> {
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const seen: string[] = []; errors.set(page, seen); page.on("pageerror", error => seen.push(error.message));
  page.setDefaultTimeout(1500);
  await guardKeys(page);
  await page.route("**/*", route => route.fulfill({ path: `${OUT}${new URL(route.request().url()).pathname === "/" ? "/index.html" : new URL(route.request().url()).pathname}` }));
  await page.goto("http://split.test/");
  await page.getByTestId("workbench").waitFor();
  return page;
}
const region = (page: Page, name: string) => page.locator(`.l-split[data-region="${name}"]`);
const separator = (page: Page, name: string) => page.getByRole("separator", { name: `${name} width`, exact: true });
async function done(page: Page): Promise<void> { await expectNoKeyListeners(page); expect(errors.get(page)).toEqual([]); await page.close(); }

for (const [name, widths] of [["Sidebar", [192, 224, 272]], ["Inspector", [240, 280, 336]]] as const) {
  test(`${name}: measured presets, keyboard stepping, name, value and focus ring`, async () => {
    const page = await open(), split = separator(page, name);
    await split.focus();
    await page.keyboard.press("ArrowLeft");
    for (let index = 0; index < widths.length; index++) {
      expect(await page.getByTestId(name === "Sidebar" ? "sidebar" : "inbox").evaluate(el => el.getBoundingClientRect().width)).toBe(widths[index]!);
      expect(await split.getAttribute("aria-valuenow")).toBe(String(index + 1));
      expect(await split.getAttribute("aria-valuetext")).toBe(["narrow", "default", "wide"][index]!);
      expect(await split.evaluate(el => getComputedStyle(el).outlineStyle)).not.toBe("none");
      await page.keyboard.press("ArrowRight");
    }
    await done(page);
  }, 30_000);
  test(`${name}: snap on release and cancel without saving`, async () => {
    const page = await open(), split = separator(page, name), pane = region(page, name.toLowerCase());
    const box = (await split.boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + 60);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 + (name === "Sidebar" ? 45 : -50), box.y + 60);
    expect(await pane.getAttribute("data-split")).toBe("default");
    expect(await pane.getAttribute("data-target")).toBe("wide");
    await page.mouse.up();
    expect(await pane.getAttribute("data-split")).toBe("wide");
    await page.reload();
    expect(await pane.getAttribute("data-split")).toBe("wide");
    const restored = (await split.boundingBox())!;
    await page.mouse.move(restored.x + restored.width / 2, restored.y + 60); await page.mouse.down();
    await page.mouse.move(restored.x + restored.width / 2 + (name === "Sidebar" ? -100 : 100), restored.y + 60);
    expect(await pane.getAttribute("data-target")).toBe("narrow");
    await split.dispatchEvent("pointercancel", { pointerId: 1 });
    expect(await pane.getAttribute("data-target")).toBeNull();
    expect(await pane.getAttribute("data-split")).toBe("wide");
    await page.mouse.up();
    expect(await pane.getAttribute("data-split")).toBe("wide");
    await done(page);
  }, 30_000);
  test(`${name}: Escape cancels an active drag before release`, async () => {
    const page = await open(), split = separator(page, name), pane = region(page, name.toLowerCase());
    const box = (await split.boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + 60);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2 + (name === "Sidebar" ? 45 : -50), box.y + 60);
    expect(await pane.getAttribute("data-target")).toBe("wide");
    await page.keyboard.press("Escape");
    const captured = await split.evaluate(element => element.hasPointerCapture(1));
    const target = await pane.getAttribute("data-target");
    await page.mouse.up();
    expect(await pane.getAttribute("data-split")).toBe("default");
    expect(target).toBeNull();
    expect(captured).toBe(false);
    await page.reload();
    expect(await pane.getAttribute("data-split")).toBe("default");
    await split.focus(); await page.keyboard.press("ArrowRight");
    expect(await pane.getAttribute("data-split")).toBe("wide");
    await done(page);
  }, 30_000);
  test(`${name}: toolbar, shortcut, palette, Enter, double click and reload restore`, async () => {
    const page = await open(), split = separator(page, name), pane = region(page, name.toLowerCase());
    await split.focus(); await page.keyboard.press("ArrowRight");
    await page.keyboard.press("?");
    expect(await page.getByRole("dialog", { name: "Keyboard shortcuts" }).textContent()).toContain(`Toggle ${name.toLowerCase()}`);
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: `Toggle ${name.toLowerCase()}`, exact: true }).click();
    expect(await pane.getAttribute("data-collapsed")).toBe("true");
    expect(await page.getByTestId(name === "Sidebar" ? "sidebar" : "inbox").isVisible()).toBe(false);
    expect(await split.getAttribute("aria-valuenow")).toBe("0");
    await page.reload();
    expect(await pane.getAttribute("data-collapsed")).toBe("true");
    const apple = await page.evaluate(() => /Mac|iPhone|iPad|iPod/.test(navigator.platform));
    await page.keyboard.press(name === "Sidebar" ? (apple ? "Control+Meta+s" : "Control+s") : "Alt+ControlOrMeta+i");
    expect(await pane.getAttribute("data-collapsed")).toBe("false");
    expect(await page.getByTestId(name === "Sidebar" ? "sidebar" : "inbox").isVisible()).toBe(true);
    expect(await pane.getAttribute("data-split")).toBe("wide");
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByRole("dialog", { name: "Command palette" }).getByRole("option", { name: new RegExp(`Toggle ${name.toLowerCase()}`) }).click();
    expect(await pane.getAttribute("data-collapsed")).toBe("true");
    await split.focus(); await page.keyboard.press("Enter");
    expect(await pane.getAttribute("data-collapsed")).toBe("false");
    await split.dblclick();
    expect(await pane.getAttribute("data-collapsed")).toBe("true");
    await done(page);
  }, 30_000);
}

test("old layouts and unknown region values retain tiles and raw future data", async () => {
  const page = await open();
  await page.getByRole("button", { name: "Toggle sidebar", exact: true }).click();
  await page.evaluate(() => {
    const saved = JSON.parse(localStorage.getItem("flock.layout:fixture")!);
    saved.layout.regions = { sidebar: { preset: "wide", collapsed: false }, inspector: { preset: "future", collapsed: "invalid", future: 42 }, future: { size: 99 } };
    localStorage.setItem("flock.layout:fixture", JSON.stringify(saved));
  });
  await page.reload();
  expect(await region(page, "sidebar").getAttribute("data-split")).toBe("wide");
  expect(await region(page, "inspector").getAttribute("data-split")).toBe("default");
  expect(await page.getByTestId("layout-notice").count()).toBe(0);
  await page.getByRole("button", { name: "Toggle sidebar", exact: true }).click();
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem("flock.layout:fixture")!).layout.regions)).toEqual({ sidebar: { preset: "wide", collapsed: true }, inspector: { preset: "future", collapsed: "invalid", future: 42 }, future: { size: 99 } });
  await done(page);
}, 30_000);

test("Compact uses an accessible icon rail, restores the chosen width, and leaves phone navigation intact", async () => {
  const page = await open();
  await separator(page, "Sidebar").focus(); await page.keyboard.press("ArrowRight");
  await page.setViewportSize({ width: 1000, height: 800 });
  const rail = page.getByTestId("compact-rail");
  await rail.waitFor({ state: "visible" });
  expect(await rail.evaluate(el => el.getBoundingClientRect().width)).toBe(48);
  const first = page.getByTestId("compact-agent-row").first();
  const label = (await first.getAttribute("title"))!;
  expect(await page.getByRole("button", { name: label, exact: true }).count()).toBe(1);
  await first.click();
  expect(await first.getAttribute("aria-current")).toBe("true");
  await page.keyboard.press("ArrowDown");
  expect(await page.getByTestId("compact-agent-row").nth(1).getAttribute("aria-current")).toBe("true");
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(1000);
  await page.setViewportSize({ width: 1280, height: 800 });
  expect(await page.getByTestId("sidebar").evaluate(el => el.getBoundingClientRect().width)).toBe(272);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByTestId("workbench").waitFor({ state: "detached" });
  expect(await page.getByTestId("workbench").count()).toBe(0);
  expect(await page.getByRole("navigation", { name: "Sections" }).getByRole("button", { name: "Inbox", exact: true }).count()).toBe(1);
  await done(page);
}, 30_000);
