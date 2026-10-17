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
 * reload restores them with their status. Settled ones stay in memory for
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

/** The outbox; the connection drives it. */
export class Outbox {
  #principal: string | null = null;
  #commands = new Map<string, Live>();
  readonly #store: KeyValueStore;
  readonly #changed: () => void;

  /** Restores the persisted outbox from `store`. A corrupt record throws. */
  constructor(store: KeyValueStore, changed: () => void) {
    this.#store = store;
    this.#changed = changed;
    const raw = store.getItem(OUTBOX_KEY);
    if (raw === null) return;
    const stored = JSON.parse(raw) as Stored;
    if (!Array.isArray(stored.commands)) throw new Error(`${OUTBOX_KEY} holds no command list`);
    this.#principal = stored.principal;
    for (const entry of stored.commands) this.#commands.set(entry.cmd_id, { entry, timer: null, onSocket: false });
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
