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
 * request. "Always allow" is a standing grant, so it takes one inline confirm.
 *
 * @module
 */
import { For, Show, createSignal, createUniqueId, type JSX } from "solid-js";

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

/** The message box, choices and status line of one live permission card. */
export function PermissionActions(props: { readonly card: InboxCard; readonly sender: Sender | null }): JSX.Element {
  const [message, setMessage] = createSignal("");
  const [confirming, setConfirming] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const reason = createUniqueId();
  const state = (): DecideState => (props.sender === null ? { kind: "idle" } : decideState(props.sender.commands(), props.card));
  /** Why the buttons are disabled, or null when they can answer. */
  const blocked = (): string | null => {
    if (props.sender === null) return "Answering is not available in this view.";
    const sender = props.sender.blocked();
    if (sender !== null) return sender;
    if (props.card.requestSeq === undefined) return "Answering needs the server's open permission record.";
    const current = state();
    if (current.kind === "answered") return "Answered; waiting for the agent";
    if (current.kind === "inflight" || current.kind === "refused") return current.line;
    return null;
  };
  const send = (decision: PermissionDecision): void => {
    const seq = props.card.requestSeq;
    if (props.sender === null || seq === undefined || blocked() !== null) return;
    const note = message().trim();
    try {
      props.sender.decide({ agent: props.card.agent, request_id: props.card.id, request_seq: seq, decision, ...(note === "" ? {} : { message: note }) });
      setError(null);
    } catch (thrown) {
      setError((thrown as Error).message);
    }
    setConfirming(false);
  };
  return (
    <div class="decide" data-testid="decide">
      <input
        class="decide__message"
        type="text"
        aria-label="Message to the agent (optional)"
        placeholder="Message to the agent (optional)"
        value={message()}
        disabled={blocked() !== null}
        onInput={(event) => setMessage(event.currentTarget.value)}
      />
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
                  onClick={() => (choice.decision === "allow_always" ? setConfirming(true) : send(choice.decision))}
                >
                  {choice.label}
                </button>
              )}
            </For>
          </div>
        }
      >
        <div class="decide__confirm" role="group" aria-label="Confirm always allow">
          <p>Always allow {props.card.title} for the rest of this agent's session? This is a standing grant.</p>
          <div class="card__choices">
            <button type="button" class="action action--primary" onClick={() => send("allow_always")}>
              Confirm always allow
            </button>
            <button type="button" class="action" onClick={() => setConfirming(false)}>
              Cancel
            </button>
          </div>
        </div>
      </Show>
      <p id={reason} class="waits__reason" data-testid="decide-status">
        {error() ?? blocked() ?? "Answering sends your decision to the agent."}
      </p>
    </div>
  );
}
