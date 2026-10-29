/**
 * The session store owns each `agent/<id>` stream's entities (ADR-0038 §3):
 * a transcript and the permission inbox on one agent share one subscription,
 * one reset, and one record. A counting fake transport stands in for the wire.
 */
import { afterEach, expect, test } from "bun:test";
import { createRoot } from "solid-js";
import { render } from "solid-js/web";

import { permissionLifecycle } from "../src/gen/FixtureExamples";
import type { AgentRow } from "../src/fleet";
import { sessionStore } from "../src/store/sessions";
import type { SessionObserver, Transport } from "../src/transport";
import { classifyStoredLine } from "../src/transport/fixture";
import { Transcript } from "../src/views/Transcript";
import { createPermissionInbox } from "../src/workbench/permissions";

const disposers: (() => void)[] = [];
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose();
  document.body.innerHTML = "";
});

/** A transport that counts what reaches the wire layer: opened and closed subscriptions per stream. */
function counting() {
  const observers: SessionObserver[] = [];
  const opened: string[] = [];
  const closed: boolean[] = [];
  let closes = 0;
  const transport: Transport = {
    subscribe: (stream, _after, observer) => {
      const index = opened.push(stream) - 1;
      observers.push(observer);
      closed.push(false);
      return { close: () => void (closes++, (closed[index] = true)) };
    },
  };
  let seq = 0;
  const only = (): SessionObserver => {
    const observer = observers[0];
    if (observer === undefined || observers.length !== 1) throw new Error(`expected one subscription, saw ${observers.length}`);
    return observer;
  };
  return {
    transport,
    opened,
    closes: () => closes,
    closed,
    observer: (index: number): SessionObserver => {
      const found = observers[index];
      if (found === undefined) throw new Error(`no subscription ${index}`);
      return found;
    },
    send: (event: object) => only().entry(classifyStoredLine(++seq, JSON.stringify(event))),
    synced: () => only().synced(),
    reset: () => {
      seq = 0;
      only().reset?.();
    },
  };
}

const open = (id: string) => ({ ...permissionLifecycle[0], request_id: id, request_seq: 1 });
const row = (id: string, waitingOnHuman: string[]): AgentRow => ({ id, label: id, waitingOnHuman }) as unknown as AgentRow;

function mount(transport: Transport, agent = "atlas") {
  const root = document.createElement("div");
  document.body.append(root);
  const disposeView = render(() => <Transcript transport={transport} stream={`agent/${agent}`} source="live" />, root);
  disposers.push(disposeView);
  let inbox!: ReturnType<typeof createPermissionInbox>;
  const disposeInbox = createRoot((dispose) => {
    inbox = createPermissionInbox(transport);
    return dispose;
  });
  disposers.push(disposeInbox);
  inbox.track([row(agent, ["r1"])]);
  return { root, inbox, disposeView, disposeInbox };
}

test("a transcript and the permission inbox on one agent open one subscription; the last release closes it", () => {
  const wire = counting();
  const { inbox, disposeView } = mount(wire.transport);
  expect(wire.opened).toEqual(["agent/atlas"]);
  disposeView();
  expect(wire.closes()).toBe(0);
  inbox.track([row("atlas", ["r1"])]);
  inbox.close();
  expect(wire.closes()).toBe(1);
  expect(wire.opened).toEqual(["agent/atlas"]);
});

test("a reset reaches both readers once: the transcript clears and earlier prompts are gone", () => {
  const wire = counting();
  const { root, inbox } = mount(wire.transport);
  wire.send(open("r1"));
  wire.synced();
  expect(root.querySelectorAll('[data-testid="known-event"]')).toHaveLength(1);
  expect(inbox.cards()[0]?.detail).toContain("true");
  const resets = [0, 0];
  const store = sessionStore(wire.transport);
  const readers = resets.map((_, index) => store.read("agent/atlas", { current: (current) => void (!current && resets[index]!++) }));
  resets.fill(0);
  wire.reset();
  expect(resets).toEqual([1, 1]);
  readers.forEach((reader) => reader.release());
  expect(root.querySelectorAll('[data-testid="known-event"]')).toHaveLength(0);
  expect(root.querySelector('[data-testid="transcript-synced"]')).toBeNull();
  expect(inbox.cards()[0]?.detail).toBe("Loading details…");
});

test("both views derive from the store's one record: an entry applied once shows in the transcript and as a card", () => {
  const wire = counting();
  const { root, inbox } = mount(wire.transport);
  const first = sessionStore(wire.transport).read("agent/atlas");
  const second = sessionStore(wire.transport).read("agent/atlas");
  expect(first.turns).toBe(second.turns);
  expect(first.permissions).toBe(second.permissions);
  expect(wire.opened).toEqual(["agent/atlas"]);
  expect(first.count()).toBe(0);
  wire.send(open("r1"));
  expect(first.count()).toBe(1);
  expect(first.permissions.open.get("r1")?.seq).toBe(1);
  expect(root.querySelectorAll('[data-testid="known-event"]')).toHaveLength(1);
  expect(inbox.cards()[0]?.detail).toContain("true");
  first.release();
  second.release();
});

test("a reader that joins late is told the entries and progress the stream already holds", () => {
  const wire = counting();
  const store = sessionStore(wire.transport);
  const early = store.read("agent/atlas");
  wire.send(open("r1"));
  wire.synced();
  const told: string[] = [];
  const late = store.read("agent/atlas", { entry: (entry) => void told.push(`entry ${entry.seq}`), current: (current) => void told.push(`current ${current}`) });
  expect(told).toEqual(["entry 1", "current true"]);
  expect(wire.opened).toEqual(["agent/atlas"]);
  early.release();
  late.release();
  expect(wire.closes()).toBe(1);
  // The entry is dropped with the last reader: the next one subscribes again.
  store.read("agent/atlas").release();
  expect(wire.opened).toEqual(["agent/atlas", "agent/atlas"]);
});

test("a double release is harmless and never closes a stream another reader holds", () => {
  const wire = counting();
  const store = sessionStore(wire.transport);
  const first = store.read("agent/atlas");
  const second = store.read("agent/atlas");
  first.release();
  first.release();
  expect(wire.closes()).toBe(0);
  second.release();
  second.release();
  expect(wire.closes()).toBe(1);
});

test("after a failure a new transcript opens a fresh subscription while the inbox still holds the dead one", () => {
  const wire = counting();
  const { inbox } = mount(wire.transport);
  wire.observer(0).failed({ message: "boom" });
  expect(inbox.cards()[0]?.detail).toBe("Details unavailable: boom");
  const root = document.createElement("div");
  document.body.append(root);
  disposers.push(render(() => <Transcript transport={wire.transport} stream="agent/atlas" source="live" />, root));
  expect(wire.opened).toEqual(["agent/atlas", "agent/atlas"]);
  wire.observer(1).entry(classifyStoredLine(1, JSON.stringify(open("r1"))));
  wire.observer(1).synced();
  expect(root.querySelectorAll('[data-testid="known-event"]')).toHaveLength(1);
  expect(root.textContent).not.toContain("Couldn't load");
  // The inbox's reader keeps the failure it saw.
  expect(inbox.cards()[0]?.detail).toBe("Details unavailable: boom");
});

test("releasing a failed record leaves the newer record for the stream and its subscription open", () => {
  const wire = counting();
  const store = sessionStore(wire.transport);
  const old = store.read("agent/atlas");
  wire.observer(0).failed({ message: "boom" });
  expect(old.progress().phase).toBe("failed");
  const fresh = store.read("agent/atlas");
  expect(wire.opened).toHaveLength(2);
  old.release();
  expect(wire.closed).toEqual([true, false]);
  const shared = store.read("agent/atlas");
  expect(wire.opened).toHaveLength(2);
  fresh.release();
  shared.release();
  expect(wire.closed).toEqual([true, true]);
});

test("a permission notice expires on the next synced without the inbox writing the store's fold", () => {
  const wire = counting();
  const { inbox } = mount(wire.transport);
  wire.send({ ...open("stranger"), request: 5 });
  expect(inbox.cards().map((card) => card.id)).toContain("atlas:invalid:stranger");
  wire.synced();
  expect(inbox.cards().map((card) => card.id)).not.toContain("atlas:invalid:stranger");
  const held = sessionStore(wire.transport).read("agent/atlas");
  expect(held.permissions.invalid.has("stranger")).toBe(true);
  held.release();
});
