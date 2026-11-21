import { afterEach, expect, test } from "bun:test";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";

import type { AgentRow } from "../src/fleet";
import type { OutboxEntry } from "../src/wire/outbox";
import type { Sender } from "../src/workbench/Composer";
import { createLifecycle } from "../src/workbench/lifecycle";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); });
const agent = (id = "fixture", state: AgentRow["state"] = "finished"): AgentRow => ({ id, label: id, state, activity: "", waitingOnHuman: [] });
function setup(throws = false) {
  const root = document.createElement("div");
  document.body.append(root);
  const [row, setRow] = createSignal<AgentRow | undefined>(agent());
  const [commands, setCommands] = createSignal<readonly OutboxEntry[]>([]);
  const [blocked, setBlocked] = createSignal<string | null>(null);
  const calls: string[][] = [];
  const send = (verb: string, target: string): string => {
    if (throws) throw new Error("fixture queue failure");
    calls.push([verb, target]);
    const id = `c${calls.length}`;
    setCommands([...commands(), { cmd_id: id, verb: `agent.${verb}`, args: { target }, principal: "fixture", created_ms: 0, status: "sent" }]);
    return id;
  };
  const sender: Sender = { commands, blocked, send: () => "", decide: () => "", restart: id => send("restart", id), stop: id => send("stop", id) };
  let lifecycle!: ReturnType<typeof createLifecycle>;
  dispose = render(() => { lifecycle = createLifecycle(() => sender, row); return lifecycle.view(); }, root);
  const button = (label: string): HTMLButtonElement => [...root.querySelectorAll("button")].find(button => button.textContent === label)!;
  const settle = (status: OutboxEntry["status"], code?: string): void => { setCommands(commands().map(entry => ({ ...entry, status, ...(code ? { error: { code, message: "raw server diagnosis", exit: 2, retry: "never" as const } } : {}) }))); };
  return { root, row, setRow, setBlocked, button, calls, settle, commands: () => lifecycle.commands() };
}

test("Stop binds a confirmation to the agent and cancel sends nothing", () => {
  const h = setup();
  h.button("Stop agent").click();
  expect(h.root.querySelector('[role="group"]')?.textContent).toContain("Stop fixture?");
  expect(h.calls).toEqual([]);
  h.button("Cancel").click();
  expect(h.calls).toEqual([]);
  h.button("Stop agent").click();
  h.button("Confirm stop").click();
  expect(h.calls).toEqual([["stop", "fixture"]]);
  expect(h.button("Stop agent").disabled).toBe(true);
  h.settle("acked");
  expect(h.root.textContent).toContain("Stop requested; waiting for the agent to go offline.");
});

test("Restart resumes only an offline agent; the palette shares the actions", () => {
  const h = setup();
  expect(h.button("Restart agent").disabled).toBe(true);
  h.setRow(agent("fixture", "offline"));
  expect(h.button("Stop agent").disabled).toBe(true);
  h.commands().find(command => command.label === "Restart agent")!.run();
  expect(h.calls).toEqual([["restart", "fixture"]]);
  h.button("Restart agent").click();
  expect(h.calls).toHaveLength(1);
  h.settle("acked");
  expect(h.root.textContent).toContain("Agent restarted.");
});

test("palette Stop requires confirmation; changing agents cancels it", () => {
  const h = setup();
  h.commands().find(command => command.label === "Stop agent")!.run();
  expect(h.calls).toHaveLength(0);
  h.setRow(agent("other"));
  expect(h.root.querySelector('[role="group"]')).toBeNull();
  h.button("Stop agent").click();
  h.button("Confirm stop").click();
  expect(h.calls).toEqual([["stop", "other"]]);
});

test.each(["uncertain", "unknown_outcome"] as const)("%s outcomes hold both lifecycle controls", status => {
  const h = setup();
  h.button("Stop agent").click(); h.button("Confirm stop").click(); h.settle(status);
  expect(h.button("Stop agent").disabled).toBe(true);
  h.setRow(agent("fixture", "offline"));
  expect(h.button("Restart agent").disabled).toBe(true);
});

test.each([
  ["operator_only", "Stopping or restarting requires the operator's app on the agent's home node. Open the app there."],
  ["structured_recovery_required", "This agent's process ownership is uncertain. Ask the operator to recover it before trying again."],
  ["structured_profile_mismatch", "This agent belongs to a different profile. Restore its original profile before restarting."],
  ["session_unconfirmed", "The server could not confirm this action. Check the agent's state before trying again."],
])("typed refusal %s uses plain words and details", (code, line) => {
  const h = setup();
  h.button("Stop agent").click(); h.button("Confirm stop").click(); h.settle("failed", code);
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe(line);
  expect(h.root.querySelector("details")?.textContent).toContain(code);
  expect(h.root.querySelector('[role="status"]')?.textContent).not.toContain("raw server diagnosis");
  if (code === "session_unconfirmed") expect(h.button("Stop agent").disabled).toBe(true);
});

test("read-only connections disable actions including an open confirmation", () => {
  const h = setup();
  h.button("Stop agent").click(); h.setBlocked("Read only."); h.button("Confirm stop").click();
  expect(h.calls).toHaveLength(0);
  expect(h.root.textContent).toContain("Read only.");
});

test("a queue failure remains visible after the confirmation closes", () => {
  const h = setup(true);
  h.button("Stop agent").click(); h.button("Confirm stop").click();
  expect(h.calls).toHaveLength(0);
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe("The action could not be queued. Reconnect, then try again.");
});
