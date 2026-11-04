/**
 * The keyboard tests' page (#2904, `e2e/keyboard.test.ts`): the desktop
 * workbench over the sample agents, with one permission card that answers
 * for real and a sender that records every command it is given in
 * `globalThis.__sent` instead of queueing it.
 *
 * `?seed=<name>` also renders one of {@link SEEDS}: real, compiled code that
 * registers a key listener outside the keyboard layer in a way ESLint's
 * `design/keys` rule once missed, for the key guard (`e2e/key-guard.ts`) to catch.
 *
 * @module
 */
import type { JSX } from "solid-js";
import { render } from "solid-js/web";

import { createFixtureFleet } from "../../src/fleet";
import type { AgentDecide } from "../../src/gen/AgentDecide";
import { emptyLayout } from "../../src/layout/model";
import { createFixtureTransport } from "../../src/transport/fixture";
import type { Sender } from "../../src/workbench/Composer";
import type { InboxCard } from "../../src/workbench/inbox";
import { SAMPLE_AGENTS } from "../../src/workbench/samples";
import { Workbench } from "../../src/workbench/Workbench";
import "../../src/tokens.css";
import "../../src/styles.css";

type Sent = { readonly verb: "agent.send"; readonly agent: string; readonly text: string } | ({ readonly verb: "agent.decide" } & AgentDecide);

const sent: Sent[] = [];
Reflect.set(globalThis, "__sent", sent);

const sender: Sender = {
  commands: () => [],
  send: (agent, text) => String(sent.push({ verb: "agent.send", agent, text })),
  decide: (args) => String(sent.push({ verb: "agent.decide", ...args })),
  blocked: () => null,
};

const CARD: InboxCard = {
  id: "request-1",
  kind: "permission",
  agent: "fixture",
  agentLabel: "fixture",
  title: "Run Bash?",
  detail: "just check",
  choices: ["Allow once", "Always allow", "Deny"],
  blocking: true,
  requestSeq: 7,
};

const ignore = (): void => undefined;

/** Each bypass the key guard must catch, as the code a component might contain. */
export const SEEDS: Readonly<Record<string, () => JSX.Element>> = {
  // The event name through a const.
  const: () => {
    const type = "keydown";
    document.addEventListener(type, ignore);
    return null;
  },
  // A bound alias of addEventListener.
  bind: () => {
    const on = document.addEventListener.bind(document);
    on("keydown", ignore);
    return null;
  },
  // A JSX key prop arriving through a spread.
  spread: () => {
    const props = { onKeyDown: ignore };
    return <input aria-label="Seed" {...props} />;
  },
  // A capture listener: Solid's capture syntax.
  capture: () => <div oncapture:keydown={ignore}>Seed</div>,
  // A same-origin child frame with a listener of its own.
  iframe: () => <iframe title="Seed" hidden srcdoc={'<script>document.addEventListener("keydown", () => undefined)</script>'} />,
};

const seed = new URLSearchParams(location.search).get("seed");

const root = document.getElementById("root");
if (root === null) throw new Error("no #root");
render(
  () => (
    <>
    {seed === null ? null : (SEEDS[seed] ?? (() => { throw new Error(`no seed ${seed}`); }))()}
    <Workbench
      fleet={createFixtureFleet(SAMPLE_AGENTS)}
      transport={createFixtureTransport("")}
      inbox={[CARD]}
      sender={sender}
      layout={emptyLayout()}
      saveLayout={() => undefined}
      mode="desktop"
      source="replay"
    />
    </>
  ),
  root,
);
