/** The keyboard layer's registry, labels and key matching (#2904, `src/workbench/keys.ts`). */
import { describe, expect, test } from "bun:test";

import { SHORTCUTS, ariaKeys, composing, editsText, formatCombo, matches, type Combo } from "../src/workbench/keys";

const ALL: Combo = { key: "j", ctrl: true, alt: true, shift: true, mod: true };
const press = (key: string, init: KeyboardEventInit = {}): KeyboardEvent => new KeyboardEvent("keydown", { key, ...init });

describe("labels", () => {
  test("modifiers are written in ⌃⌥⇧⌘ order", () => {
    expect(formatCombo(ALL, true)).toBe("⌃⌥⇧⌘J");
    expect(formatCombo(ALL, false)).toBe("Ctrl+Alt+Shift+J");
    expect(ariaKeys(ALL, true)).toBe("Control+Alt+Shift+Meta+j");
  });

  test("named keys read as their glyphs on Apple platforms", () => {
    expect(formatCombo({ key: "Enter", mod: true }, true)).toBe("⌘↩");
    expect(formatCombo({ key: "Backspace", mod: true }, false)).toBe("Ctrl+Backspace");
    expect(formatCombo({ key: "?" }, true)).toBe("?");
  });

  test("ids are unique; a page combo is shared only by focused shortcuts, listed before its one wider shortcut; a layer's combos are its own", () => {
    expect(new Set(SHORTCUTS.map((entry) => entry.id)).size).toBe(SHORTCUTS.length);
    const layer = SHORTCUTS.filter((entry) => entry.scope === "layer").map((entry) => formatCombo(entry.combo, true));
    expect(new Set(layer).size).toBe(layer.length);
    const combos = new Map<string, string[]>();
    for (const entry of SHORTCUTS.filter((each) => each.scope !== "layer")) combos.set(formatCombo(entry.combo, true), [...(combos.get(formatCombo(entry.combo, true)) ?? []), entry.scope]);
    // The dispatcher takes the first match, so a focused element's own binding must come first.
    for (const scopes of combos.values()) expect(scopes.slice(0, -1).every((scope) => scope === "focused")).toBe(true);
  });
});

describe("matching", () => {
  test("⌘ is Meta on Apple platforms and Control elsewhere; the other one never matches", () => {
    const k: Combo = { key: "k", mod: true };
    expect(matches(press("k", { metaKey: true }), k, true)).toBe(true);
    expect(matches(press("k", { ctrlKey: true }), k, true)).toBe(false);
    expect(matches(press("K", { ctrlKey: true }), k, false)).toBe(true);
    expect(matches(press("k", { metaKey: true }), k, false)).toBe(false);
    expect(matches(press("k", { metaKey: true, shiftKey: true }), k, true)).toBe(false);
  });

  test("a symbol typed with Shift matches without naming Shift", () => {
    expect(matches(press("?", { shiftKey: true }), { key: "?" }, true)).toBe(true);
    expect(matches(press("?", { shiftKey: true, metaKey: true }), { key: "?" }, true)).toBe(false);
  });

  test("a composing key is recognised by isComposing or keyCode 229", () => {
    expect(composing(press("Enter", { isComposing: true }))).toBe(true);
    expect(composing(press("Process", { keyCode: 229 }))).toBe(true);
    expect(composing(press("Enter"))).toBe(false);
  });

  test("text fields edit text; buttons and checkboxes do not", () => {
    const input = (type: string): HTMLInputElement => Object.assign(document.createElement("input"), { type });
    expect([input("text"), input("search"), document.createElement("textarea")].map(editsText)).toEqual([true, true, true]);
    expect([input("checkbox"), document.createElement("button"), null].map(editsText)).toEqual([false, false, false]);
  });
});
