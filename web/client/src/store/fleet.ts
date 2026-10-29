/**
 * The fleet store: the one owner of the `fleet` stream's entities (ADR-0038
 * §3, "The store"; §1's entity table; §9, Mesh).
 *
 * "Entities are kept normalised by kind and id", and "the store is their only
 * owner: views read and subscribe to it, and hold no copy." The wire source
 * (`wire/fleet.ts`) parses and validates; it hands the store {@link FleetChange}s
 * and keeps no rows. Three kinds are held, each under its own key:
 * - agents, by `id`, as presentation {@link AgentRow}s;
 * - nodes, by `node_id`;
 * - projects, by (`node_id`, `id`): a project's `id` is a cwd on one owning
 *   node, and two nodes can hold the same path (§9: rows learned over the mesh
 *   merge by `node_id`, #1076).
 *
 * A complete snapshot replaces every kind at once, so an entity of any kind
 * missing from it is gone; a change upserts or removes one entity of its kind;
 * `reset` clears all kinds. The store derives no relation (§1: "The client
 * never invents a relation"): an agent keeps the `node_id` and `project` the
 * server sent, and grouping them into trees is the Fleet and Projects views'
 * job (G4).
 *
 * Per kind the store holds the entities and the progress: loading until the
 * first complete snapshot, then current, or failed with the message.
 *
 * Lifetime: as in `store/sessions.ts`, the record lives while it has a reader.
 * One `subscribe` on the source serves every reader, and the last
 * {@link FleetReader.release} closes it. A failed record is over for good: it
 * leaves the store, readers that hold it keep the failure they saw, and the
 * next reader opens a fresh subscription. Persistence is a later slice.
 *
 * @module
 */
import { batch, createSignal, type Accessor } from "solid-js";

import type { AgentRow, FleetChange, FleetSource } from "../fleet";
import type { FleetNodeRow } from "../gen/FleetNodeRow";
import type { FleetProjectRow } from "../gen/FleetProjectRow";

/** Where the stream is: before its first complete snapshot, Current, or ended with a failure. */
export type FleetProgress = { readonly phase: "loading" | "current" } | { readonly phase: "failed"; readonly message: string };

/** One kind's entities, in the order the server introduced them, and the stream's progress. */
export type FleetKind<T> = { readonly items: Accessor<readonly T[]>; readonly progress: Accessor<FleetProgress> };

/** One reader's view of the fleet: the store's record, read-only, and the release. */
export type FleetReader = {
  readonly agents: FleetKind<AgentRow>;
  readonly nodes: FleetKind<FleetNodeRow>;
  readonly projects: FleetKind<FleetProjectRow>;
  /** Gives the fleet up; the last release closes the subscription. Idempotent. */
  readonly release: () => void;
};

/** The store of one source's fleet stream. */
export interface FleetStore {
  read(): FleetReader;
}

type Table<T> = FleetKind<T> & {
  readonly rows: Map<string, T>;
  readonly publish: () => void;
  readonly setProgress: (progress: FleetProgress) => void;
};

function table<T>(): Table<T> {
  const rows = new Map<string, T>();
  const [items, setItems] = createSignal<readonly T[]>([]);
  const [progress, setProgress] = createSignal<FleetProgress>({ phase: "loading" });
  return { rows, items, progress, publish: () => setItems([...rows.values()]), setProgress };
}

/** A project is keyed by its owner and cwd together; the array form cannot collide. */
const projectKey = (nodeId: string, id: string): string => JSON.stringify([nodeId, id]);

type FleetRecord = {
  readonly agents: Table<AgentRow>;
  readonly nodes: Table<FleetNodeRow>;
  readonly projects: Table<FleetProjectRow>;
  /** Readers holding the record. */
  readers: number;
  unsubscribe: (() => void) | null;
};

function createFleetStore(source: FleetSource): FleetStore {
  let current: FleetRecord | null = null;

  const open = (): FleetRecord => {
    const record: FleetRecord = { agents: table(), nodes: table(), projects: table(), readers: 0, unsubscribe: null };
    const tables = [record.agents, record.nodes, record.projects] as const;
    const clear = (): void => {
      for (const each of tables) {
        each.rows.clear();
        each.publish();
      }
    };
    /** Applies one change to its kind's rows and returns that table, unpublished. */
    const mutate = (change: FleetChange): { readonly publish: () => void } => {
      if (change.kind === "agent") {
        if (change.op === "upsert") record.agents.rows.set(change.row.id, change.row);
        else record.agents.rows.delete(change.id);
        return record.agents;
      }
      if (change.kind === "node") {
        if (change.op === "upsert") record.nodes.rows.set(change.row.node_id, change.row);
        else record.nodes.rows.delete(change.node_id);
        return record.nodes;
      }
      if (change.op === "upsert") record.projects.rows.set(projectKey(change.row.node_id, change.row.id), change.row);
      else record.projects.rows.delete(projectKey(change.node_id, change.id));
      return record.projects;
    };
    // Every kind is replaced and published inside one batch: a reader never sees a half-replaced fleet.
    const snapshot = (changes: readonly FleetChange[]): void => {
      batch(() => {
        for (const each of tables) each.rows.clear();
        for (const change of changes) mutate(change);
        for (const each of tables) {
          each.publish();
          each.setProgress({ phase: "current" });
        }
      });
    };
    record.unsubscribe = source.subscribe({
      rows: (rows) => snapshot(rows.map((row) => ({ op: "upsert", kind: "agent", row }))),
      snapshot,
      change: (change) => {
        mutate(change).publish();
        },
      reset: () => {
        batch(() => {
          clear();
          for (const each of tables) each.setProgress({ phase: "loading" });
        });
        },
      failed: (message) => {
        batch(() => {
          clear();
          for (const each of tables) each.setProgress({ phase: "failed", message });
        });
        // The subscription is over for good: the next reader opens a fresh one.
        if (current === record) current = null;
        },
    });
    current = record;
    return record;
  };

  return {
    read: () => {
      const record = current ?? open();
      record.readers++;
      let released = false;
      const reader: FleetReader = {
        agents: record.agents,
        nodes: record.nodes,
        projects: record.projects,
        release: () => {
          if (released) return;
          released = true;
          if (--record.readers > 0) return;
          record.unsubscribe?.();
          // A failed record left already; a newer record is not ours to drop.
          if (current === record) current = null;
        },
      };
      return reader;
    },
  };
}

const stores = new WeakMap<FleetSource, FleetStore>();

/** The store for `source`: every view over one source shares it, so the fleet is subscribed once. */
export function fleetStore(source: FleetSource): FleetStore {
  const found = stores.get(source);
  if (found !== undefined) return found;
  const created = createFleetStore(source);
  stores.set(source, created);
  return created;
}
