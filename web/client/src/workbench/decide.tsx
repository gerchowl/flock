/**
 * Answering one open permission prompt from its inbox card (#1749 PR 2).
 *
 * The buttons send `agent.decide` through the outbox with the card's
 * `request_seq` exactly as the open record carried it. The card's state is
 * read back from the outbox entry for that `request_seq`, so a reload shows
 * it too. An accepted ack never removes the card: it leaves on the server's
 * closed record or when the id is delisted (ADR-0038 §1), and until then it
 * says it was answered. A typed refusal is shown in plain words and never
 * retried; the buttons stay disabled for that `request_seq`, and a new open
 * record, with a new seq, enables them again. One decision is in flight per
 * request. "Always allow" is a standing grant, so it takes one inline confirm,
 * and every decision is bound to the `request_id` and `request_seq` the person
 * was shown: each button carries the values it was rendered with, and the
 * command sends exactly those, never the card's current ones. When a request's
 * seq changes, the card enters "Request changed — review it": every button
 * (and an open confirm) stays disabled until the person acknowledges with
 * "Review new request", and only then do the buttons bind to the new seq.
 * A card whose agent stream has a history gap is stale: its buttons wait for the
 * resync (ADR-0038 §1).
 *
 * Settled outbox entries are not persisted (`wire/outbox.ts`), so after a reload
 * an answered but still-open card has live buttons again. That is harmless: the
 * server refuses a second answer with `permission_request_closed`, shown here.
 *
 * @module
 */
import { For, Show, createEffect, createSignal, createUniqueId, untrack, type JSX, type Signal } from "solid-js";

import type { AgentDecideErrorCode } from "../gen/AgentDecideErrorCode";
import type { PermissionDecision } from "../gen/PermissionDecision";
import type { OutboxEntry } from "../wire/outbox";
import { STATUS_LABELS, type Sender } from "./Composer";
import type { InboxCard } from "./inbox";

/** What the server's typed refusals mean for the person, in plain words. */
export const REFUSALS: { readonly [C in AgentDecideErrorCode]: string } = {
  permission_request_unknown: "The server does not know this request any more. This card follows the agent's list.",
  permission_request_closed: "This request was already closed. This card follows the agent's list.",
  not_authorized: "This client is not allowed to answer permission prompts.",
};

/** Where the answer to one request stands. */
export type DecideState =
  | { readonly kind: "idle" }
  | { readonly kind: "inflight"; readonly line: string }
  | { readonly kind: "answered" }
  | { readonly kind: "refused"; readonly line: string };

/** The latest `agent.decide` for this card's request and `request_seq`, read from the outbox. */
export function decideState(commands: readonly OutboxEntry[], card: InboxCard): DecideState {
  const mine = commands.filter((entry) => {
    const args = entry.args;
    return (
      entry.verb === "agent.decide" &&
      typeof args === "object" &&
      args !== null &&
      !Array.isArray(args) &&
      args["agent"] === card.agent &&
      args["request_id"] === card.id &&
      args["request_seq"] === card.requestSeq
    );
  });
  const entry = mine.at(-1);
  if (entry === undefined) return { kind: "idle" };
  switch (entry.status) {
    case "acked":
      return { kind: "answered" };
    case "failed": {
      const code = entry.error?.code;
      const known = code !== undefined && Object.hasOwn(REFUSALS, code) ? REFUSALS[code as AgentDecideErrorCode] : null;
      return { kind: "refused", line: known ?? `${STATUS_LABELS.failed}: ${entry.error?.message ?? "no reason given"}` };
    }
    default:
      return { kind: "inflight", line: STATUS_LABELS[entry.status] };
  }
}

const CHOICES: readonly { readonly label: string; readonly decision: PermissionDecision }[] = [
  { label: "Allow once", decision: "allow_once" },
  { label: "Always allow", decision: "allow_always" },
  { label: "Deny", decision: "deny" },
];

/**
 * The longest message the server takes, in UTF-8 bytes of the trimmed text. The server
 * bound is #2249 (2048 bytes, `prompt_control_bytes` rules, typed `invalid_params`); this
 * constant becomes generated once #2249 lands.
 */
export const MESSAGE_MAX_BYTES = 2048;

const utf8Bytes = (text: string): number => new TextEncoder().encode(text).length;

/** The request a decision is bound to: the one the person was shown and acknowledged. */
export type Target = { readonly agent: string; readonly id: string; readonly seq: number; readonly title: string };

/** What the buttons carry before any request has been shown: it matches no card, so they send nothing. */
const UNBOUND: Target = { agent: "", id: "", seq: -1, title: "" };

/** A caught error, with the request, attempt and status it was raised for: it shows only while all three still hold. */
type Raised = { readonly text: string; readonly seq: number; readonly attempt: number; readonly under: string | null };

/**
 * What the person has typed and opened on one card. It outlives the card's
 * component: a new record rebuilds the card object, and with it the component,
 * but a draft message, an open confirm and the acknowledged request must
 * survive that.
 */
export type DecideUi = {
  readonly message: Signal<string>;
  readonly confirming: Signal<Target | null>;
  /** The request the buttons are bound to; moves only when the person acknowledges a change. */
  readonly bound: Signal<Target | null>;
  /** How many decisions this card has tried to send. */
  readonly attempts: Signal<number>;
  readonly error: Signal<Raised | null>;
};

/** Creates one card's {@link DecideUi}. */
export function createDecideUi(): DecideUi {
  return {
    message: createSignal(""),
    confirming: createSignal<Target | null>(null),
    bound: createSignal<Target | null>(null),
    attempts: createSignal(0),
    error: createSignal<Raised | null>(null),
  };
}

/** The message box, choices and status line of one live permission card. */
export function PermissionActions(props: { readonly card: InboxCard; readonly sender: Sender | null; readonly ui: DecideUi }): JSX.Element {
  const [message, setMessage] = props.ui.message;
  const [confirming, setConfirming] = props.ui.confirming;
  const [bound, setBound] = props.ui.bound;
  const [attempts, setAttempts] = props.ui.attempts;
  const [error, setError] = props.ui.error;
  const reason = createUniqueId();
  const state = (): DecideState => (props.sender === null ? { kind: "idle" } : decideState(props.sender.commands(), props.card));
  const bytes = (): number => utf8Bytes(message().trim());
  /** The request the card shows now. */
  const current = (): Target | null => (props.card.requestSeq === undefined ? null : { agent: props.card.agent, id: props.card.id, seq: props.card.requestSeq, title: props.card.title });
  // The first request this card shows is the one the person sees first, whenever it arrives (a card
  // can be pending first); later ones need an acknowledgement. Bound before any button can render enabled.
  const bindFirst = (): void =>
    untrack(() => {
      const now = current();
      if (bound() === null && now !== null) setBound(now);
    });
  bindFirst();
  createEffect(() => {
    if (props.card.requestSeq !== undefined) bindFirst();
  });
  /** The card's request_seq moved since the person last acknowledged one. */
  const changed = (): boolean => {
    const at = bound();
    return at !== null && props.card.requestSeq !== undefined && at.seq !== props.card.requestSeq;
  };
  /** Why the buttons are disabled, or null when they can answer. */
  const blocked = (): string | null => {
    if (props.sender === null) return "Answering is not available in this view.";
    const sender = props.sender.blocked();
    if (sender !== null) return sender;
    if (props.card.requestSeq === undefined) return "Answering needs the server's open permission record.";
    if (changed()) return "Request changed — review it.";
    if (props.card.stale === true) return "This agent's history has a gap; answering waits for it to catch up.";
    const now = state();
    if (now.kind === "answered") return "Answered; waiting for the agent";
    if (now.kind === "inflight" || now.kind === "refused") return now.line;
    if (bytes() > MESSAGE_MAX_BYTES) return `The message is ${bytes() - MESSAGE_MAX_BYTES} bytes over the ${MESSAGE_MAX_BYTES}-byte limit.`;
    return null;
  };
  const fail = (text: string, at: Target): void => void setError({ text, seq: at.seq, attempt: attempts(), under: blocked() });
  /** Sends `decision` for exactly the request `at` carries, which is the one the button was rendered with. */
  const send = (decision: PermissionDecision, at: Target): void => {
    if (props.sender === null || blocked() !== null) return;
    if (at.id !== props.card.id || at.seq !== props.card.requestSeq) return;
    const note = message().trim();
    setAttempts(attempts() + 1);
    try {
      props.sender.decide({ agent: at.agent, request_id: at.id, request_seq: at.seq, decision, ...(note === "" ? {} : { message: note }) });
      setError(null);
    } catch (thrown) {
      fail((thrown as Error).message, at);
    }
    setConfirming(null);
  };
  // A changed request cancels an open confirm; the card says why.
  createEffect(() => {
    if (changed() && confirming() !== null) setConfirming(null);
  });
  const review = (): void => {
    setBound(current());
    setConfirming(null);
    setError(null);
  };
  const shownError = (): string | null => {
    const raised = error();
    return raised !== null && raised.seq === bound()?.seq && raised.attempt === attempts() && raised.under === blocked() ? raised.text : null;
  };
  return (
    <div class="decide" data-testid="decide">
      <input
        class="decide__message"
        type="text"
        aria-label="Message to the agent (optional)"
        placeholder="Message to the agent (optional)"
        aria-describedby={reason}
        value={message()}
        disabled={props.sender === null || props.sender.blocked() !== null || props.card.requestSeq === undefined}
        onInput={(event) => setMessage(event.currentTarget.value)}
      />
      <Show when={message().trim() !== ""}>
        <p class="decide__count" data-testid="decide-count">
          {bytes() > MESSAGE_MAX_BYTES ? `${bytes() - MESSAGE_MAX_BYTES} bytes over the limit` : `${MESSAGE_MAX_BYTES - bytes()} bytes left`}
        </p>
      </Show>
      <Show when={changed()}>
        <button type="button" class="action action--primary" onClick={review}>
          Review new request
        </button>
      </Show>
      <Show when={bound() ?? UNBOUND} keyed>
        {(at) => (
          <Show
            when={confirming()}
            fallback={
              <div class="card__choices">
                <For each={CHOICES}>
                  {(choice) => (
                    <button
                      type="button"
                      class="action"
                      disabled={blocked() !== null}
                      aria-describedby={reason}
                      onClick={() => (choice.decision === "allow_always" ? setConfirming(at) : send(choice.decision, at))}
                    >
                      {choice.label}
                    </button>
                  )}
                </For>
              </div>
            }
          >
            {(asked) => (
              <div class="decide__confirm" role="group" aria-label="Confirm always allow">
                <p>Always allow {asked().title} for the rest of this agent's session? This is a standing grant.</p>
                <div class="card__choices">
                  <button type="button" class="action action--primary" disabled={blocked() !== null} aria-describedby={reason} onClick={() => send("allow_always", asked())}>
                    Confirm always allow
                  </button>
                  <button type="button" class="action" onClick={() => setConfirming(null)}>
                    Cancel
                  </button>
                </div>
              </div>
            )}
          </Show>
        )}
      </Show>
      <p id={reason} class="waits__reason" data-testid="decide-status">
        {shownError() ?? blocked() ?? "Answering sends your decision to the agent."}
      </p>
    </div>
  );
}
