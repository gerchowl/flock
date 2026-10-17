/**
 * Test harness for the wire connection: a virtual clock, scripted sockets,
 * and an executor for S7a's fixture steps (`test/fixtures/wire/README.md`).
 *
 * Nothing here waits on wall-clock time (ADR-0037). Every wait is a bounded
 * advance of the virtual clock, and a missing effect fails at its ceiling.
 * A step that fails throws with the step's index and the reason.
 */
import { expect } from "bun:test";

import type { ClientFrame } from "../../src/gen/ClientFrame";
import type { FixtureLine } from "../../src/gen/FixtureLine";
import type { ServerFrame } from "../../src/gen/ServerFrame";
import type { WireEvent } from "../../src/gen/WireEvent";
import { WireConnection, type Refusal, type StreamName, type Subscription } from "../../src/wire/connection";
import { memoryStore, type Clock, type SocketLike, type TimerHandle } from "../../src/wire/env";
import { OUTBOX_KEY } from "../../src/wire/outbox";

/**
 * S7a's stage ceilings (its README), kept apart from the connection's own
 * constants so a wrong deadline in the client cannot also move the test's.
 */
const CEILINGS = { connect: 10_000, handshake: 5_000, liveness: 45_000, progress: 30_000, ack: 30_000 } as const;

type Timer = { readonly at: number; readonly order: number; readonly callback: () => void };

/** A virtual monotonic clock; wall time is the same virtual time. */
export class VirtualClock implements Clock {
  #now: number;
  #order = 0;
  #timers = new Map<number, Timer>();

  constructor(start = 0) {
    this.#now = start;
  }

  now(): number {
    return this.#now;
  }

  wallNow(): number {
    return this.#now;
  }

  setTimeout(callback: () => void, ms: number): TimerHandle {
    const order = ++this.#order;
    this.#timers.set(order, { at: this.#now + Math.max(0, ms), order, callback });
    return order as unknown as TimerHandle;
  }

  clearTimeout(handle: TimerHandle): void {
    this.#timers.delete(handle as unknown as number);
  }

  /** The next timer due at or before `limit`, earliest and oldest first. */
  #next(limit: number): Timer | undefined {
    let next: Timer | undefined;
    for (const timer of this.#timers.values()) {
      if (timer.at > limit) continue;
      if (next === undefined || timer.at < next.at || (timer.at === next.at && timer.order < next.order)) next = timer;
    }
    return next;
  }

  /**
   * Fires due timers one at a time up to `limit` and stops after the first
   * that makes `done()` true, resting at that timer's instant. Otherwise it
   * rests at `limit`. Returns whether `done()` became true.
   */
  advanceUntil(limit: number, done: () => boolean): boolean {
    if (limit < this.#now) throw new Error(`cannot advance backwards to ${limit} from ${this.#now}`);
    for (let timer = this.#next(limit); timer !== undefined; timer = this.#next(limit)) {
      this.#timers.delete(timer.order);
      this.#now = timer.at;
      timer.callback();
      if (done()) return true;
    }
    this.#now = limit;
    return false;
  }

  /** Fires every timer due up to `limit` and rests at `limit`. */
  advanceTo(limit: number): void {
    this.advanceUntil(limit, () => false);
  }
}

/** A scripted socket: records what the client sends and closes. */
export class FakeSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onmessage: ((event: { readonly data: unknown }) => void) | null = null;
  onclose: ((event: { readonly code: number; readonly reason: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  readonly #net: FakeNetwork;

  constructor(net: FakeNetwork) {
    this.#net = net;
  }

  send(text: string): void {
    this.#net.frames.push({ text, at: this.#net.clock.now() });
  }

  close(code: number, reason: string): void {
    this.#net.closes.push({ code, reason, at: this.#net.clock.now(), consumed: false });
  }

  /** The server's side of the socket. */
  open(): void {
    this.onopen?.();
  }

  deliver(frame: ServerFrame | string): void {
    this.onmessage?.({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
  }

  serverClose(code: number, reason: string): void {
    this.onclose?.({ code, reason });
  }

  drop(): void {
    this.onerror?.();
    this.onclose?.({ code: 1006, reason: "" });
  }
}

/** Every attempt, frame and close the client made, in order. */
export class FakeNetwork {
  readonly attempts: { readonly url: string; readonly at: number; readonly socket: FakeSocket; claimed: boolean }[] = [];
  readonly frames: { readonly text: string; readonly at: number }[] = [];
  readonly closes: { readonly code: number; readonly reason: string; readonly at: number; consumed: boolean }[] = [];
  readonly clock: VirtualClock;

  constructor(clock: VirtualClock) {
    this.clock = clock;
  }

  readonly factory = (url: string): SocketLike => {
    const socket = new FakeSocket(this);
    this.attempts.push({ url, at: this.clock.now(), socket, claimed: false });
    return socket;
  };

  /** The latest attempt's socket. */
  current(): FakeSocket {
    const socket = this.attempts.at(-1)?.socket;
    if (socket === undefined) throw new Error("no connection attempt yet");
    return socket;
  }

  /** The socket before the latest attempt's. */
  previous(): FakeSocket {
    const socket = this.attempts.at(-2)?.socket;
    if (socket === undefined) throw new Error("no previous socket");
    return socket;
  }
}

/** What a subscribed handler saw, for assertions. */
export type Seen =
  | { readonly kind: "event"; readonly seq: number; readonly ev: WireEvent }
  | { readonly kind: "gap"; readonly from: number; readonly to: number }
  | { readonly kind: "snapshot"; readonly lines: readonly WireEvent[] }
  | { readonly kind: "synced" | "reset" }
  | { readonly kind: "refused"; readonly refusal: Refusal };

type Step = FixtureLine;

/** The fixture credential provider's tokens (S7a's README). */
export const FIXTURE_TOKEN = "fixture-token";

function fail(message: string): never {
  throw new Error(message);
}

function deepEqual(a: unknown, b: unknown): boolean {
  return JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (typeof value === "object" && value !== null) {
    return Object.fromEntries(Object.entries(value).sort(([a], [b]) => (a < b ? -1 : 1)).map(([k, v]) => [k, canonical(v)]));
  }
  return value;
}

/** Runs S7a's steps against a real {@link WireConnection}. */
export class Harness {
  clock = new VirtualClock();
  net = new FakeNetwork(this.clock);
  store = memoryStore();
  conn: WireConnection | null = null;
  readonly seen = new Map<string, Seen[]>();
  /** The script's subscriptions, which follow their ids across re-subs. */
  readonly #subscriptions: Subscription[] = [];
  readonly #random: () => number;
  #transitions: string[] = [];
  #changes = 0;
  #consumedFrames = 0;
  /** When each client frame naming a sub or cmd went out, by `sub:`/`cmd:` key. */
  #origins = new Map<string, number>();
  #openedAt = 0;
  #lastServerFrameAt = 0;

  constructor(random: () => number = () => 0) {
    this.#random = random;
  }

  /** The connection, after `given`. */
  get connection(): WireConnection {
    return this.conn ?? fail("no given step yet");
  }

  /** Runs every step, then requires that no output is left unasserted. */
  run(steps: readonly Step[]): void {
    steps.forEach((step, index) => {
      try {
        this.step(step);
      } catch (error) {
        throw new Error(`step ${index + 1} (${step.step}): ${(error as Error).message}`);
      }
    });
    this.#requireQuiet("at the end of the script");
  }

  step(step: Step): void {
    if (step.v !== 1) fail(`fixture version ${step.v}`);
    this.#collectOrigins();
    switch (step.step) {
      case "given":
        return this.#given(step);
      case "client":
        return this.#client(step.action);
      case "expect_connect":
        return this.#expectConnect(step.not_before_ms ?? 0, step.within_ms);
      case "open":
        this.#openedAt = this.clock.now();
        this.#lastServerFrameAt = this.clock.now();
        return this.net.current().open();
      case "send":
        if ((step.socket ?? "current") === "current") {
          this.#lastServerFrameAt = this.clock.now();
          if ("sub_id" in step.frame) this.#origins.set(`sub:${step.frame.sub_id}`, this.clock.now());
          return this.net.current().deliver(step.frame);
        }
        return this.net.previous().deliver(step.frame);
      case "expect":
        return this.#expect(step.frame, step.fields, step.absent ?? []);
      case "expect_close":
        return this.#expectClose(step.code);
      case "observe":
        return this.#observe(step);
      case "deadline":
        return this.#deadline(step.stage);
      case "heartbeat_missed":
        return this.#untilEffect(this.#lastServerFrameAt + CEILINGS.liveness, "heartbeat_missed");
      case "close":
        if (step.code === 4401 || step.code === 4403) fail("close cannot carry 4401/4403; use token_refused");
        return this.net.current().serverClose(step.code, step.reason);
      case "drop":
        return this.net.current().drop();
      case "token_refused":
        if (step.code !== 4401 && step.code !== 4403) fail(`token_refused with ${step.code}`);
        return this.net.current().serverClose(step.code, "");
      case "quiet":
        return this.#quiet(step.ceiling_ms);
    }
  }

  #given(step: Extract<Step, { step: "given" }>): void {
    if (this.conn !== null) fail("a second given");
    if (step.resync !== undefined && Object.keys(step.resync).length > 0) fail("resync markers are G3b's (#1595)");
    if (step.idempotency_window_ms != null) fail("window expiry is G3b's (#1595)");
    this.clock = new VirtualClock(step.now_ms ?? 0);
    this.net = new FakeNetwork(this.clock);
    if (step.outbox.length > 0) {
      const principal = step.outbox[0]?.principal ?? null;
      this.store.setItem(OUTBOX_KEY, JSON.stringify({ principal, commands: step.outbox }));
    }
    this.conn = new WireConnection({
      url: "ws://fixture.invalid/v1/ws",
      token: FIXTURE_TOKEN,
      client: { name: "fixture", version: "1" },
      clock: this.clock,
      sockets: this.net.factory,
      random: this.#random,
      store: this.store,
      cursors: step.cursors,
    });
    this.conn.onTransition((state) => this.#transitions.push(state));
    this.conn.onChange(() => this.#changes++);
  }

  #client(action: Extract<Step, { step: "client" }>["action"]): void {
    const conn = this.connection;
    switch (action.kind) {
      case "connect":
        return conn.connect();
      case "disconnect":
        return conn.disconnect();
      case "set_token":
        return conn.setToken(`${FIXTURE_TOKEN}:${action.principal}`);
      case "network_change":
        return conn.networkChange();
      case "foreground":
        return conn.foreground();
      case "subscribe": {
        const seen: Seen[] = [];
        this.seen.set(action.sub_id, seen);
        const subscription = conn.subscribe(action.stream as StreamName, {
          event: (seq, ev) => seen.push({ kind: "event", seq, ev }),
          gap: (from, to) => seen.push({ kind: "gap", from, to }),
          synced: () => seen.push({ kind: "synced" }),
          snapshot: (lines) => seen.push({ kind: "snapshot", lines }),
          reset: () => seen.push({ kind: "reset" }),
          refused: (refusal) => seen.push({ kind: "refused", refusal }),
        });
        // The connection allocates every id; a script names the one it expects, in order.
        if (subscription.id !== action.sub_id) fail(`the script subscribes ${action.sub_id}, but the connection allocated ${subscription.id}`);
        this.#subscriptions.push(subscription);
        return;
      }
      case "unsubscribe":
        // The script names the id on the wire now, which a re-sub may have moved.
        return this.#subscriptions.find((subscription) => subscription.id === action.sub_id)?.close();
      case "command":
        conn.command(action.verb, action.args, action.cmd_id);
        return;
    }
  }

  /** Records when each `sub`/`cmd` frame went out, for deadline origins. */
  #collectOrigins(): void {
    for (const frame of this.net.frames) {
      const parsed = JSON.parse(frame.text) as ClientFrame;
      if (parsed.t === "sub") this.#origins.set(`sub:${parsed.sub_id}`, Math.max(frame.at, this.#origins.get(`sub:${parsed.sub_id}`) ?? 0));
      if (parsed.t === "cmd") this.#origins.set(`cmd:${parsed.cmd_id}`, frame.at);
    }
  }

  #effects(): number {
    return this.net.frames.length + this.net.closes.length + this.net.attempts.length + this.#changes + this.#transitions.length;
  }

  #expectConnect(notBefore: number, within: number): void {
    if (within <= 0) fail("within_ms must be positive");
    const entry = this.clock.now();
    const unclaimed = (): (typeof this.net.attempts)[number] | undefined => this.net.attempts.find((attempt) => !attempt.claimed);
    const early = unclaimed();
    if (early !== undefined && early.at < entry) fail(`an unasserted attempt at ${early.at}`);
    if (early === undefined && !this.clock.advanceUntil(entry + within, () => unclaimed() !== undefined)) {
      fail(`no connection attempt within ${within} ms`);
    }
    const attempt = unclaimed() ?? fail("no attempt");
    attempt.claimed = true;
    const delay = attempt.at - entry;
    if (delay < notBefore || delay > within) fail(`attempt after ${delay} ms, outside [${notBefore}, ${within}]`);
  }

  #expect(example: ClientFrame, fields: readonly string[], absent: readonly string[]): void {
    const next = this.net.frames[this.#consumedFrames];
    if (next === undefined) fail(`no client frame; expected ${example.t}`);
    this.#consumedFrames++;
    const actual = JSON.parse(next.text) as Record<string, unknown>;
    const wanted = example as Record<string, unknown>;
    if (actual["t"] !== example.t) fail(`expected ${example.t}, got ${next.text}`);
    for (const field of fields) {
      if (absent.includes(field)) fail(`${field} is both selected and absent`);
      if (!(field in wanted)) fail(`selected field ${field} is not in the example`);
      if (!deepEqual(actual[field], wanted[field])) fail(`${example.t}.${field}: expected ${JSON.stringify(wanted[field])}, got ${JSON.stringify(actual[field])}`);
    }
    for (const field of absent) {
      if (field in actual) fail(`${example.t}.${field} must be absent, got ${JSON.stringify(actual[field])}`);
    }
  }

  #expectClose(code: number): void {
    const close = this.net.closes.find((entry) => !entry.consumed) ?? fail(`no client close; expected ${code}`);
    close.consumed = true;
    if (close.code !== code) fail(`client closed with ${close.code}, expected ${code}`);
  }

  #observe(step: Extract<Step, { step: "observe" }>): void {
    const conn = this.connection;
    const transitions = this.#transitions;
    this.#transitions = [];
    if (step.connection != null && conn.state !== step.connection) fail(`connection is ${conn.state}, expected ${step.connection}`);
    const streams = conn.streams();
    for (const [id, want] of Object.entries(step.streams ?? {})) {
      const got = streams.get(id) ?? fail(`no stream ${id}`);
      if (got.state !== want.state || got.cursor !== want.cursor) {
        fail(`stream ${id} is ${got.state}@${got.cursor}, expected ${want.state}@${want.cursor}`);
      }
    }
    const outbox = new Map(conn.commands().map((entry) => [entry.cmd_id, entry.status]));
    for (const [id, want] of Object.entries(step.outbox ?? {})) {
      if (outbox.get(id) !== want) fail(`command ${id} is ${outbox.get(id) ?? "absent"}, expected ${want}`);
    }
    for (const id of step.absent_streams ?? []) if (streams.has(id)) fail(`stream ${id} should be absent`);
    for (const id of step.absent_outbox ?? []) if (outbox.has(id)) fail(`command ${id} should be absent`);
    if (step.connection_transitions != null && !deepEqual(transitions, step.connection_transitions)) {
      fail(`transitions ${JSON.stringify(transitions)}, expected ${JSON.stringify(step.connection_transitions)}`);
    }
  }

  #deadline(stage: Extract<Step, { step: "deadline" }>["stage"]): void {
    switch (stage.kind) {
      case "connect": {
        const attempt = this.net.attempts.at(-1) ?? fail("no attempt to expire");
        return this.#untilEffect(attempt.at + CEILINGS.connect, "connect deadline");
      }
      case "handshake":
        return this.#untilEffect(this.#openedAt + CEILINGS.handshake, "handshake deadline");
      case "subscription": {
        const origin = this.#origins.get(`sub:${stage.sub_id}`) ?? fail(`no sub ${stage.sub_id} was sent`);
        return this.#untilEffect(origin + CEILINGS.progress, `progress deadline of ${stage.sub_id}`);
      }
      case "command_ack": {
        const origin = this.#origins.get(`cmd:${stage.cmd_id}`) ?? fail(`no cmd ${stage.cmd_id} was sent`);
        return this.#untilEffect(origin + CEILINGS.ack, `ack deadline of ${stage.cmd_id}`);
      }
    }
  }

  /**
   * Advances to `ceiling`, requiring no effect strictly before it and some
   * effect exactly at it. Client closes made by the expiry are its own.
   */
  #untilEffect(ceiling: number, what: string): void {
    if (ceiling <= this.clock.now()) fail(`${what} at ${ceiling} is not in the future (now ${this.clock.now()})`);
    const before = this.#effects();
    this.clock.advanceTo(ceiling - 1);
    if (this.#effects() !== before) fail(`an effect before the ${what} (${ceiling} ms)`);
    const closes = this.net.closes.length;
    this.clock.advanceTo(ceiling);
    if (this.#effects() === before) fail(`no effect at the ${what} (${ceiling} ms)`);
    for (const close of this.net.closes.slice(closes)) close.consumed = true;
  }

  #requireQuiet(when: string): void {
    const frame = this.net.frames[this.#consumedFrames];
    if (frame !== undefined) fail(`unexpected client frame ${when}: ${frame.text}`);
    const close = this.net.closes.find((entry) => !entry.consumed);
    if (close !== undefined) fail(`unexpected client close ${close.code} ${when}`);
    const attempt = this.net.attempts.find((entry) => !entry.claimed);
    if (attempt !== undefined) fail(`unexpected connection attempt at ${attempt.at} ${when}`);
  }

  #quiet(ceiling: number): void {
    if (ceiling <= 0) fail("ceiling_ms must be positive");
    this.#requireQuiet("before quiet");
    this.clock.advanceTo(this.clock.now() + ceiling);
    this.#requireQuiet("during quiet");
  }
}

type Body = Step extends infer L ? (L extends unknown ? Omit<L, "v"> : never) : never;

/** A fixture step without its version field. */
export const step = (body: Body): Step => ({ v: 1, ...body }) as Step;

/** The fixture server's `welcome`. */
export const WELCOME: ServerFrame = {
  t: "welcome",
  protocol: 1,
  server_version: "fixture",
  capabilities: ["read", "send"],
  principal: "fixture-principal",
  server_epoch: "e",
  idempotency_window_ms: 86_400_000,
};

/** given → connect → open → hello → welcome: Live, with no subscriptions. */
export function live(h: Harness, given: Partial<Extract<Step, { step: "given" }>> = {}): void {
  h.step(step({ step: "given", cursors: {}, outbox: [], ...given }));
  h.step(step({ step: "client", action: { kind: "connect" } }));
  h.step(step({ step: "expect_connect", within_ms: 1 }));
  h.step(step({ step: "open" }));
  h.step(step({ step: "expect", frame: { t: "hello", protocol: 1, client: { name: "fixture", version: "1" }, token: FIXTURE_TOKEN }, fields: ["protocol", "token"] }));
  h.step(step({ step: "send", frame: WELCOME }));
}

/** Reaches a state with a socket, from a fresh harness. */
export const REACH: { readonly [state: string]: (h: Harness) => void } = {
  Connecting: (h) => {
    h.step(step({ step: "given", cursors: {}, outbox: [] }));
    h.step(step({ step: "client", action: { kind: "connect" } }));
  },
  Hello: (h) => {
    REACH["Connecting"]?.(h);
    h.step(step({ step: "open" }));
  },
  Live: (h) => live(h),
};

/** Asserts close `code` takes the connection from its state to NeedsToken, to stay. */
export function expectNeedsToken(h: Harness, state: string, code: number): void {
  expect(h.connection.state).toBe(state as never);
  h.step(step({ step: "token_refused", code }));
  expect(h.connection.state).toBe("NeedsToken");
  expect(h.connection.error).toBe(code === 4401 ? "The server refused the token." : "The token was revoked.");
  const attempts = h.net.attempts.length;
  h.clock.advanceTo(h.clock.now() + 120_000);
  expect(h.net.attempts.length).toBe(attempts);
  h.connection.setToken("new-token");
  expect(h.connection.state).toBe("Connecting");
}

/** Every client frame sent so far, parsed. */
export function sentFrames(h: Harness): unknown[] {
  return h.net.frames.map((frame) => JSON.parse(frame.text));
}

/** Parses a JSON Lines script into typed steps. */
export function parseScript(text: string): Step[] {
  return text
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line) as Step);
}
