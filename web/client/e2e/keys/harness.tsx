/**
 * The keyboard tests' page (#2904, `e2e/keyboard.test.ts`): the desktop
 * workbench over the sample agents, with one permission card that answers
 * for real and a sender that records every command it is given in
 * `globalThis.__sent` instead of queueing it.
 *
 * @module
 */
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

const root = document.getElementById("root");
if (root === null) throw new Error("no #root");
render(
  () => (
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
  ),
  root,
);
