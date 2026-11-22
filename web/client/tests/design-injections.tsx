import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { Stack } from "../src/layout/primitives";
import { SPACE } from "../src/tokens";

const element = () => document.body.appendChild(document.createElement("div"));
const css = "margin:13px;color:rgb(1, 2, 3)";
const html = `<p style="${css}">seed</p>`;
export const injections = {
  setAttribute() { element().setAttribute("style", css); },
  computedAttribute() { const key = ["sty", "le"].join(""); element().setAttribute(key, css); },
  innerHTML() { element().innerHTML = html; },
  adjacentHTML() { element().insertAdjacentHTML("beforeend", html); },
  templateHTML() { const template = document.createElement("template"); template.innerHTML = html; element().append(template.content.cloneNode(true)); },
  jsxHTML() { render(() => <div innerHTML={html} />, element()); },
  styleElement() { const node = document.createElement("style"); node.textContent = `.seed{${css}}`; document.head.append(node); },
  jsxStyle() { render(() => <style>{`.seed{${css}}`}</style>, element()); },
  attributeNode() { const attr = document.createAttribute("style"); attr.value = css; element().setAttributeNode(attr); },
  dataLink() { const node = document.createElement("link"); node.rel = "stylesheet"; node.href = `data:text/css,.seed{${css}}`; document.head.append(node); },
  styleProperty() { element().style.margin = "13px"; },
  styleSetter() { element().style.cssText = css; },
  elementStyle() { Object.assign(element(), {style:css}); },
  assignStyle() { Object.assign(element().style, {margin:"13px"}); },
  computedAssign() { const k = ["sty","le"].join(""); Object.assign(element(), {[k]:css}); },
  doubleCast() { const k = "style"; (element() as unknown as Record<string,string>)[k] = css; },
  setProperty() { element().style.setProperty("margin", "13px"); },
};
Object.assign(window, { designInjections: injections, mountDesignPrimitive() {
  const [space, setSpace] = createSignal<import("../src/tokens").SpaceStep>(SPACE[4]);
  Object.assign(window, { changeDesignSpace: () => setSpace(SPACE[1]) });
  render(() => <Stack space={space()} />, element());
} });
