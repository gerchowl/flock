import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";
import { Workbench } from "../src/layout/Workbench";
import { createManager, defaultLayout } from "../src/layout/model";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; localStorage.clear(); });
function mount(mode: "phone" | "desktop") {
  const m = createManager(defaultLayout()); m.open("artifact/example", "split_right", "artifact");
  const chip = m.open("inbox/example", "tab", "inbox"); m.collapse(chip);
  dispose = render(() => <Workbench serverKey="fixture" mode={mode} initial={m.state} />, document.body);
}
function click(label: string) {
  const button = [...document.querySelectorAll("button")].find(b => b.textContent === label);
  expect(button).toBeDefined(); button!.click();
}
test("phone renders one tile, switcher, bottom-sheet rail open and closed", () => {
  mount("phone");
  expect(document.querySelectorAll(".wb-tile")).toHaveLength(1);
  expect(document.querySelector('[aria-label="Tile switcher"]')).not.toBeNull();
  expect(document.querySelector('[role="dialog"]')).toBeNull();
  click("Rail (1)"); expect(document.querySelector(".wb-sheet")).not.toBeNull();
  click("Close rail"); expect(document.querySelector(".wb-sheet")).toBeNull();
});
test("desktop renders split tiles and edge rail; keyboard focus selects another tile", () => {
  mount("desktop");
  expect(document.querySelectorAll(".wb-tile")).toHaveLength(2);
  expect(document.querySelector('.wb-desktop > [aria-label="Rail"]')).not.toBeNull();
  document.querySelector(".workbench")!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", altKey: true, bubbles: true }));
  expect(document.activeElement?.id).toBe("tile-t2");
});
test("phone expand uses history and popstate returns to origin", () => {
  mount("phone");
  const origin = document.querySelector(".wb-tile")!.id;
  click("Expand into second");
  expect(document.querySelectorAll(".wb-tile")).toHaveLength(1);
  expect(document.querySelector(".wb-tile")!.id).not.toBe(origin);
  window.dispatchEvent(new PopStateEvent("popstate", { state: { "flock-layout:fixture": origin.replace("tile-", "") } }));
  expect(document.querySelector(".wb-tile")!.id).toBe(origin);
});
test("desktop prop overrides a phone viewport and phone prop overrides a desktop viewport", () => {
  const previous = window.innerWidth;
  Object.defineProperty(window, "innerWidth", { configurable: true, value: 390 });
  mount("desktop"); expect(document.querySelector(".workbench")?.getAttribute("data-mode")).toBe("desktop"); dispose?.();
  Object.defineProperty(window, "innerWidth", { configurable: true, value: 1280 });
  mount("phone"); expect(document.querySelector(".workbench")?.getAttribute("data-mode")).toBe("phone");
  Object.defineProperty(window, "innerWidth", { configurable: true, value: previous });
});
