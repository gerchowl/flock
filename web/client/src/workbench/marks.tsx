/**
 * Status marks (ADR-0038 §2a): each state differs in shape and lightness,
 * never in hue alone, and always carries a text label for screen readers.
 *
 * The mark is a presentation of the server's state (ADR-0038 §3): the
 * client maps the reported S4 phase to a mark and derives nothing else.
 *
 * @module
 */
import { Show, type JSX } from "solid-js";

import type { Phase } from "../gen/Phase";
import type { AgentRow } from "../fleet";

/** The five marks of §2a's table. */
export type Mark = "working" | "needs" | "idle" | "done" | "failed";

const PHASE_MARKS: { readonly [P in Phase]: Mark } = {
  working: "working",
  settling: "working",
  needs_human: "needs",
  blocked: "failed",
  limit_reached: "failed",
  finished: "idle",
  finished_with_background: "idle",
  waiting_on_event: "idle",
  hibernated: "idle",
  offline: "idle",
  gone: "idle",
  unknown: "idle",
};

/** Human labels of the phases, as the sidebar and headers show them. */
export const PHASE_LABELS: { readonly [P in Phase]: string } = {
  working: "working",
  settling: "settling",
  needs_human: "needs you",
  blocked: "blocked",
  limit_reached: "limit reached",
  finished: "idle",
  finished_with_background: "idle, background running",
  waiting_on_event: "waiting on an event",
  hibernated: "hibernated",
  offline: "offline",
  gone: "gone",
  unknown: "state unknown",
};

/** The mark for a row: an open item waiting on a human always shows as "needs you". */
export function markFor(row: AgentRow): Mark {
  return row.waitingOnHuman.length > 0 ? "needs" : PHASE_MARKS[row.state];
}

/** Factories, not elements: one DOM node cannot render in two places. */
const SHAPES: { readonly [M in Mark]: () => JSX.Element } = {
  working: () => <circle cx="8" cy="8" r="6" />,
  needs: () => <circle cx="8" cy="8" r="6" />,
  idle: () => <rect x="3" y="3" width="10" height="10" rx="1" />,
  done: () => <path d="M3 8.5l3.2 3.2L13 4.8" fill="none" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" />,
  failed: () => <circle cx="8" cy="8" r="5.5" fill="none" stroke-width="2" />,
};

/** One status mark, with an optional count badge (the "needs you" count). */
export function StatusMark(props: { readonly mark: Mark; readonly label: string; readonly count?: number }): JSX.Element {
  return (
    <span class={`mark mark--${props.mark}`} role="img" aria-label={props.label} data-mark={props.mark}>
      <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
        {SHAPES[props.mark]()}
      </svg>
      <Show when={props.count !== undefined && props.count > 0}>
        <span class="mark__count">{props.count}</span>
      </Show>
    </span>
  );
}
