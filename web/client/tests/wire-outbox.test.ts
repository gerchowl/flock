/**
 * The command outbox and `cmd` (#1747, third slice): pending → sent →
 * acked/failed, uncertain at the ack deadline, and persistence across a
 * reload. All time is virtual (ADR-0037).
 */
import { describe, expect, test } from "bun:test";

import { OUTBOX_KEY } from "../src/wire/outbox";
import { uuidv7 } from "../src/wire/env";
import { Harness, live, sentFrames, step } from "./wire/harness";

describe("outbox", () => {
  test("pending → sent → acked, and an error ack → failed", () => {
    const h = new Harness();
    live(h);
    const ok = h.connection.command("agent.send", { target: "a", text: "hi" }, "c1");
    const bad = h.connection.command("agent.send", { target: "b", text: "hi" }, "c2");
    expect(h.connection.commands().map((entry) => entry.status)).toEqual(["sent", "sent"]);
    h.step(step({ step: "send", frame: { t: "ack", cmd_id: ok, ok: true, result: null } }));
    h.step(step({ step: "send", frame: { t: "ack", cmd_id: bad, ok: false, error: { code: "agent_not_found", message: "no", exit: 2, retry: "never" } } }));
    expect(h.connection.commands().map((entry) => [entry.status, entry.error?.code])).toEqual([["acked", undefined], ["failed", "agent_not_found"]]);
  });

  test("a command queued while not Live stays pending until Live", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    h.connection.command("agent.send", { target: "a", text: "hi" }, "c1");
    expect(h.connection.commands()[0]?.status).toBe("pending");
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 0 } }));
    expect(h.connection.state).toBe("Live");
    expect(h.connection.commands()[0]?.status).toBe("sent");
  });

  test("sent → uncertain at exactly 30 s, nothing resent, a late ack still settles it", () => {
    const h = new Harness();
    live(h);
    h.connection.command("agent.send", { target: "a", text: "hi" }, "c1");
    const sent = h.net.frames.length;
    h.clock.advanceTo(h.clock.now() + 29_999);
    expect(h.connection.commands()[0]?.status).toBe("sent");
    h.clock.advanceTo(h.clock.now() + 1);
    expect(h.connection.commands()[0]?.status).toBe("uncertain");
    expect(h.connection.state).toBe("Live");
    expect(h.net.frames.length).toBe(sent);
    h.step(step({ step: "send", frame: { t: "ack", cmd_id: "c1", ok: true } }));
    expect(h.connection.commands()[0]?.status).toBe("acked");
  });

  test("persisted across a reload: restored with its status, never resent as a first send", () => {
    const first = new Harness();
    live(first);
    first.connection.command("agent.send", { target: "a", text: "sent one" }, "c1");
    first.step(step({ step: "drop" }));
    first.connection.disconnect();
    first.connection.command("agent.send", { target: "a", text: "queued one" }, "c2");
    expect(JSON.parse(first.store.getItem(OUTBOX_KEY) ?? "null")).toEqual({
      principal: "fixture-principal",
      commands: [
        { cmd_id: "c1", verb: "agent.send", args: { target: "a", text: "sent one" }, principal: "fixture-principal", created_ms: 0, status: "sent", first_sent_ms: 0 },
        { cmd_id: "c2", verb: "agent.send", args: { target: "a", text: "queued one" }, principal: "fixture-principal", created_ms: 0, status: "pending" },
      ],
    });

    const reloaded = new Harness();
    reloaded.store = first.store;
    live(reloaded);
    expect(reloaded.connection.commands().map((entry) => [entry.cmd_id, entry.status])).toEqual([["c1", "sent"], ["c2", "sent"]]);
    const cmds = sentFrames(reloaded).filter((frame) => (frame as { t: string }).t === "cmd");
    expect(cmds).toEqual([{ t: "cmd", cmd_id: "c2", verb: "agent.send", args: { target: "a", text: "queued one" }, resume_only: false }]);
  });

  test("settled commands are not persisted", () => {
    const h = new Harness();
    live(h);
    h.connection.command("agent.send", {}, "c1");
    h.step(step({ step: "send", frame: { t: "ack", cmd_id: "c1", ok: true } }));
    expect(JSON.parse(h.store.getItem(OUTBOX_KEY) ?? "null").commands).toEqual([]);
  });

  test("no command can be queued before any principal is known", () => {
    const h = new Harness();
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    expect(() => h.connection.command("agent.send", {})).toThrow("no principal yet");
  });

  test("an ack for a command not awaiting one is a protocol error", () => {
    const h = new Harness();
    live(h);
    h.step(step({ step: "send", frame: { t: "ack", cmd_id: "nope", ok: true } }));
    expect(h.connection.state).toBe("Reconnecting");
    expect(h.net.closes.at(-1)?.code).toBe(1002);
  });

  test("the connection mints UUIDv7 cmd_ids, ordered by the clock", () => {
    const h = new Harness();
    live(h);
    h.connection.command("agent.send", { target: "a", text: "one", submit: true });
    h.clock.advanceTo(h.clock.now() + 5);
    h.connection.command("agent.send", { target: "a", text: "two", submit: true });
    const ids = sentFrames(h).flatMap((frame) => ((frame as { t: string }).t === "cmd" ? [(frame as { cmd_id: string }).cmd_id] : []));
    expect(ids).toHaveLength(2);
    for (const id of ids) expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    const [first, second] = ids as [string, string];
    expect(first < second).toBe(true);
    expect(parseInt(second.replace("-", "").slice(0, 12), 16) - parseInt(first.replace("-", "").slice(0, 12), 16)).toBe(5);
  });

  test("an unreadable stored record is dropped and named; the others restore", () => {
    const good = { cmd_id: "c0", verb: "agent.send", args: {}, principal: "fixture-principal", created_ms: 0, status: "pending", first_sent_ms: null };
    const h = new Harness();
    h.store.setItem(OUTBOX_KEY, JSON.stringify({ principal: "fixture-principal", commands: [good, { cmd_id: "c1", verb: 3 }, "junk", { ...good, cmd_id: "c2", status: "lost" }] }));
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    expect(h.connection.commands().map((entry) => entry.cmd_id)).toEqual(["c0"]);
    expect(h.connection.outboxProblem).toBe("Dropped unreadable stored commands: c1, record 3, c2.");

    const broken = new Harness();
    broken.store.setItem(OUTBOX_KEY, "{not json");
    broken.step(step({ step: "given", cursors: {}, outbox: [] }));
    expect(broken.connection.commands()).toEqual([]);
    expect(broken.connection.outboxProblem).toBe(`The stored outbox (${OUTBOX_KEY}) is not JSON; its commands were dropped.`);
  });

  test("generated cmd_ids are UUIDv7", () => {
    const id = uuidv7(0x0189_abcd_ef01, new Uint8Array(10).fill(0xff));
    expect(id).toBe("0189abcd-ef01-7fff-bfff-ffffffffffff");
    expect(uuidv7(1) < uuidv7(2)).toBe(true);
  });
});
