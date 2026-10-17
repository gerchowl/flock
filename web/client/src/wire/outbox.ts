/**
 * The command outbox (ADR-0038 §3), in G3a's minimal form (#1747).
 *
 * Each command is stored with its `cmd_id`, verb, args, the principal that
 * queued it and its first-send time, as the generated {@link QueuedCommand},
 * and persisted before its first send. G3a's transitions:
 *
 * - `pending` → `sent` on the first send (Live only, `resume_only: false`);
 * - `sent` → `acked` on an ok `ack`, → `failed` on an error `ack`;
 * - `sent` → `uncertain` at the 30 s ack deadline, with nothing resent on
 *   the same socket; a late `ack` on that socket still settles it.
 *
 * Unsettled commands (`pending`, `sent`, `uncertain`) are persisted, so a
 * reload restores them with their status. A stored record that is not a
 * readable command is dropped, never fatal: the others still restore, and
 * {@link Outbox.problem} names what was dropped. Settled ones stay in memory for
 * display and are not persisted, which bounds the stored outbox.
 *
 * Known limit, G3b (#1595): a `sent` or `uncertain` command whose socket is
 * lost keeps its status and is never resent, because the resume-only lookup
 * and its window expiry (`unknown_outcome`) are G3b's. A restored `sent` or
 * `uncertain` command is held the same way; it is never sent again as a
 * first send. A `welcome` with another principal drops every command queued
 * under the old one (commands must not run under another identity).
 *
 * @module
 */
import type { OutboxStatus } from "../gen/OutboxStatus";
import type { QueuedCommand } from "../gen/QueuedCommand";
import type { WireError } from "../gen/WireError";
import type { KeyValueStore, TimerHandle } from "./env";

/** The storage key; the version is the stored format's, not the wire's. */
export const OUTBOX_KEY = "flock.outbox.v1";

/** One outbox command, with the error that failed it, if any. */
export type OutboxEntry = QueuedCommand & { readonly error?: WireError };

type Stored = { readonly principal: string | null; readonly commands: readonly QueuedCommand[] };

type Live = {
  entry: OutboxEntry;
  /** The ack deadline of the current socket's send, and how to cancel it. */
  timer: { readonly handle: TimerHandle; readonly cancel: (handle: TimerHandle) => void } | null;
  /** Sent on the current socket, so an `ack` for it may still arrive. */
  onSocket: boolean;
};

const UNSETTLED: ReadonlySet<OutboxStatus> = new Set(["pending", "sent", "uncertain", "unknown_outcome"]);

/** Typed against {@link OutboxStatus}, so a new status fails `tsc` until listed. */
const STATUSES: { readonly [S in OutboxStatus]: true } = { pending: true, sent: true, uncertain: true, unknown_outcome: true, acked: true, failed: true };

type Json = { readonly [key: string]: unknown };
const isObject = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);

/** Whether a stored record is a readable {@link QueuedCommand}. */
function isCommand(value: unknown): value is QueuedCommand {
  return (
    isObject(value) &&
    typeof value["cmd_id"] === "string" &&
    typeof value["verb"] === "string" &&
    "args" in value &&
    typeof value["principal"] === "string" &&
    typeof value["created_ms"] === "number" &&
    typeof value["status"] === "string" &&
    Object.hasOwn(STATUSES, value["status"]) &&
    (value["first_sent_ms"] === undefined || value["first_sent_ms"] === null || typeof value["first_sent_ms"] === "number")
  );
}

/** The outbox; the connection drives it. */
export class Outbox {
  #principal: string | null = null;
  #commands = new Map<string, Live>();
  readonly #store: KeyValueStore;
  readonly #changed: () => void;
  #problem: string | null = null;

  /** Restores the persisted outbox from `store`, dropping what it cannot read. */
  constructor(store: KeyValueStore, changed: () => void) {
    this.#store = store;
    this.#changed = changed;
    const raw = store.getItem(OUTBOX_KEY);
    if (raw === null) return;
    let stored: unknown;
    try {
      stored = JSON.parse(raw);
    } catch {
      this.#problem = `The stored outbox (${OUTBOX_KEY}) is not JSON; its commands were dropped.`;
      return;
    }
    const commands = isObject(stored) ? stored["commands"] : undefined;
    const principal = isObject(stored) ? stored["principal"] : undefined;
    if (!Array.isArray(commands) || (principal !== null && typeof principal !== "string")) {
      this.#problem = `The stored outbox (${OUTBOX_KEY}) holds no command list; it was dropped.`;
      return;
    }
    this.#principal = principal;
    const dropped: string[] = [];
    commands.forEach((entry: unknown, index) => {
      if (isCommand(entry)) this.#commands.set(entry.cmd_id, { entry, timer: null, onSocket: false });
      else dropped.push(isObject(entry) && typeof entry["cmd_id"] === "string" ? entry["cmd_id"] : `record ${index + 1}`);
    });
    if (dropped.length > 0) this.#problem = `Dropped unreadable stored ${dropped.length === 1 ? "command" : "commands"}: ${dropped.join(", ")}.`;
  }

  /** What restoring the stored outbox had to drop, or null. */
  get problem(): string | null {
    return this.#problem;
  }

  /** The principal the stored commands were queued under. */
  get principal(): string | null {
    return this.#principal;
  }

  /** Every command, oldest first. */
  entries(): OutboxEntry[] {
    return [...this.#commands.values()].map((live) => live.entry);
  }

  /** Pending commands, oldest first. */
  pending(): OutboxEntry[] {
    return this.entries().filter((entry) => entry.status === "pending");
  }

  /** Adds a new command; it is persisted before this returns. */
  add(entry: QueuedCommand): void {
    if (this.#commands.has(entry.cmd_id)) throw new Error(`command ${entry.cmd_id} is already queued`);
    this.#commands.set(entry.cmd_id, { entry, timer: null, onSocket: false });
    this.#commit();
  }

  /** A `welcome` named `principal`: drops commands queued under another. */
  adoptPrincipal(principal: string): void {
    if (this.#principal === principal) return;
    for (const [id, live] of this.#commands) {
      if (live.entry.principal !== principal) {
        this.#cancel(live);
        this.#commands.delete(id);
      }
    }
    this.#principal = principal;
    this.#commit();
  }

  /** The first send went out at `wallMs`; its ack deadline is `handle`. */
  sent(id: string, wallMs: number, handle: TimerHandle, cancel: (handle: TimerHandle) => void): void {
    const live = this.#require(id);
    live.entry = { ...live.entry, status: "sent", first_sent_ms: wallMs };
    live.timer = { handle, cancel };
    live.onSocket = true;
    this.#commit();
  }

  /** The ack deadline elapsed with the socket up. */
  uncertain(id: string): void {
    const live = this.#require(id);
    live.timer = null;
    live.entry = { ...live.entry, status: "uncertain" };
    this.#commit();
  }

  /**
   * An `ack` arrived. Returns false if `id` was not sent on the current
   * socket, which is a protocol error for the caller.
   */
  settle(id: string, error: WireError | null): boolean {
    const live = this.#commands.get(id);
    if (live === undefined || !live.onSocket) return false;
    this.#cancel(live);
    live.onSocket = false;
    live.entry = error === null ? { ...live.entry, status: "acked" } : { ...live.entry, status: "failed", error };
    this.#commit();
    return true;
  }

  /** The socket is gone: no ack can arrive for anything sent on it. */
  socketLost(): void {
    for (const live of this.#commands.values()) {
      this.#cancel(live);
      live.onSocket = false;
    }
  }

  #cancel(live: Live): void {
    if (live.timer !== null) live.timer.cancel(live.timer.handle);
    live.timer = null;
  }

  #require(id: string): Live {
    const live = this.#commands.get(id);
    if (live === undefined) throw new Error(`no command ${id}`);
    return live;
  }

  #commit(): void {
    const commands = this.entries()
      .filter((entry) => UNSETTLED.has(entry.status))
      .map(({ error: _error, ...entry }) => entry);
    const stored: Stored = { principal: this.#principal, commands };
    this.#store.setItem(OUTBOX_KEY, JSON.stringify(stored));
    this.#changed();
  }
}
