import { afterEach, expect, test } from "bun:test";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";

import type { FleetNodeRow } from "../src/gen/FleetNodeRow";
import type { OutboxEntry } from "../src/wire/outbox";
import type { Sender } from "../src/workbench/Composer";
import { NewAgent } from "../src/workbench/NewAgent";

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; });
const node = (id: string): FleetNodeRow => ({ node_id: id, name: id, health: null, latency_ms: null, last_ok: null, version: null, error: null, link_diagnostics: [], online: true, route: null, counts: null, unbound_configured_peers: null });
function setup(blocked: string | null = null, withProjects = true) {
  const root = document.createElement("div");
  document.body.append(root);
  const [commands, setCommands] = createSignal<readonly OutboxEntry[]>([]);
  const starts: string[][] = [];
  let closed = false;
  const sender: Sender = { commands, blocked: () => blocked, send: () => "", decide: () => "", start: (...args) => { starts.push(args); return `start-${starts.length}`; } };
  dispose = render(() => <NewAgent sender={sender} home="home" nodes={[node("home"), node("remote")]} projects={withProjects ? [
    { node_id: "home", name: "first", id: "/fixture/first" },
    { node_id: "home", name: "second", id: "/fixture/second" },
    { node_id: "remote", name: "other", id: "/fixture/other" },
  ] : []} close={() => { closed = true; }} />, root);
  const field = (label: string): HTMLInputElement | HTMLSelectElement => [...root.querySelectorAll("label")].find(item => item.textContent?.startsWith(label))!.querySelector("input, select")!;
  const change = (label: string, value: string): void => {
    const input = field(label);
    input.value = value;
    input.dispatchEvent(new Event(input instanceof HTMLInputElement ? "input" : "change", { bubbles: true }));
  };
  const submit = (): void => { root.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })); };
  const settle = (status: "acked" | "failed" | "uncertain", code = "unsupported_for_agent"): void => { setCommands([{ cmd_id: "start-1", verb: "agent.start", args: {}, principal: "fixture", created_ms: 0, status, ...(status === "failed" ? { error: { code, exit: 2, message: "diagnosis", retry: "never" as const } } : {}) }]); };
  return { root, field, change, submit, settle, starts, closed: () => closed };
}

test("home and its project default; typed name and selected project are sent once", () => {
  const h = setup();
  expect(h.field("Node").value).toBe("home");
  expect(h.field("Runtime").disabled).toBe(true);
  expect(h.field("Harness").disabled).toBe(true);
  h.change("Name", "  task  ");
  h.change("Project", "/fixture/second");
  h.submit(); h.submit();
  expect(h.starts).toEqual([["task", "home", "/fixture/second"]]);
  expect(h.root.textContent).not.toContain("Agent started.");
  h.settle("acked");
  expect(h.root.textContent).toContain("Agent started.");
});

test("remote selection resets the project; typed refusal is plain language with details", () => {
  const h = setup();
  h.change("Name", "task");
  h.change("Project", "/fixture/second");
  h.change("Node", "remote");
  h.submit();
  expect(h.starts).toEqual([["task", "remote", "/fixture/other"]]);
  h.settle("failed");
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe("Agents cannot be started on another node yet. Choose the home node.");
  expect(h.root.querySelector("details")?.textContent).toContain("unsupported_for_agent");
});

test("blank names and blocked connections cannot enqueue", () => {
  const h = setup("Read only.");
  h.submit(); h.change("Name", "task"); h.submit();
  expect(h.starts).toEqual([]);
  expect(h.root.textContent).toContain("Read only.");
});

test("uncertain starts cannot be duplicated and Done closes the sheet", () => {
  const h = setup();
  h.change("Name", "task"); h.submit(); h.settle("uncertain"); h.submit();
  expect(h.starts).toHaveLength(1);
  expect(h.root.textContent).toContain("no answer from the server in 30 s");
  [...h.root.querySelectorAll("button")].find(button => button.textContent === "Done")!.click();
  expect(h.closed()).toBe(true);
});

test("without project rows an absolute typed directory is required", () => {
  const h = setup(null, false);
  h.change("Name", "task");
  const start = [...h.root.querySelectorAll("button")].find(button => button.textContent === "Start agent")!;
  expect(h.field("Working directory").value).toBe("");
  expect(start.disabled).toBe(true);
  h.submit();
  h.change("Working directory", "relative/path");
  expect(start.disabled).toBe(true);
  h.submit();
  expect(h.starts).toHaveLength(0);
  h.change("Working directory", "/fixture/typed");
  expect(start.disabled).toBe(false);
  h.submit();
  expect(h.starts).toEqual([["task", "home", "/fixture/typed"]]);
});

test("custom directory overrides the first project and resets on node change", () => {
  const h = setup();
  expect(h.field("Working directory").value).toBe("/fixture/first");
  h.change("Working directory", "/fixture/custom");
  expect(h.field("Project").value).toBe("");
  h.change("Node", "remote");
  expect(h.field("Working directory").value).toBe("/fixture/other");
  h.change("Working directory", "/fixture/remote-custom");
  h.change("Name", "task");
  h.submit();
  expect(h.starts).toEqual([["task", "remote", "/fixture/remote-custom"]]);
});

test("directory validation refusal is plain language with the server diagnosis in Details", () => {
  const h = setup();
  h.change("Name", "task"); h.submit(); h.settle("failed", "invalid_params");
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe("The agent could not be started with these settings. Check the name and working directory, then try again.");
  expect(h.root.querySelector("details")?.textContent).toContain("invalid_params");
  expect(h.root.querySelector("details")?.textContent).toContain("diagnosis");
});

test("a definitive refusal allows correcting the node and directory before a new start", () => {
  const h = setup();
  h.change("Name", "task"); h.change("Node", "remote"); h.submit(); h.settle("failed");
  expect(h.field("Node").disabled).toBe(false);
  h.change("Node", "home");
  h.change("Working directory", "/fixture/corrected");
  h.submit();
  expect(h.starts).toEqual([["task", "remote", "/fixture/other"], ["task", "home", "/fixture/corrected"]]);
});

test("unknown refusal codes use the retry copy even when they name object properties", () => {
  const h = setup();
  h.change("Name", "task"); h.submit(); h.settle("failed", "__proto__");
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe("The server refused this. Trying again will not help; check the details.");
});

test("an unconfirmed command outcome stays held even when delivered in a failed ack", () => {
  const h = setup();
  h.change("Name", "task"); h.submit(); h.settle("failed", "session_unconfirmed"); h.submit();
  expect(h.starts).toHaveLength(1);
  expect(h.field("Name").disabled).toBe(true);
  expect(h.root.querySelector('[role="status"]')?.textContent).toBe("The server could not confirm whether the agent started. Check the agent list before starting another one.");
});
