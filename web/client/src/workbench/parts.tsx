/**
 * Workbench parts shared by the desktop and phone layouts (ADR-0038 §2a).
 *
 * @module
 */
import { For, Show, createUniqueId, type JSX } from "solid-js";

import type { AgentRow } from "../fleet";
import { sortInbox, type InboxCard } from "./inbox";
import { PHASE_LABELS, StatusMark, markFor } from "./marks";

/**
 * A post-MVP action, shown but inert. What it waits for is visible text,
 * which the control names through `aria-describedby`, so phones and
 * keyboards reach it too. `reasonId` points at a reason shared with other
 * actions instead of a caption of its own.
 */
export function Waits(props: { readonly label: string } & ({ readonly waitsFor: string } | { readonly reasonId: string })): JSX.Element {
  const id = "reasonId" in props ? props.reasonId : createUniqueId();
  return (
    <span class="waits">
      <button type="button" class="action" aria-disabled="true" aria-describedby={id} onClick={(event) => event.preventDefault()}>
        {props.label}
      </button>
      {"waitsFor" in props && (
        <span id={id} class="waits__reason">
          Waits for {props.waitsFor}
        </span>
      )}
    </span>
  );
}

/** One sidebar row: status mark, name, activity, and the open-item count. */
export function AgentRowButton(props: { readonly row: AgentRow; readonly focused: boolean; readonly onFocus: () => void }): JSX.Element {
  return (
    <button type="button" class="agent-row" aria-current={props.focused ? "true" : undefined} onClick={props.onFocus} data-testid="agent-row">
      <StatusMark mark={markFor(props.row)} label={PHASE_LABELS[props.row.state]} count={props.row.waitingOnHuman.length} />
      <span class="agent-row__text">
        <span class="agent-row__name">{props.row.label}</span>
        <span class="agent-row__activity">{props.row.activity}</span>
      </span>
    </button>
  );
}

/** The inbox's cards, blocking first. */
export function InboxCards(props: { readonly cards: readonly InboxCard[]; readonly source: "replay" | "live" }): JSX.Element {
  const note = (): string | null =>
    props.source === "live"
      ? props.cards.length === 0
        ? "No open permission prompts."
        : null
      : props.cards.length === 0
        ? "The live inbox arrives with G5 (#1597)."
        : "Sample cards: the live inbox arrives with G5 (#1597).";
  return (
    <>
      <Show when={note()}>{(text) => <p class="inbox__note">{text()}</p>}</Show>
      <For each={sortInbox(props.cards)}>
        {(card) => {
          const reason = createUniqueId();
          return (
          <article class="card" classList={{ "card--blocking": card.blocking }} data-testid="inbox-card" data-request-seq={card.requestSeq}>
            <p class="card__meta">
              {card.kind === "permission" ? "Permission" : "Question"} · {card.agentLabel}
            </p>
            <h3 class="card__title">{card.title}</h3>
            <p class="card__detail">{card.detail}</p>
            <div class="card__choices">
              <For each={card.choices}>{(choice) => <Waits label={choice} reasonId={reason} />}</For>
              <Waits label="Edit…" reasonId={reason} />
            </div>
            <p id={reason} class="waits__reason">
              Answering waits for {card.waitsFor}.
            </p>
          </article>
          );
        }}
      </For>
    </>
  );
}

/** The focused agent's name and state, as both headers show them. */
export function AgentTitle(props: { readonly agent: string; readonly row: AgentRow | undefined }): JSX.Element {
  return (
    <>
      <h1 class="focus__name">{props.row?.label ?? props.agent}</h1>
      <Show when={props.row}>
        {(row) => (
          <span class="chip">
            <StatusMark mark={markFor(row())} label={PHASE_LABELS[row().state]} />
            {PHASE_LABELS[row().state]}
          </span>
        )}
      </Show>
    </>
  );
}
