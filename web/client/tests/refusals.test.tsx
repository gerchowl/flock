/**
 * The shared plain-language map (#2902): every refusal and connection state
 * has copy with a next step, primary copy carries no protocol strings, and
 * the raw code sits behind Details.
 */
import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { CONNECTION_COPY, ErrorState, REFUSALS, RefusalDetails, explain } from "../src/workbench/refusals";

/** What primary copy must never show: snake_case codes, CamelCase state names, ids and wire jargon. */
const PROTOCOL = /\b[a-z]+_[a-z_]+\b|\b[A-Z][a-z]+[A-Z][A-Za-z]*\b|ADR-\d|#\d|\bwire\b|\bprotocol\b|\bsub(fail| id)\b|\back\b/;

let dispose: (() => void) | null = null;
afterEach(() => {
  dispose?.();
  dispose = null;
  document.body.innerHTML = "";
});
const mount = (view: () => ReturnType<typeof ErrorState>): HTMLElement => {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(view, root);
  return root;
};

describe("the refusal map", () => {
  test("every code has an effect and a next step, each a sentence with no protocol strings", () => {
    for (const [code, copy] of Object.entries(REFUSALS)) {
      for (const part of [copy.effect, copy.next]) {
        expect({ code, part, sentence: /^[A-Z].*\.$/.test(part) }).toEqual({ code, part, sentence: true });
        expect({ code, part, raw: PROTOCOL.test(part) }).toEqual({ code, part, raw: false });
      }
    }
  });

  test("a known code reads as its effect then its next step, never the server's message", () => {
    const worded = explain({ code: "session_busy", message: "command refused: session_busy", retry: "later" });
    expect(worded).toEqual({ line: "The server is busy. Try again in a moment.", code: "session_busy", message: "command refused: session_busy" });
  });

  test("an unknown code falls back to its retry class's next step, with nothing raw in the line", () => {
    for (const retry of ["n/a", "never", "maybe", "yes", "later", "after_inspecting", "after_human", "after_handoff"] as const) {
      const { line } = explain({ code: "brand_new_code", message: "server words", retry });
      expect(line).toStartWith("The server refused this. ");
      expect([line.includes("brand_new_code"), line.includes("server words"), PROTOCOL.test(line)]).toEqual([false, false, false]);
    }
    expect(explain({ code: "brand_new_code", message: "m" }).line).toBe("The server refused this. Check the details.");
  });
});

describe("connection states", () => {
  test("no state reads as its own name or a protocol string", () => {
    for (const [state, copy] of Object.entries(CONNECTION_COPY)) {
      expect({ state, copy, named: copy === state || PROTOCOL.test(copy) }).toEqual({ state, copy, named: false });
    }
  });
});

describe("Details", () => {
  test("holds the raw code and message, closed until opened", () => {
    const root = mount(() => <RefusalDetails code="scope" message="not for this token" />);
    const details = root.querySelector("details") as HTMLDetailsElement;
    expect([details.open, details.querySelector("summary")?.textContent, details.querySelector("code")?.textContent]).toEqual([false, "Details", "scope"]);
    expect(details.textContent).toBe("Detailsscope not for this token");
  });

  test("the error state's Retry calls back, and its line shows no code", () => {
    let retried = 0;
    const worded = explain({ code: "scope", message: "not for this token" });
    const root = mount(() => <ErrorState title="Couldn't load this agent's history." line={worded.line} code={worded.code} message={worded.message} retry={() => retried++} />);
    (root.querySelector('[data-testid="retry"]') as HTMLButtonElement).click();
    expect(retried).toBe(1);
    const primary = [...root.querySelectorAll('[data-testid="error-state"] > :not(details)')].map((el) => el.textContent).join(" ");
    expect(primary).not.toContain("scope");
    expect(primary).toContain(worded.line);
  });
});
