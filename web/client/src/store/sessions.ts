/**
 * The session store: the one owner of every `agent/<id>` stream's entities
 * (ADR-0038 §3, "The store"; §2 `close(tile)`).
 *
 * "The store is their only owner: views read and subscribe to it, and hold no
 * copy of a stream's entities." A reader asks for a stream with
 * {@link SessionStore.read}; the store opens one `transport.subscribe` on the
 * first reader and closes it on the last reader's {@link StreamReader.release},
 * as §2 has `close(tile)` unsubscribe only "if no other tile shares" the
 * stream. `reset`, `synced` and `failed` are applied here once, and every
 * reader sees the result.
 *
 * Per stream the store holds:
 * - the entries, in seq order, which are the input of a regroup;
 * - the turn fold (tool runs by `tool_use` id, permission twins by request id),
 *   which is a presentation derivation (§3: "grouping by turn");
 * - the permission records by `request_id`: open, closed and unusable;
 * - the stream's progress.
 *
 * It derives nothing the server has not sent (§3): whether a permission is
 * open is the server's record, and which ids wait on a human is the fleet's.
 *
 * Lifetime: a stream's entry is dropped when its last reader releases. §3
 * leaves open whether it should outlive its readers; that is persistence's
 * question (a later slice), so until then a new first reader replays from the
 * start, as every subscription did before the store.
 *
 * A late reader is told the entries the stream already holds, then whether it
 * is Current, so it sees what a reader that was there from the start saw.
 *
 * A `failed` stream is over for good (the wire ends that `sub`, §3): its record
 * leaves the store at once, readers that hold it keep the failure they saw, and
 * the next reader opens a fresh subscription. Releasing a failed record never
 * touches a newer record for the same stream.
 *
 * Memory: a stream's entries and turn fold are held while any reader holds it,
 * so a permission-only agent holds them too, for as long as it is tracked.
 *
 * @module
 */
import { createSignal, type Accessor } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";

import type { SessionEntry, SessionStream, Subscription, Transport, TransportFailure } from "../transport";
import { emptyFold, foldAll, foldEntry, type TurnFold } from "../views/turns";
import { permissionRecord, type OpenPermissionRecord } from "../wire/permission-record";

/** Where a stream is: replaying history, Current (`synced`), or ended with a failure. */
export type Progress = { readonly phase: "replaying" | "synced" } | { readonly phase: "failed"; readonly failure: TransportFailure };

/** An open permission record, with the stream seq and server time it arrived at. */
export type OpenPermission = { readonly seq: number; readonly tsMs: number; readonly record: OpenPermissionRecord };

/** A record that did not match the decided shape, with the {@link PermissionFold.syncs} count it arrived at. */
export type InvalidPermission = { readonly id: string | null; readonly problem: string; readonly syncs: number };

/**
 * The permission records of one stream, by `request_id`. Readers never write
 * it: what a reader hides or expires is its own state, derived from this.
 */
export type PermissionFold = {
  /** Open records; a closed record removes its entry. */
  readonly open: ReadonlyMap<string, OpenPermission>;
  /** Ids whose closed record arrived. */
  readonly closed: ReadonlySet<string>;
  /** Records that did not match the decided shape, by `request_id` (or a per-entry key when unreadable). */
  readonly invalid: ReadonlyMap<string, InvalidPermission>;
  /** A history gap arrived and nothing has followed it yet. */
  readonly stale: boolean;
  /** How many times the stream has reported `synced`; it keeps counting across a `reset`. */
  readonly syncs: number;
};

type MutableFold = { open: Map<string, OpenPermission>; closed: Set<string>; invalid: Map<string, InvalidPermission>; stale: boolean; syncs: number };

/** What a reader is told as the stream changes; every part is optional. */
export type ReaderListener = {
  /** Each entry, in seq order; a late reader gets the entries already held first, then `current`. */
  readonly entry?: (entry: SessionEntry) => void;
  /** False when the stream is replaying (at the start and after a `reset`), true once `synced`. */
  readonly current?: (current: boolean) => void;
  /** After any change to the stream's state. */
  readonly change?: () => void;
};

/** One reader's view of a stream: the store's record, read-only, and the release. */
export type StreamReader = {
  readonly turns: TurnFold;
  readonly count: Accessor<number>;
  readonly progress: Accessor<Progress>;
  readonly permissions: PermissionFold;
  /** Gives the stream up; the last release closes the subscription. Idempotent. */
  readonly release: () => void;
};

/** The store of one transport's session streams. */
export interface SessionStore {
  read(stream: SessionStream, listener?: ReaderListener): StreamReader;
}

type StreamRecord = {
  readonly entries: SessionEntry[];
  readonly turns: TurnFold;
  readonly count: Accessor<number>;
  readonly progress: Accessor<Progress>;
  readonly permissions: MutableFold;
  readonly listeners: Set<ReaderListener>;
  subscription: Subscription | null;
};

function applyPermission(fold: MutableFold, entry: SessionEntry): void {
  if (entry.kind === "gap") {
    fold.stale = true;
    return;
  }
  fold.stale = false;
  if (entry.kind !== "known") return;
  const record = permissionRecord(entry.event);
  if (record === null) return;
  if (record.state === "invalid") {
    fold.invalid.set(record.request_id ?? `seq-${entry.seq}`, { id: record.request_id, problem: record.problem, syncs: fold.syncs });
  } else if (record.state === "open") {
    fold.invalid.delete(record.request_id);
    fold.open.set(record.request_id, { seq: entry.seq, tsMs: entry.tsMs ?? 0, record });
    fold.closed.delete(record.request_id);
  } else {
    fold.invalid.delete(record.request_id);
    fold.open.delete(record.request_id);
    fold.closed.add(record.request_id);
  }
}

function createSessionStore(transport: Transport): SessionStore {
  const records = new Map<SessionStream, StreamRecord>();

  const open = (stream: SessionStream): StreamRecord => {
    const entries: SessionEntry[] = [];
    const [turns, setTurns] = createStore<TurnFold>(emptyFold());
    const [count, setCount] = createSignal(0);
    const [progress, setProgress] = createSignal<Progress>({ phase: "replaying" });
    const permissions: MutableFold = { open: new Map(), closed: new Set(), invalid: new Map(), stale: false, syncs: 0 };
    const record: StreamRecord = { entries, turns, count, progress, permissions, listeners: new Set(), subscription: null };
    const each = (call: (listener: ReaderListener) => void): void => {
      const readers = [...record.listeners];
      for (const listener of readers) if (record.listeners.has(listener)) call(listener);
      for (const listener of readers) if (record.listeners.has(listener)) listener.change?.();
    };
    record.subscription = transport.subscribe(stream, null, {
      entry: (entry) => {
        entries.push(entry);
        setCount(entries.length);
        let regroup = false;
        setTurns(produce((fold) => void (regroup = !foldEntry(fold, entry))));
        if (regroup) setTurns(reconcile(foldAll(entries), { key: "key" }));
        applyPermission(permissions, entry);
        each((listener) => listener.entry?.(entry));
      },
      reset: () => {
        entries.length = 0;
        setCount(0);
        setTurns(emptyFold());
        permissions.open.clear();
        permissions.closed.clear();
        permissions.invalid.clear();
        permissions.stale = false;
        // Entries restart from the stream's first retained seq: replaying again, not
        // Current, until the new epoch's `synced`.
        setProgress({ phase: "replaying" });
        each((listener) => listener.current?.(false));
      },
      synced: () => {
        setProgress({ phase: "synced" });
        permissions.stale = false;
        permissions.syncs++;
        each((listener) => listener.current?.(true));
      },
      failed: (failure) => {
        setProgress({ phase: "failed", failure });
        // The subscription is over for good: the next reader opens a fresh one (§3, a Refused `sub` is final for that `sub`).
        if (records.get(stream) === record) records.delete(stream);
        each(() => undefined);
      },
    });
    records.set(stream, record);
    return record;
  };

  return {
    read: (stream, listener = {}) => {
      const record = records.get(stream) ?? open(stream);
      record.listeners.add(listener);
      let released = false;
      const reader: StreamReader = {
        turns: record.turns,
        count: record.count,
        progress: record.progress,
        permissions: record.permissions,
        release: () => {
          if (released) return;
          released = true;
          record.listeners.delete(listener);
          if (record.listeners.size > 0) return;
          record.subscription?.close();
          // A failed record left the map already; a newer record for the stream is not ours to delete.
          if (records.get(stream) === record) records.delete(stream);
        },
      };
      // The order a reader that was there from the start saw: the entries, then Current.
      for (const entry of [...record.entries]) {
        if (released) return reader;
        listener.entry?.(entry);
      }
      if (!released) listener.current?.(record.progress().phase === "synced");
      return reader;
    },
  };
}

const stores = new WeakMap<Transport, SessionStore>();

/** The store for `transport`: every view over one transport shares it, so a stream is subscribed once. */
export function sessionStore(transport: Transport): SessionStore {
  const found = stores.get(transport);
  if (found !== undefined) return found;
  const created = createSessionStore(transport);
  stores.set(transport, created);
  return created;
}
