import { afterEach, describe, expect, test } from "bun:test";
import { render } from "solid-js/web";

import { App } from "../src/App";
import type { Transport } from "../src/transport";
import { FIXTURE_STREAM, createFixtureTransport } from "../src/transport/fixture";
import { Transcript } from "../src/views/Transcript";
import { readFixture, readVariants } from "./fixture-replay";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.innerHTML = "";
});

/** Wraps a transport so a test can await the view's `synced`. */
function whenSynced(inner: Transport): { transport: Transport; synced: Promise<void> } {
  let done: () => void = () => {};
  const synced = new Promise<void>((resolve) => (done = resolve));
  return {
    synced,
    transport: {
      subscribe: (stream, after, observer) =>
        inner.subscribe(stream, after, {
          ...observer,
          synced: () => {
            observer.synced();
            done();
          },
        }),
    },
  };
}

function mount(view: () => ReturnType<typeof Transcript>): HTMLElement {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(view, root);
  return root;
}

const UNKNOWN_LINE = '{"type":"future_event","detail":{"x":1}}';
const MALFORMED_KNOWN_LINE = '{"type":"assistant","message":{"content":5}}';

describe("Transcript", () => {
  test("renders every replayed event, an unknown line, and a malformed known event", async () => {
    const [fixture, variants] = await Promise.all([readFixture(), readVariants()]);
    const jsonl = `${fixture}${UNKNOWN_LINE}\nnot json\n${MALFORMED_KNOWN_LINE}\n`;
    const { transport, synced } = whenSynced(createFixtureTransport(jsonl));
    const root = mount(() => <Transcript transport={transport} stream={FIXTURE_STREAM} source="replay" />);
    await synced;

    expect(root.querySelectorAll('[data-testid="known-event"]').length).toBe(variants.length);
    const unknown = [...root.querySelectorAll<HTMLDetailsElement>('[data-testid="unknown-event"]')];
    expect(unknown.map((block) => block.querySelector("summary")?.textContent)).toEqual([
      "Unrecognised event: future_event",
      "Unrecognised event",
      "Unrecognised event: assistant",
    ]);
    expect(unknown.every((block) => !block.open)).toBe(true);
    expect(unknown[0]?.querySelector("pre")?.textContent).toBe(JSON.stringify(JSON.parse(UNKNOWN_LINE), null, 2));
    expect(unknown[1]?.querySelector("pre")?.textContent).toBe("not json");
    expect(root.querySelector('[data-testid="transcript-synced"]')?.textContent).toBe(
      `End of recording · ${variants.length + 3} events`,
    );
    // The assistant's text and the tool result's content are on the page.
    expect(root.textContent).toContain("Session started · claude-haiku-5-5");
    expect(root.textContent).toContain("q-example submitted for user feedback");
  });

  test("a gap entry renders the fixed missing-range marker, and reset clears the list", async () => {
    let observer: Parameters<Transport["subscribe"]>[2] | undefined;
    const transport: Transport = { subscribe: (_stream, _after, given) => ((observer = given), { close: () => undefined }) };
    const root = mount(() => <Transcript transport={transport} stream="agent/a" source="replay" />);
    observer?.entry({ seq: 1, kind: "unknown", unknown: { raw: "x", shape: "s" } });
    observer?.entry({ seq: 9, kind: "gap", from: 2, to: 9 });
    expect(root.querySelector('[data-testid="transcript-gap"]')?.textContent).toBe("Events 2–9 of this session are no longer stored.");
    observer?.reset?.();
    expect(root.querySelector('[data-testid="transcript-gap"]')).toBeNull();
    expect(root.querySelector('[data-testid="unknown-event"]')).toBeNull();
  });

  test("a failed subscription is shown, not swallowed", async () => {
    const root = mount(() => <Transcript transport={createFixtureTransport("")} stream="agent/other" source="replay" />);
    await Promise.resolve();
    expect(root.querySelector('[role="alert"]')?.textContent).toContain("serves only agent/fixture");
  });
});

describe("App", () => {
  test("a missing shell config renders an explicit error state", () => {
    const root = mount(() => <App config={{ ok: false, error: { message: "No shell configuration" } }} />);
    expect(root.querySelector('[data-testid="config-error"]')?.textContent).toContain("No shell configuration");
    expect(root.querySelector('[data-testid="replay-badge"]')).toBeNull();
  });

  test("fixture mode shows the replay badge above the transcript", async () => {
    const config = { transport: "fixture", fixtureJsonl: await readFixture(), source: "events.jsonl" } as const;
    const root = mount(() => <App config={{ ok: true, config }} />);
    expect(root.querySelector('[data-testid="replay-badge"]')?.textContent).toBe("Fixture replayevents.jsonl");
    await Promise.resolve();
    expect(root.querySelectorAll('[data-testid="known-event"]').length).toBeGreaterThan(0);
  });
});
