/**
 * The layout primitives render their element and class, and pass their
 * values to layout.css as custom properties (#2895).
 */
import { afterEach, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { Anchor, AnchorPanel, Center, Cluster, Grid, Sidebar, Stack, Switcher } from "../src/layout/primitives";

import { SPACE } from "../src/tokens";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

function mount(view: () => Element | Element[] | string | number | boolean | null | undefined | unknown): HTMLElement {
  dispose = render(view as () => never, document.body);
  return document.body.firstElementChild as HTMLElement;
}

const prop = (element: HTMLElement, name: string): string => element.style.getPropertyValue(name);

test("Stack spaces its children on the token scale, by default 16 px", () => {
  const stack = mount(() => (
    <Stack as="section" class="x" aria-label="s">
      <p>a</p>
    </Stack>
  ));
  expect(stack.tagName).toBe("SECTION");
  expect(stack.className).toBe("l-stack x");
  expect(stack.getAttribute("aria-label")).toBe("s");
  expect(prop(stack, "--l-space")).toBe("var(--space-4)");
});

test("Cluster passes its space, justification and alignment", () => {
  const cluster = mount(() => <Cluster space={SPACE[1]} justify="space-between" align="baseline" />);
  expect(cluster.className).toBe("l-cluster");
  expect(prop(cluster, "--l-space")).toBe("var(--space-1)");
  expect(prop(cluster, "--l-justify")).toBe("space-between");
  expect(prop(cluster, "--l-align")).toBe("baseline");
});

test("Sidebar marks its side and passes its widths", () => {
  const sidebar = mount(() => <Sidebar side="right" sideWidth="16rem" contentMin="60%" space={SPACE[6]} />);
  expect(sidebar.dataset["side"]).toBe("right");
  expect(prop(sidebar, "--l-side")).toBe("16rem");
  expect(prop(sidebar, "--l-content-min")).toBe("60%");
  expect(prop(sidebar, "--l-space")).toBe("var(--space-6)");
});

test("Switcher, Center and Grid pass their measures", () => {
  expect(prop(mount(() => <Switcher threshold="40rem" />), "--l-threshold")).toBe("40rem");
  dispose?.();
  const center = mount(() => <Center max="40ch" gutters={SPACE[2]} intrinsic />);
  expect(prop(center, "--l-max")).toBe("40ch");
  expect(prop(center, "--l-space")).toBe("var(--space-2)");
  expect(center.hasAttribute("data-intrinsic")).toBe(true);
  dispose?.();
  expect(prop(mount(() => <Grid min="12rem" />), "--l-min")).toBe("12rem");
});

test("Anchor holds a trigger and its panel; the panel passes its gap and the edge it lines up with", () => {
  const anchor = mount(() => (
    <Anchor class="x">
      <button type="button">open</button>
      <AnchorPanel as="section" align="end" space={SPACE[2]} hidden>
        panel
      </AnchorPanel>
    </Anchor>
  ));
  const panel = anchor.querySelector("section") as HTMLElement;
  expect([anchor.className, panel.className, panel.dataset["align"], panel.hidden]).toEqual(["l-anchor x", "l-anchor__panel", "end", true]);
  expect(prop(panel, "--l-space")).toBe("var(--space-2)");
  dispose?.();
  const plain = mount(() => <AnchorPanel />);
  expect([plain.dataset["align"], prop(plain, "--l-space")]).toEqual(["start", "var(--space-1)"]);
});

test("space accepts only steps of the scale", () => {
  // @ts-expect-error 14 px is off the 4 px scale (tsc fails if this ever compiles).
  mount(() => <Stack space="14" />);
  // @ts-expect-error a primitive owns its style.
  mount(() => <Stack style={{ color: "red" }} />);
});
