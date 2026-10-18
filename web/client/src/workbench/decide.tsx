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
 * bound to the `request_id` and `request_seq` it was opened for: if the card's
 * seq changes while it is open, it is cancelled with a reason, and a decision
 * always carries the values captured at the click, never the card's current ones.
 * A card whose agent stream has a history gap is stale: its buttons wait for the
 * resync (ADR-0038 §1).
 *
 * Settled outbox entries are not persisted (`wire/outbox.ts`), so after a reload
 * an answered but still-open card has live buttons again. That is harmless: the
 * server refuses a second answer with `permission_request_closed`, shown here.
 *
 * @module
 */
import { For, Show, createEffect, createMemo, createSignal, createUniqueId, on, type JSX, type Signal } from "solid-js";

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

/** The request a decision is bound to: captured when the person clicked, never read live. */
export type Target = { readonly agent: string; readonly id: string; readonly seq: number; readonly title: string };

/**
 * What the person has typed and opened on one card. It outlives the card's
 * component: a new record rebuilds the card object, and with it the component,
 * but a draft message and an open confirm must survive that, or be cancelled
 * with a reason, never lost silently.
 */
export type DecideUi = {
  readonly message: Signal<string>;
  readonly confirming: Signal<Target | null>;
  readonly error: Signal<string | null>;
};

/** Creates one card's {@link DecideUi}. */
export function createDecideUi(): DecideUi {
  return { message: createSignal(""), confirming: createSignal<Target | null>(null), error: createSignal<string | null>(null) };
}

/** The message box, choices and status line of one live permission card. */
export function PermissionActions(props: { readonly card: InboxCard; readonly sender: Sender | null; readonly ui: DecideUi }): JSX.Element {
  const [message, setMessage] = props.ui.message;
  const [confirming, setConfirming] = props.ui.confirming;
  const [error, setError] = props.ui.error;
  const reason = createUniqueId();
  const state = (): DecideState => (props.sender === null ? { kind: "idle" } : decideState(props.sender.commands(), props.card));
  const bytes = (): number => utf8Bytes(message().trim());
  /** Why the buttons are disabled, or null when they can answer. */
  const blocked = (): string | null => {
    if (props.sender === null) return "Answering is not available in this view.";
    const sender = props.sender.blocked();
    if (sender !== null) return sender;
    if (props.card.requestSeq === undefined) return "Answering needs the server's open permission record.";
    if (props.card.stale === true) return "This agent's history has a gap; answering waits for it to catch up.";
    const current = state();
    if (current.kind === "answered") return "Answered; waiting for the agent";
    if (current.kind === "inflight" || current.kind === "refused") return current.line;
    if (bytes() > MESSAGE_MAX_BYTES) return `The message is ${bytes() - MESSAGE_MAX_BYTES} bytes over the ${MESSAGE_MAX_BYTES}-byte limit.`;
    return null;
  };
  /** The request the card shows now, for a click to capture. */
  const target = (): Target | null => (props.card.requestSeq === undefined ? null : { agent: props.card.agent, id: props.card.id, seq: props.card.requestSeq, title: props.card.title });
  const send = (decision: PermissionDecision, at: Target | null): void => {
    if (props.sender === null || at === null || blocked() !== null) return;
    if (at.id !== props.card.id || at.seq !== props.card.requestSeq) {
      setConfirming(null);
      setError("The request changed; review it again.");
      return;
    }
    const note = message().trim();
    try {
      props.sender.decide({ agent: at.agent, request_id: at.id, request_seq: at.seq, decision, ...(note === "" ? {} : { message: note }) });
      setError(null);
    } catch (thrown) {
      setError((thrown as Error).message);
    }
    setConfirming(null);
  };
  // The confirm belongs to one request_seq: a new open record cancels it, saying why.
  const changed = (): boolean => {
    const at = confirming();
    return at !== null && (at.id !== props.card.id || at.seq !== props.card.requestSeq);
  };
  createEffect(() => {
    if (!changed()) return;
    setConfirming(null);
    setError("The request changed; review it again.");
  });
  // A send error covers the status only until the status moves on.
  const stateKey = createMemo(() => blocked());
  createEffect(on(stateKey, () => setError(null), { defer: true }));
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
                  onClick={() => (choice.decision === "allow_always" ? setConfirming(target()) : send(choice.decision, target()))}
                >
                  {choice.label}
                </button>
              )}
            </For>
          </div>
        }
      >
        {(at) => (
          <div class="decide__confirm" role="group" aria-label="Confirm always allow">
            <p>Always allow {at().title} for the rest of this agent's session? This is a standing grant.</p>
            <div class="card__choices">
              <button type="button" class="action action--primary" onClick={() => send("allow_always", at())}>
                Confirm always allow
              </button>
              <button type="button" class="action" onClick={() => setConfirming(null)}>
                Cancel
              </button>
            </div>
          </div>
        )}
      </Show>
      <p id={reason} class="waits__reason" data-testid="decide-status">
        {error() ?? blocked() ?? "Answering sends your decision to the agent."}
      </p>
    </div>
  );
}
