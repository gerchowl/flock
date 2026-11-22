/**
 * The icon set and the window's focus (#2903): every glyph renders as
 * decoration, an icon button carries its name, and the workbench marks
 * whether its window is active so selections are emphasised only there.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { ICON_NAMES, Icon, IconButton } from "../src/components/Icon";
import { createFixtureFleet } from "../src/fleet";
import { emptyLayout } from "../src/layout/model";
import { createFixtureTransport } from "../src/transport/fixture";
import { SAMPLE_AGENTS } from "../src/workbench/samples";
import { Workbench } from "../src/workbench/Workbench";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

function mount(view: () => ReturnType<typeof Icon>): HTMLElement {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(view, root);
  return root;
}

describe("the icon set", () => {
  test("every glyph is hidden decoration on the 16-unit grid, stroked in the text colour", () => {
    const root = mount(() => (
      <>
        {ICON_NAMES.map((name) => (
          <Icon name={name} />
        ))}
      </>
    ));
    const svgs = [...root.querySelectorAll("svg")];
    expect(svgs.map((svg) => svg.getAttribute("data-icon"))).toEqual([...ICON_NAMES]);
    for (const svg of svgs) {
      expect(svg.getAttribute("aria-hidden")).toBe("true");
      expect(svg.getAttribute("viewBox")).toBe("0 0 16 16");
      expect(svg.getAttribute("class")).toBe("icon");
      expect(svg.children.length).toBeGreaterThan(0);
    }
  });

  test("an icon button is named by its label, which is also its tooltip unless a title says more", () => {
    const root = mount(() => (
      <>
        <IconButton icon="send" label="Send" />
        <IconButton icon="search" label="Search" title="Search ⌘K" />
      </>
    ));
    const [send, search] = [...root.querySelectorAll("button")];
    expect([send?.getAttribute("aria-label"), send?.getAttribute("title"), send?.getAttribute("type")]).toEqual(["Send", "Send", "button"]);
    expect([search?.getAttribute("aria-label"), search?.getAttribute("title")]).toEqual(["Search", "Search ⌘K"]);
    expect(send?.textContent).toBe("");
  });
});

describe("the window's focus", () => {
  test("the workbench says whether its window is active, following focus and blur", () => {
    const root = mount(() => (
      <Workbench fleet={createFixtureFleet(SAMPLE_AGENTS)} transport={createFixtureTransport("")} inbox={[]} sender={null} layout={emptyLayout()} saveLayout={() => undefined} source="replay" />
    ));
    const workbench = root.querySelector('[data-testid="workbench"]');
    window.dispatchEvent(new FocusEvent("blur"));
    expect(workbench?.getAttribute("data-window")).toBe("inactive");
    window.dispatchEvent(new FocusEvent("focus"));
    expect(workbench?.getAttribute("data-window")).toBe("active");
    window.dispatchEvent(new FocusEvent("blur"));
    expect(workbench?.getAttribute("data-window")).toBe("inactive");
  });
});
