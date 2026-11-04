/**
 * The keyboard layer's runtime guard (#2904, web/DESIGN.md "Keyboard"): the
 * authority on "no key listener outside `src/workbench/keys.ts`". ESLint's
 * `design/keys` rule is only the early warning.
 *
 * Every browser test installs {@link installKeyGuard} as an init script, so
 * it runs before any page code. It wraps `EventTarget.prototype.addEventListener`
 * and the `onkeydown`/`onkeyup`/`onkeypress` setters, and records every
 * key-event listener except the one carrying the layer's brand
 * (`Symbol.for("flock.keyboard-layer")`, set only on the layer's listener).
 * Aliases (`addEventListener.bind`, `.call`), computed event names and
 * capture listeners all reach the wrapped method, so none escapes. Solid
 * compiles a JSX `onKeyDown` (spread or not) to a `$$keydown` property on
 * the element plus a document listener of its own: the first is found by a
 * scan of the rendered elements, the second by the wrapper. The guard
 * patches only test pages, never a shipped bundle.
 */
import { expect } from "bun:test";

import type { Page } from "playwright";

/** In the page: installs the guard and defines `globalThis.__keyListeners`. Serialised by Playwright: self-contained. */
export function installKeyGuard(): void {
  const brand = Symbol.for("flock.keyboard-layer");
  const keyEvent = /^key(?:down|up|press)/i;
  const found: string[] = [];
  const where = (target: unknown): string => {
    if (target === window) return "window";
    if (target === document) return "document";
    if (target instanceof Element) return `<${target.tagName.toLowerCase()}${target.getAttribute("data-testid") ? ` data-testid="${target.getAttribute("data-testid")}"` : ""}>`;
    return Object.prototype.toString.call(target);
  };
  const listen = EventTarget.prototype.addEventListener;
  EventTarget.prototype.addEventListener = function (this: EventTarget, type: string, listener: EventListenerOrEventListenerObject, options?: boolean | AddEventListenerOptions): void {
    const branded = typeof listener === "function" && Reflect.get(listener, brand) === true;
    const capture = options === true || (typeof options === "object" && options.capture === true);
    if (keyEvent.test(String(type)) && !branded) found.push(`a "${type}" listener on ${where(this)}${capture ? " (capture)" : ""}`);
    listen.call(this, type, listener, options);
  };
  for (const owner of [HTMLElement.prototype, SVGElement.prototype, Document.prototype, window]) {
    for (const name of ["onkeydown", "onkeyup", "onkeypress"]) {
      const own = Object.getOwnPropertyDescriptor(owner, name);
      const set = own?.set;
      if (own === undefined || set === undefined) continue;
      Object.defineProperty(owner, name, {
        ...own,
        set(this: EventTarget, value: unknown) {
          if (value !== null && value !== undefined) found.push(`an ${name} property on ${where(this)}`);
          set.call(this, value);
        },
      });
    }
  }
  Reflect.set(globalThis, "__keyListeners", (): string[] => [
    ...found,
    ...[...document.querySelectorAll("*")].flatMap((element) =>
      Object.keys(element)
        .filter((key) => /^\$\$key(?:down|up|press)$/i.test(key) && Reflect.get(element, key) != null)
        .map((key) => `a JSX ${key.slice(2)} handler on ${where(element)}`),
    ),
  ]);
}

/** Installs the guard on `page` for every document it loads. */
export async function guardKeys(page: Page): Promise<void> {
  await page.addInitScript(installKeyGuard);
}

/** What the guard recorded on `page`'s document. */
export async function keyListeners(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const read = Reflect.get(globalThis, "__keyListeners");
    return typeof read === "function" ? read() : ["the key guard is not installed in this document"];
  });
}

/** Fails unless the guard recorded nothing on `page`. */
export async function expectNoKeyListeners(page: Page): Promise<void> {
  expect(await keyListeners(page)).toEqual([]);
}
