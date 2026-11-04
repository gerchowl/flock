/**
 * One plain-language map for what the server refuses and how the connection
 * stands (#2902, ADR-0048 D4's copy rule): every line leads with the effect
 * on the person, then says what to do next. The composer, the transcript, the
 * inbox cards, the toolbar's popover and the connection banner all word
 * refusals and states from here, so one code never reads two ways.
 *
 * Primary copy carries no protocol strings: the server's code and its own
 * diagnosis sit behind a *Details* disclosure ({@link RefusalDetails}).
 *
 * The table is typed exhaustively over the generated refusal vocabularies
 * (`AgentDecideErrorCode`, `SubfailCode`), so a code added there fails `tsc`
 * here until it has copy. An ack's `WireError.code` is an open string on the
 * wire; {@link CommandCode} lists the codes the `agent.send` and
 * `agent.decide` paths return today (`src/web/wire/commands.rs`,
 * `src/app/api/structured_queue.rs`, `src/structured/`). A code outside the
 * table falls back to copy chosen by the error's retry class, never to the
 * raw message.
 *
 * @module
 */
import { Show, type JSX } from "solid-js";

import type { AgentDecideErrorCode } from "../gen/AgentDecideErrorCode";
import type { SubfailCode } from "../gen/SubfailCode";
import type { WireRetry } from "../gen/WireRetry";
import { Stack } from "../layout/primitives";
import { SPACE } from "../tokens";
import type { ConnectionState } from "../wire/connection";
import "./refusals.css";

/**
 * The longest message the server takes, in UTF-8 bytes of the trimmed text. The server
 * bound is #2249 (`prompt_control_bytes` rules, typed `invalid_params`). The generated
 * TypeScript schema describes the bound but does not export a numeric constant.
 */
export const MESSAGE_MAX_BYTES = 2048;

/** Command refusals the send and decide paths return besides the decide vocabulary; the wire types none of them. */
export type CommandCode =
  | "agent_not_found"
  | "unsupported_for_agent"
  | "structured_submit_required"
  | "self_message_forbidden"
  | "invalid_request"
  | "auth_revoked"
  | "session_unconfirmed"
  | "internal_error"
  | "server_version_gap"
  | "serialization_error"
  | "input_write_unknown"
  | "input_write_failed"
  | "agent_start_failed"
  | "queue_item_too_large"
  | "queue_item_conflict"
  | "queue_mode_unsupported"
  | "queue_journal_corrupt"
  | "queue_journal_newer"
  | "queue_kind_reserved"
  | "mail_store_unavailable"
  | "no_result"
  | "permission_grant_limit"
  | "permission_input_already_held"
  | "permission_request_limit"
  | "permission_request_too_large";

/** Every code with its own copy. */
export type RefusalCode = AgentDecideErrorCode | SubfailCode | CommandCode;

/** One refusal in plain words: what happened to the person, then their next step. */
export type Copy = { readonly effect: string; readonly next: string };

const PICK = "Pick an agent from the list.";
const AGAIN = "Send it again.";
const CHECK = "Check the transcript before sending it again.";
const LOG = "Try again; if it keeps happening, check the server's log.";
const TOKEN = "Connect with a token that allows it.";
const LISTED = "Nothing to answer: the card follows the agent's list.";

/** The copy for every typed refusal. */
export const REFUSALS: { readonly [C in RefusalCode]: Copy } = {
  permission_request_unknown: { effect: "The server no longer has this request.", next: LISTED },
  permission_request_closed: { effect: "This request was already answered or withdrawn.", next: LISTED },
  not_authorized: { effect: "This client is not allowed to do that.", next: TOKEN },
  invalid_params: {
    effect: "The server could not accept what was sent.",
    next: `Keep the message to ${MESSAGE_MAX_BYTES} bytes, with no control characters other than newlines and tabs.`,
  },
  invalid_agent: { effect: "The server does not know this agent.", next: PICK },
  cursor_ahead: { effect: "The history could not resume where it stopped.", next: "Retry to load it from the start." },
  operator_only: { effect: "Only the operator can do that.", next: "Connect with the operator's token." },
  unknown_stream: { effect: "This server cannot show this view.", next: "Update the server, then retry." },
  scope: { effect: "This client's token does not cover that.", next: TOKEN },
  session_busy: { effect: "The server is busy.", next: "Try again in a moment." },
  sub_id_reused: { effect: "The server would not reopen this view on this connection.", next: "Reconnect, then retry." },
  agent_not_found: { effect: "The server no longer knows this agent.", next: PICK },
  unsupported_for_agent: { effect: "This agent does not take messages from here.", next: PICK },
  structured_submit_required: { effect: "Nothing was sent.", next: AGAIN },
  self_message_forbidden: { effect: "An agent cannot message itself.", next: "Choose another agent." },
  invalid_request: { effect: "The server could not accept this request.", next: "Check the message, then send it again." },
  auth_revoked: { effect: "This client's token was revoked.", next: "Connect with a new token." },
  session_unconfirmed: { effect: "The server could not confirm whether this went through.", next: CHECK },
  internal_error: { effect: "The server hit an internal error.", next: LOG },
  server_version_gap: { effect: "This server runs an older version that cannot do this.", next: "Update the server." },
  serialization_error: { effect: "The server could not write its reply.", next: LOG },
  input_write_unknown: { effect: "It is unknown whether the agent received this.", next: CHECK },
  input_write_failed: { effect: "The agent did not receive this.", next: AGAIN },
  agent_start_failed: { effect: "The agent could not be started.", next: "Check the agent's state, then try again." },
  queue_item_too_large: { effect: "The message is too long for the agent's queue.", next: "Shorten it, then send it again." },
  queue_item_conflict: { effect: "A different message with the same id is already queued.", next: AGAIN },
  queue_mode_unsupported: { effect: "This agent does not take messages this way.", next: PICK },
  queue_journal_corrupt: { effect: "The agent's message queue on the server is damaged.", next: "Check the server's log; sending waits until it is repaired." },
  queue_journal_newer: { effect: "A newer server wrote this agent's message queue.", next: "Update the server." },
  queue_kind_reserved: { effect: "The server reserves this kind of message.", next: "Send a plain message instead." },
  mail_store_unavailable: { effect: "The server's message store is unavailable.", next: "Try again later." },
  no_result: { effect: "The agent finished without a reply.", next: "Check the transcript." },
  permission_grant_limit: { effect: "This agent holds too many standing grants.", next: "Answer with Allow once or Deny instead." },
  permission_input_already_held: { effect: "Another answer to this request is being handled.", next: "Wait for it to finish." },
  permission_request_limit: { effect: "This agent has too many open requests.", next: "Answer the open ones first." },
  permission_request_too_large: { effect: "This request is too large to answer with a message.", next: "Answer it without a message." },
};

/** The next step for a code without its own copy, by the error's retry class (ADR-0029). */
const BY_RETRY: { readonly [R in WireRetry]: string } = {
  "n/a": "Check the details.",
  never: "Trying again will not help; check the details.",
  maybe: "Check the transcript, then try again.",
  yes: "Try again.",
  later: "Try again later.",
  after_inspecting: "Check the details before trying again.",
  after_human: "This needs someone on the server's side.",
  after_handoff: "Try again once the handoff has finished.",
};

/** A refusal as the person reads it: the plain line, and the raw code and message for Details. */
export type Explained = { readonly line: string; readonly code: string; readonly message: string };

/** Words one refusal: its own copy when the code has some, else the retry class's next step. */
export function explain(error: { readonly code: string; readonly message: string; readonly retry?: WireRetry }): Explained {
  const known = Object.hasOwn(REFUSALS, error.code) ? REFUSALS[error.code as RefusalCode] : null;
  const copy = known ?? { effect: "The server refused this.", next: BY_RETRY[error.retry ?? "n/a"] };
  return { line: `${copy.effect} ${copy.next}`, code: error.code, message: error.message };
}

/** Every connection state in plain words; never the state's own name. */
export const CONNECTION_COPY: { readonly [S in ConnectionState]: string } = {
  Disconnected: "Not connected.",
  Connecting: "Connecting…",
  Hello: "Connecting…",
  Syncing: "Catching up with the server…",
  Live: "Connected.",
  ReadOnly: "Connected, read-only.",
  Stale: "The server stopped answering. Reconnecting…",
  Reconnecting: "Connection lost. Reconnecting…",
  NeedsToken: "Sign-in needed.",
};

/**
 * The raw code and diagnosis behind a closed *Details* disclosure. `code` is
 * left out when there is none (a connection diagnosis has only a message).
 * `open` starts it expanded, for the gallery.
 */
export function RefusalDetails(props: { readonly code?: string | undefined; readonly message: string; readonly open?: true }): JSX.Element {
  return (
    <details class="details" data-testid="details" open={props.open === true}>
      <summary class="details__toggle">Details</summary>
      <p class="details__body">
        <Show when={props.code}>{(code) => <code class="details__code">{code()}</code>}</Show> {props.message}
      </p>
    </details>
  );
}

/**
 * An empty state for something that could not load: what failed, the plain
 * line, *Retry*, and the raw diagnosis behind Details.
 */
export function ErrorState(props: { readonly title: string; readonly line: string | null; readonly code?: string | undefined; readonly message: string; readonly retry: () => void }): JSX.Element {
  return (
    <Stack as="section" space={SPACE[2]} class="empty-state" role="alert" aria-label={props.title} data-testid="error-state">
      <strong class="empty-state__title">{props.title}</strong>
      <Show when={props.line}>{(line) => <span>{line()}</span>}</Show>
      <span>
        <button type="button" class="empty-state__retry" data-testid="retry" onClick={() => props.retry()}>
          Retry
        </button>
      </span>
      <RefusalDetails code={props.code} message={props.message} />
    </Stack>
  );
}
