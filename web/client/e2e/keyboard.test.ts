/**
 * The keyboard model (#2904) and composition safety (#2967) in a real
 * browser: every shortcut of the keyboard layer (`src/workbench/keys.ts`)
 * driven with Playwright's keyboard on the harness page
 * (`e2e/keys/harness.tsx`), whose sender records what it is given.
 *
 * ⌘ is the platform's command key: Playwright's `ControlOrMeta` presses ⌘
 * on macOS and Ctrl elsewhere, as the layer reads it.
 *
 * Needs Playwright's Chromium: `just client-browsers`. Runs in
 * `just client-check`.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { chromium, type Browser, type Page } from "playwright";

import { SHORTCUTS } from "../src/workbench/keys";
import { CLIENT_ROOT } from "../tests/build";
import { expectNoKeyListeners, guardKeys, keyListeners } from "./key-guard";

const OUT = `${CLIENT_ROOT}/artifacts/keys-harness`;
const ORIGIN = "http://keys.test";
const LIMIT = 30_000;
const REQUEST = '[data-testid="inbox-card"][data-request-seq="7"]';
/** `KEYS_SEED=<seed>` loads that harness seed into every page, so every test's key guard fails (`e2e/keys/harness.tsx`). */
const RUN_SEED = process.env["KEYS_SEED"];

type Sent = { readonly verb: string; readonly [field: string]: unknown };

let browser: Browser;

beforeAll(async () => {
  const child = Bun.spawn([process.execPath, "x", "vite", "build", "--config", "e2e/keys/vite.config.ts", "--outDir", OUT, "--emptyOutDir", "--logLevel", "error"], {
    cwd: CLIENT_ROOT,
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, NODE_ENV: "production" },
  });
  const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
  if (code !== 0) throw new Error(`keyboard harness build exited ${code}: ${stderr}`);
  browser = await chromium.launch();
}, 120_000);

afterAll(async () => {
  await browser?.close();
});

/** The harness page (with `seed`, if given), served from the build by Playwright itself; no request leaves it. */
async function open(seed: string | null = RUN_SEED ?? null): Promise<Page> {
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await guardKeys(page);
  // A missing element fails its test well inside the test's limit, never the shared browser.
  page.setDefaultTimeout(5_000);
  await page.route("**/*", (route) => {
    const url = new URL(route.request().url());
    return url.origin === ORIGIN ? route.fulfill({ path: `${OUT}${url.pathname}` }) : route.abort();
  });
  await page.goto(`${ORIGIN}/e2e/keys/index.html${seed === null ? "" : `?seed=${seed}`}`);
  await page.getByTestId("agent-row").first().waitFor();
  return page;
}

/** Opens the first agent with the pointer, for the tests that are not about ↑/↓. */
const pickFirst = (page: Page): Promise<void> => page.getByTestId("agent-row").first().click();

/** Closes a test's page once its key guard recorded nothing: every test fails on a key listener outside the layer. */
async function done(page: Page): Promise<void> {
  await expectNoKeyListeners(page);
  await page.close();
}

const sent = (page: Page): Promise<Sent[]> => page.evaluate(() => [...Reflect.get(globalThis, "__sent")]);
/** The open agent's name in the focus pane, or null. */
const openAgent = async (page: Page): Promise<string | null> => ((await page.locator(".focus__name").count()) === 0 ? null : page.locator(".focus__name").textContent());
/** The sidebar's agents, each once, in the order it shows them. */
const agents = (page: Page): Promise<string[]> => page.evaluate(() => [...new Set([...document.querySelectorAll<HTMLElement>("[data-agent-id]")].map((row) => row.dataset["agentId"] ?? ""))]);
const activeTestId = (page: Page): Promise<string | null> => page.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? document.activeElement?.tagName.toLowerCase() ?? null);
const composer = (page: Page) => page.getByTestId("composer").locator("textarea");
const sheet = (page: Page) => page.getByRole("dialog", { name: "Keyboard shortcuts" });
const palette = (page: Page) => page.getByRole("dialog", { name: "Command palette" });

/**
 * Dispatches `key` (with the platform's ⌘ when `command`) on the element `selector` names, as if
 * focus had been forced there; returns whether anything prevented it.
 */
const dispatchOn = (page: Page, selector: string, key: string, extra: { command?: boolean; isComposing?: boolean; keyCode?: number } = {}): Promise<boolean> =>
  page.evaluate(
    ([selector, key, extra]) => {
      const apple = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
      const command = extra.command === true ? { metaKey: apple, ctrlKey: !apple } : {};
      const event = new KeyboardEvent("keydown", { key, ...command, bubbles: true, cancelable: true, isComposing: extra.isComposing === true });
      if (extra.keyCode !== undefined) Object.defineProperty(event, "keyCode", { value: extra.keyCode });
      document.querySelector(selector)?.dispatchEvent(event);
      return event.defaultPrevented;
    },
    [selector, key, extra] as const,
  );

describe("↑/↓ move through the agents", () => {
  test("from no focus: each agent once, in the sidebar's order, stopping at the ends", async () => {
    const page = await open();
    const order = await agents(page);
    expect(order.length).toBeGreaterThan(2);
    for (const agent of order) {
      await page.keyboard.press("ArrowDown");
      expect(await openAgent(page)).toBe(agent);
    }
    await page.keyboard.press("ArrowDown");
    expect(await openAgent(page)).toBe(order.at(-1) ?? null);
    await page.keyboard.press("ArrowUp");
    expect(await openAgent(page)).toBe(order.at(-2) ?? null);
    expect(await activeTestId(page)).toBe("body");
    await done(page);
  }, LIMIT);

  test("in the sidebar: focus moves to the next row and opens its agent", async () => {
    const page = await open();
    const rows = page.locator("[data-agent-id]");
    await rows.nth(0).focus();
    await page.keyboard.press("ArrowDown");
    expect(await rows.nth(1).evaluate((row) => row === document.activeElement)).toBe(true);
    expect(await openAgent(page)).toBe(await rows.nth(1).getAttribute("data-agent-id"));
    await page.keyboard.press("ArrowUp");
    expect(await rows.nth(0).evaluate((row) => row === document.activeElement)).toBe(true);
    await done(page);
  }, LIMIT);

  test("keep their meaning in a text field and on a card", async () => {
    const page = await open();
    await page.keyboard.press("ArrowDown");
    const first = await openAgent(page);
    await composer(page).fill("one\ntwo");
    await composer(page).press("ArrowUp");
    expect(await composer(page).evaluate((field: HTMLTextAreaElement) => field.selectionStart)).toBeLessThan(4);
    await composer(page).press("ArrowDown");
    await page.locator(REQUEST).focus();
    await page.keyboard.press("ArrowDown");
    expect(await openAgent(page)).toBe(first);
    await done(page);
  }, LIMIT);
});

describe("⌘K palette", () => {
  test("opens from the composer and runs the layer's commands, each with its keys", async () => {
    const page = await open();
    await pickFirst(page);
    const order = await agents(page);
    await composer(page).focus();
    await page.keyboard.press("ControlOrMeta+k");
    await palette(page).waitFor();
    const commands = await page.getByTestId("palette-command").allTextContents();
    for (const entry of SHORTCUTS.filter((each) => each.command)) expect(commands.some((text) => text.startsWith(entry.label))).toBe(true);
    await page.keyboard.type("Next agent");
    await page.keyboard.press("Enter");
    expect(await palette(page).count()).toBe(0);
    expect(await openAgent(page)).toBe(order[1] ?? null);
    await done(page);
  }, LIMIT);

  test("the other platform's modifier (⌃K on a Mac) does not open it", async () => {
    const page = await open();
    const apple = await page.evaluate(() => /Mac|iPhone|iPad|iPod/.test(navigator.platform));
    await page.keyboard.press(apple ? "Control+k" : "Meta+k");
    expect(await palette(page).count()).toBe(0);
    await done(page);
  }, LIMIT);
});

describe("? shortcut sheet", () => {
  test("lists every shortcut; Tab stays inside; Esc closes it and returns focus", async () => {
    const page = await open();
    const row = page.locator("[data-agent-id]").first();
    await row.focus();
    await page.keyboard.press("?");
    await sheet(page).waitFor();
    const labels = await sheet(page).locator("li > span").allTextContents();
    expect(labels).toEqual(SHORTCUTS.filter((entry) => entry.unlisted !== true).map((entry) => entry.label));
    for (const key of ["Tab", "Tab", "Shift+Tab", "Shift+Tab", "Shift+Tab"]) {
      await page.keyboard.press(key);
      expect(await sheet(page).evaluate((dialog) => dialog.contains(document.activeElement))).toBe(true);
    }
    await page.keyboard.press("Escape");
    expect(await sheet(page).isVisible()).toBe(false);
    expect(await row.evaluate((element) => element === document.activeElement)).toBe(true);
    await done(page);
  }, LIMIT);

  test("opens from the palette, and ? typed in a text field is a question mark", async () => {
    const page = await open();
    await pickFirst(page);
    await composer(page).focus();
    await page.keyboard.press("?");
    expect(await composer(page).inputValue()).toBe("?");
    expect(await sheet(page).isVisible()).toBe(false);
    await page.keyboard.press("ControlOrMeta+k");
    await page.keyboard.type("Keyboard shortcuts");
    await page.keyboard.press("Enter");
    expect(await sheet(page).isVisible()).toBe(true);
    await sheet(page).getByRole("button", { name: "Done" }).click();
    expect(await sheet(page).isVisible()).toBe(false);
    await done(page);
  }, LIMIT);
});

describe("⌘↩ allows and ⌘⌫ denies the focused card, and nothing else", () => {
  test("nothing focused, a button or a text field in the card: nothing is sent", async () => {
    const page = await open();
    await page.keyboard.press("ControlOrMeta+Enter");
    await page.keyboard.press("ControlOrMeta+Backspace");
    const card = page.locator(REQUEST);
    await card.getByRole("button", { name: "Deny" }).focus();
    await page.keyboard.press("ControlOrMeta+Enter");
    await page.keyboard.press("ControlOrMeta+Backspace");
    const message = card.getByRole("textbox");
    await message.fill("not this");
    await message.press("ControlOrMeta+Enter");
    await message.press("ControlOrMeta+Backspace");
    expect(await message.inputValue()).not.toBe("not this");
    expect(await sent(page)).toEqual([]);
    await done(page);
  }, LIMIT);

  test("on the focused card: ⌘↩ allows once and ⌘⌫ denies that request", async () => {
    const page = await open();
    const card = page.locator(REQUEST);
    expect(await card.getAttribute("aria-keyshortcuts")).toMatch(/^(Meta|Control)\+Enter (Meta|Control)\+Backspace$/);
    await card.focus();
    await page.keyboard.press("ControlOrMeta+Enter");
    await page.keyboard.press("ControlOrMeta+Backspace");
    expect(await sent(page)).toEqual([
      { verb: "agent.decide", agent: "fixture", request_id: "request-1", request_seq: 7, decision: "allow_once" },
      { verb: "agent.decide", agent: "fixture", request_id: "request-1", request_seq: 7, decision: "deny" },
    ]);
    await done(page);
  }, LIMIT);

  test("in the composer ⌘↩ sends the message, never a decision", async () => {
    const page = await open();
    await pickFirst(page);
    await composer(page).fill("hello");
    await composer(page).press("ControlOrMeta+Enter");
    expect(await sent(page)).toEqual([{ verb: "agent.send", agent: (await openAgent(page)) ?? "", text: "hello" }]);
    await done(page);
  }, LIMIT);
});

describe("Esc", () => {
  test("leaves the composer; in another text field it keeps the focus", async () => {
    const page = await open();
    await pickFirst(page);
    await composer(page).focus();
    await page.keyboard.press("Escape");
    expect(await activeTestId(page)).toBe("body");
    const message = page.locator(REQUEST).getByRole("textbox");
    await message.focus();
    await page.keyboard.press("Escape");
    expect(await message.evaluate((field) => field === document.activeElement)).toBe(true);
    await done(page);
  }, LIMIT);
});

describe("a modal owns the keyboard: nothing behind the palette or the sheet acts", () => {
  test("palette: ⌘↩/⌘⌫ on a card focused behind it decide nothing; its own Esc still closes it", async () => {
    const page = await open();
    await page.keyboard.press("ControlOrMeta+k");
    await palette(page).waitFor();
    await page.locator(REQUEST).evaluate((card: HTMLElement) => card.focus());
    await page.keyboard.press("ControlOrMeta+Enter");
    await page.keyboard.press("ControlOrMeta+Backspace");
    await dispatchOn(page, REQUEST, "Enter", { command: true });
    expect(await sent(page)).toEqual([]);
    expect(await palette(page).isVisible()).toBe(true);
    await page.keyboard.press("Escape");
    expect(await palette(page).count()).toBe(0);
    await done(page);
  }, LIMIT);

  test("sheet: a card's ⌘↩ and the page's ↓ dispatched behind it do nothing; its own Esc still closes it", async () => {
    const page = await open();
    await page.keyboard.press("?");
    await sheet(page).waitFor();
    await dispatchOn(page, REQUEST, "Enter", { command: true });
    await dispatchOn(page, REQUEST, "Backspace", { command: true });
    await dispatchOn(page, "body", "ArrowDown");
    expect(await sent(page)).toEqual([]);
    expect(await openAgent(page)).toBe(null);
    expect(await sheet(page).isVisible()).toBe(true);
    await page.keyboard.press("Escape");
    expect(await sheet(page).isVisible()).toBe(false);
    await done(page);
  }, LIMIT);
});

describe("an input method's composition (#2967)", () => {
  test("the palette's Enter does not choose while composing", async () => {
    const page = await open();
    await pickFirst(page);
    const before = await openAgent(page);
    await page.keyboard.press("ControlOrMeta+k");
    await page.keyboard.type("Next agent");
    expect(await dispatchOn(page, '[data-testid="palette"] input', "Enter", { isComposing: true })).toBe(false);
    expect(await dispatchOn(page, '[data-testid="palette"] input', "Enter", { keyCode: 229 })).toBe(false);
    expect(await palette(page).isVisible()).toBe(true);
    expect(await openAgent(page)).toBe(before);
    await page.keyboard.press("Enter");
    expect(await palette(page).count()).toBe(0);
    expect(await openAgent(page)).not.toBe(before);
    await done(page);
  }, LIMIT);

  test("no shortcut fires while composing; ⌘↩ sends once the composition ends", async () => {
    const page = await open();
    await pickFirst(page);
    await composer(page).fill("unfinished");
    await composer(page).evaluate((field) => {
      const apple = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
      const press = (key: string, extra: (event: KeyboardEvent) => void): void => {
        const event = new KeyboardEvent("keydown", { key, metaKey: apple, ctrlKey: !apple, bubbles: true, cancelable: true, isComposing: true });
        extra(event);
        field.dispatchEvent(event);
      };
      field.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true, data: "" }));
      press("Enter", () => undefined);
      press("k", () => undefined);
      // Safari reports the key that ends a composition as keyCode 229, with isComposing false.
      const safari = new KeyboardEvent("keydown", { key: "Enter", metaKey: apple, ctrlKey: !apple, bubbles: true, cancelable: true });
      Object.defineProperty(safari, "keyCode", { value: 229 });
      field.dispatchEvent(safari);
      field.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "unfinished" }));
    });
    expect(await sent(page)).toEqual([]);
    expect(await palette(page).count()).toBe(0);
    await composer(page).press("ControlOrMeta+Enter");
    expect((await sent(page)).map((each) => each["text"])).toEqual(["unfinished"]);
    await done(page);
  }, LIMIT);
});

describe("the key guard catches each bypass of the keyboard layer", () => {
  test.each([
    ["const", 'a "keydown" listener on document'],
    ["bind", 'a "keydown" listener on document'],
    ["spread", 'a JSX keydown handler on <input>'],
    ["capture", 'a "keydown" listener on <div> (capture)'],
    ["iframe", 'in a frame (about:srcdoc): a "keydown" listener on document'],
  ])("seed %s: %s", async (seed, finding) => {
    const clean = await open(null);
    expect(await keyListeners(clean)).toEqual([]);
    await clean.close();
    const page = await open(seed);
    expect(await keyListeners(page)).toContain(finding);
    await page.close();
  }, LIMIT);
});

for (const name of ["sidebar", "inspector"] as const) {
  test(`${name}: View collapse focuses its toggle and expand restores the hidden control`, async () => {
    const page = await open();
    const control = name === "sidebar" ? page.getByTestId("agent-row").first() : page.locator(REQUEST);
    const apple = await page.evaluate(() => /Mac|iPhone|iPad|iPod/.test(navigator.platform));
    const combo = name === "sidebar" ? (apple ? "Control+Meta+s" : "Control+s") : "Alt+ControlOrMeta+i";
    await control.focus();
    await page.keyboard.press(combo);
    expect(await control.isVisible()).toBe(false);
    const toggle = page.getByRole("button", { name: `Toggle ${name}`, exact: true });
    expect(await toggle.evaluate(element => element === document.activeElement)).toBe(true);
    await page.keyboard.press(combo);
    expect(await control.isVisible()).toBe(true);
    expect(await control.evaluate(element => element === document.activeElement)).toBe(true);
    await done(page);
  }, LIMIT);
}
