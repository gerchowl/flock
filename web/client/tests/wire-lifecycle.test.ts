/**
 * The connection lifecycle beyond S7a's scripts (#1747, first slice): auth
 * refusal, liveness, decoding, versions and backoff. All time is virtual
 * (ADR-0037).
 */
import { describe, expect, test } from "bun:test";

import type { ServerFrame } from "../src/gen/ServerFrame";
import { WIRE_PROTOCOL } from "../src/wire/connection";
import { decodeServerFrame } from "../src/wire/decode";
import { backoffDelay, endpointUrl } from "../src/wire/env";
import { createWireSender } from "../src/workbench/sender";
import { Harness, REACH, WELCOME, expectNeedsToken, live, sentFrames, step } from "./wire/harness";

describe("auth refusal → NeedsToken from every state with a socket", () => {
  for (const [state, setup] of Object.entries(REACH)) {
    for (const code of [4401, 4403]) {
      test(`${state} + close ${code}`, () => {
        const h = new Harness();
        setup(h);
        expectNeedsToken(h, state, code);
      });
    }
  }
});

describe("transient handshake loss retains credentials", () => {
  for (const code of [1001, 1006]) {
    test(`Hello + close ${code} reconnects with the same token`, () => {
      const h = new Harness();
      REACH["Hello"]?.(h);
      h.step(step({ step: "expect_connect", within_ms: 10_000 }));
      h.net.current().serverClose(code, "transport ended");
      expect(h.connection.state).toBe("Reconnecting");
      h.step(step({ step: "expect_connect", not_before_ms: 250, within_ms: 500 }));
      h.net.current().open();
      expect(h.connection.state).toBe("Hello");
      expect(JSON.parse(h.net.frames.at(-1)?.text ?? "null").token).toBe("fixture-token");
      expect(h.connection.error).not.toContain("token refused");
      h.net.current().deliver(WELCOME);
      expect(h.connection.state).toBe("Live");
    });
  }
});

describe("liveness", () => {
  test("each ping is answered at once; 45 s without a frame → Stale → Reconnecting", () => {
    const h = new Harness();
    live(h);
    h.clock.advanceTo(15_000);
    h.step(step({ step: "send", frame: { t: "ping", nonce: "n1" } }));
    expect(JSON.parse(h.net.frames.at(-1)?.text ?? "null")).toEqual({ t: "pong", nonce: "n1" });
    const transitions: string[] = [];
    h.connection.onTransition((state) => transitions.push(state));
    h.clock.advanceTo(15_000 + 44_999);
    expect(h.connection.state).toBe("Live");
    h.clock.advanceTo(15_000 + 45_000);
    expect(transitions).toEqual(["Stale", "Reconnecting"]);
    expect(h.connection.error).toContain("liveness window");
  });
});

describe("frames and versions", () => {
  test("WIRE_PROTOCOL matches the server's WIRE_JSON_VERSION", async () => {
    const rust = await Bun.file(`${import.meta.dir}/../../../src/structured_wire/mod.rs`).text();
    expect(rust).toContain(`pub const WIRE_JSON_VERSION: u32 = ${WIRE_PROTOCOL};`);
  });

  test("decoding refuses what the generated union does not allow", () => {
    expect(decodeServerFrame('{"t":"ping","nonce":"n"}')).toEqual({ ok: true, frame: { t: "ping", nonce: "n" } });
    expect(decodeServerFrame('{"t":"ping","nonce":"n","extra":1}').ok).toBe(true);
    expect(decodeServerFrame("{")).toEqual({ ok: false, reason: "a frame that is not JSON" });
    expect(decodeServerFrame('{"t":"pong","nonce":"n"}')).toEqual({ ok: false, reason: 'an unknown frame type "pong"' });
    expect(decodeServerFrame('{"t":"synced","sub_id":"s"}')).toEqual({ ok: false, reason: "synced without seq" });
    expect(decodeServerFrame('{"t":"synced","sub_id":"s","seq":"1"}')).toEqual({ ok: false, reason: "synced.seq is not a number" });
  });

  test("an undecodable frame closes with 1002 and reconnects", () => {
    const h = new Harness();
    live(h);
    h.net.current().deliver('{"t":"mystery"}');
    expect(h.net.closes.at(-1)?.code).toBe(1002);
    expect(h.connection.error).toContain('unknown frame type "mystery"');
    expect(h.connection.state).toBe("Reconnecting");
  });

  test("an older server keeps the client working normally", () => {
    const h = new Harness();
    live(h, {}, { ...WELCOME, protocol: WIRE_PROTOCOL + 1 } as ServerFrame);
    expect(h.connection.state).toBe("Live");
    expect(h.connection.readOnly).toBeNull();
  });

  test("the token goes only into hello, never into the URL", () => {
    const h = new Harness();
    live(h);
    expect(h.net.attempts[0]?.url).not.toContain("fixture-token");
    expect(endpointUrl("https://example.invalid:8443/some/page")).toBe("wss://example.invalid:8443/v1/ws");
    expect(endpointUrl("http://127.0.0.1:7000")).toBe("ws://127.0.0.1:7000/v1/ws");
  });

  test("backoff: equal jitter in [c/2, c], doubling from 500 ms, capped at 30 s", () => {
    for (let n = 0; n < 12; n++) {
      const ceiling = Math.min(30_000, 500 * 2 ** n);
      expect(backoffDelay(n, () => 0)).toBe(ceiling / 2);
      expect(backoffDelay(n, () => 0.999_999)).toBe(ceiling);
    }
    expect(backoffDelay(10_000, () => 0.5)).toBeLessThanOrEqual(30_000);
  });
});

describe("ReadOnly: a client newer than the server (ADR-0038 §3)", () => {
  const NEWER = WIRE_PROTOCOL + 1;
  /** A newer client, welcomed by a server one protocol behind it. */
  const readOnly = (): Harness => {
    const h = new Harness(() => 0, NEWER);
    live(h, {}, { ...WELCOME, protocol: WIRE_PROTOCOL } as ServerFrame, NEWER);
    return h;
  };
  const cmds = (h: Harness): unknown[] => sentFrames(h).filter((frame) => (frame as { t: string }).t === "cmd");

  test("the welcome enters ReadOnly, not Syncing or a disconnect, and names the gap", () => {
    const h = readOnly();
    expect(h.connection.state).toBe("ReadOnly");
    expect(h.connection.error).toBeNull();
    expect(h.connection.readOnly).toContain(`wire protocol ${WIRE_PROTOCOL}, older than this client's ${NEWER}`);
  });

  test("streams render and settle, and the connection stays ReadOnly", () => {
    const h = readOnly();
    h.step(step({ step: "client", action: { kind: "subscribe", sub_id: "s1", stream: "agent/a" } }));
    h.step(step({ step: "expect", frame: { t: "sub", sub_id: "s1", stream: "agent/a" }, fields: ["sub_id", "stream"], absent: ["after"] }));
    h.step(step({ step: "send", frame: { t: "ev", sub_id: "s1", stream: "agent/a", seq: 1, ts_ms: 1, ev: { type: "x" } } }));
    h.step(step({ step: "send", frame: { t: "synced", sub_id: "s1", seq: 1 } }));
    expect(h.seen.get("s1")).toEqual([{ kind: "event", seq: 1, ev: { type: "x" } }, { kind: "synced" }]);
    expect(h.connection.streams().get("s1")).toEqual({ stream: "agent/a", state: "Current", cursor: 1 });
    expect(h.connection.state).toBe("ReadOnly");
    h.step(step({ step: "send", frame: { t: "ping", nonce: "n1" } }));
    expect(sentFrames(h).at(-1)).toEqual({ t: "pong", nonce: "n1" });
  });

  test("commands are held, never sent or dropped, and the controls say why", () => {
    const h = readOnly();
    const sender = createWireSender(h.connection, () => 0);
    expect(sender.blocked()).toBe(h.connection.readOnly);
    const id = h.connection.command("queue.add", {}, "c1");
    h.clock.advanceTo(h.clock.now() + 40_000);
    h.step(step({ step: "send", frame: { t: "ping", nonce: "n" } }));
    expect(h.connection.commands().map((entry) => [entry.cmd_id, entry.status])).toEqual([[id, "pending"]]);
    expect(cmds(h)).toEqual([]);
  });

  test("a lost socket reconnects, and the next welcome decides again: Live sends the held command", () => {
    const h = readOnly();
    h.connection.command("queue.add", {}, "c1");
    h.step(step({ step: "drop" }));
    expect(h.connection.state).toBe("Reconnecting");
    h.step(step({ step: "expect_connect", not_before_ms: 250, within_ms: 500 }));
    h.step(step({ step: "open" }));
    h.step(step({ step: "send", frame: { ...WELCOME, protocol: NEWER } as ServerFrame }));
    expect(h.connection.state).toBe("Live");
    expect(h.connection.readOnly).toBeNull();
    expect(cmds(h)).toEqual([{ t: "cmd", cmd_id: "c1", verb: "queue.add", args: {}, resume_only: false }]);
  });

  test("a missed heartbeat goes Stale, then Reconnecting", () => {
    const h = readOnly();
    const states: string[] = [];
    h.connection.onTransition((state) => states.push(state));
    h.step(step({ step: "heartbeat_missed" }));
    expect(states).toEqual(["Stale", "Reconnecting"]);
  });

  test("foreground closes the possibly suspended socket and connects again", () => {
    const h = readOnly();
    h.connection.foreground();
    expect(h.net.closes.at(-1)?.code).toBe(1000);
    expect(h.connection.state).toBe("Connecting");
  });
});
