/**
 * The composer (ADR-0038 §2a): it says what will happen to a message, sends
 * it as `cmd agent.send` through the outbox (§3, G3a #1747), and shows each
 * message's command state.
 *
 * The delivery line presents the server's own state, mapped from every
 * reported phase in {@link delivery}. The `ack` is the authority on what
 * happened. Optimism is visual only: a message never appears in the
 * transcript until the server's event does, and once that event has
 * arrived the accepted message leaves "Your messages", so it shows once.
 * A message's event is a person's message with its text and a seq after
 * the transcript's cursor when it was sent; each event answers one message.
 *
 * Drafts are kept per agent while the workbench is open, so switching
 * agents never loses one.
 *
 * `agent.send`'s args are the socket API's `AgentSendParams` (`target`,
 * `text`, `submit`), opaque on the wire until S8b #1677 types them.
 *
 * @module
 */
import { For, createSignal, type JSX } from "solid-js";

import type { OutboxStatus } from "../gen/OutboxStatus";
import type { Phase } from "../gen/Phase";
import type { AgentRow } from "../fleet";
import type { OutboxEntry } from "../wire/outbox";

/** Sending, as the workbench sees it. */
export type Sender = {
  /** The outbox, oldest first; reactive. */
  readonly commands: () => readonly OutboxEntry[];
  /** Queues `agent.send`; throws when no principal is known yet. */
  readonly send: (agent: string, text: string) => string;
  /** Why sending is impossible right now, or null. */
  readonly blocked: () => string | null;
};

const STATUS_LABELS: { readonly [S in OutboxStatus]: string } = {
  pending: "queued here",
  sent: "sent, waiting for the server",
  uncertain: "no answer yet: delivery unknown",
  unknown_outcome: "unknown outcome",
  acked: "accepted",
  failed: "failed",
};

/** The `target` and `text` of an `agent.send` command, or null for other verbs. */
function sentText(entry: OutboxEntry, agent: string): string | null {
  const args = entry.args;
  if (entry.verb !== "agent.send" || typeof args !== "object" || args === null || Array.isArray(args)) return null;
  return args["target"] === agent && typeof args["text"] === "string" ? args["text"] : null;
}

/** What sending does for one reported phase: the line to show, and whether sending is open. */
export type Delivery = { readonly line: string; readonly open: boolean };

/**
 * The delivery line for every {@link Phase}; a new phase fails `tsc` here.
 *
 * - A turn in progress holds the message until the turn ends: an
 *   `at_idle` item is written after the current `result` (ADR-0036 §3).
 * - The idle phases take it at once ("an idle agent with an empty queue
 *   takes an `at_idle` item at once", ADR-0036 §3): `finished`, and
 *   `waiting_on_event` and `finished_with_background`, whose turn is idle
 *   (ADR-0028's phase table).
 * - Waiting on a decision, or away, it stays queued, and nothing is
 *   promised about when.
 * - A `gone` agent has ended: sending is closed.
 */
export function deliveryFor(state: Phase): Delivery {
  switch (state) {
    case "working":
    case "settling":
      return { line: "Queued until the turn ends.", open: true };
    case "finished":
    case "waiting_on_event":
    case "finished_with_background":
      return { line: "Delivered now.", open: true };
    case "blocked":
    case "needs_human":
      return { line: "Queued; the agent is waiting on a decision.", open: true };
    case "offline":
    case "hibernated":
    case "limit_reached":
    case "unknown":
      return { line: "Queued. The agent isn't reachable right now.", open: true };
    case "gone":
      return { line: "This agent has ended.", open: false };
  }
}

/** The delivery line for `row`, which may not be listed. */
export function delivery(row: AgentRow | undefined): Delivery {
  return row === undefined ? { line: "Goes to the agent's queue.", open: true } : deliveryFor(row.state);
}

/** The send shortcut as this platform writes it. */
export function sendHint(platform: string): string {
  return `${/Mac|iPhone|iPad|iPod/.test(platform) ? "⌘" : "Ctrl"} + Enter to send`;
}

/** What one agent's transcript has shown: its highest seq, and its people's messages by seq. */
type Seen = { readonly cursor: number; readonly texts: ReadonlyMap<number, string> };

/**
 * Composer state that outlives one agent's composer: drafts by agent, what
 * each agent's transcript has shown, and the transcript cursor each sent
 * message was sent at.
 */
export type ComposerState = {
  readonly drafts: Map<string, string>;
  /** The transcript applied `seq`, a person's message when `text` is not null. */
  readonly observe: (agent: string, seq: number, text: string | null) => void;
  /** `cmdId` was just sent to `agent`. */
  readonly sent: (agent: string, cmdId: string) => void;
  /** Which accepted commands' events the transcript has shown (see the module doc). */
  readonly echoed: (agent: string, accepted: readonly { readonly cmdId: string; readonly text: string }[]) => ReadonlySet<string>;
};

/** Creates the {@link ComposerState} of one workbench. */
export function createComposerState(): ComposerState {
  const [seen, setSeen] = createSignal<ReadonlyMap<string, Seen>>(new Map());
  const sentAt = new Map<string, number>();
  return {
    drafts: new Map(),
    observe: (agent, seq, text) => {
      const before = seen().get(agent) ?? { cursor: 0, texts: new Map() };
      if (seq <= before.cursor && (text === null || before.texts.get(seq) === text)) return;
      const texts = text === null ? before.texts : new Map(before.texts).set(seq, text);
      setSeen(new Map(seen()).set(agent, { cursor: Math.max(before.cursor, seq), texts }));
    },
    sent: (agent, cmdId) => void sentAt.set(cmdId, seen().get(agent)?.cursor ?? 0),
    echoed: (agent, accepted) => {
      const texts = [...(seen().get(agent)?.texts ?? new Map<number, string>())].sort(([a], [b]) => a - b);
      const used = new Set<number>();
      const matched = new Set<string>();
      for (const { cmdId, text } of accepted) {
        const after = sentAt.get(cmdId);
        // A command restored from storage has no send cursor here: it stays listed.
        if (after === undefined) continue;
        const echo = texts.find(([seq, shown]) => seq > after && shown === text && !used.has(seq));
        if (echo === undefined) continue;
        used.add(echo[0]);
        matched.add(cmdId);
      }
      return matched;
    },
  };
}

/** The composer for one agent. */
export function Composer(props: {
  readonly agent: string;
  readonly row: AgentRow | undefined;
  readonly sender: Sender | null;
  readonly state: ComposerState;
}): JSX.Element {
  const [text, setDraft] = createSignal(props.state.drafts.get(props.agent) ?? "");
  const setText = (value: string): void => {
    setDraft(value);
    if (value === "") props.state.drafts.delete(props.agent);
    else props.state.drafts.set(props.agent, value);
  };
  const [error, setError] = createSignal<string | null>(null);
  const blocked = (): string | null => {
    if (props.sender === null) return "Replay: sending needs a live server.";
    const mode = delivery(props.row);
    return mode.open ? props.sender.blocked() : mode.line;
  };
  /** Accepted messages whose event the transcript has shown are left out (see the module doc). */
  const mine = (): { readonly id: string; readonly text: string; readonly status: OutboxStatus; readonly error?: string }[] => {
    const own = (props.sender?.commands() ?? []).flatMap((entry) => {
      const sent = sentText(entry, props.agent);
      return sent === null ? [] : [{ entry, sent }];
    });
    const accepted = own.filter(({ entry }) => entry.status === "acked").map(({ entry, sent }) => ({ cmdId: entry.cmd_id, text: sent }));
    const echoed = props.state.echoed(props.agent, accepted);
    return own
      .filter(({ entry }) => !echoed.has(entry.cmd_id))
      .map(({ entry, sent }) => ({ id: entry.cmd_id, text: sent, status: entry.status, ...(entry.error ? { error: entry.error.message } : {}) }));
  };
  const submit = (): void => {
    const message = text().trim();
    if (message === "" || props.sender === null || blocked() !== null) return;
    try {
      props.state.sent(props.agent, props.sender.send(props.agent, message));
      setText("");
      setError(null);
    } catch (thrown) {
      setError((thrown as Error).message);
    }
  };
  return (
    <form
      class="composer"
      data-testid="composer"
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
    >
      <ul class="composer__sent" aria-label="Your messages">
        <For each={mine()}>
          {(item) => (
            <li class={`sent sent--${item.status}`} data-testid="sent-message" data-status={item.status}>
              <span class="sent__text">{item.text}</span>
              <span class="sent__status">{item.error === undefined ? STATUS_LABELS[item.status] : `${STATUS_LABELS[item.status]}: ${item.error}`}</span>
            </li>
          )}
        </For>
      </ul>
      <textarea
        class="composer__input"
        aria-label={`Message ${props.row?.label ?? props.agent}`}
        rows="2"
        value={text()}
        disabled={blocked() !== null}
        onInput={(event) => setText(event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
            event.preventDefault();
            submit();
          }
        }}
      />
      <div class="composer__footer">
        <span class="composer__mode" data-testid="delivery-mode">
          {blocked() ?? error() ?? delivery(props.row).line}
        </span>
        <span class="composer__hint" data-testid="send-hint">
          {sendHint(navigator.platform || navigator.userAgent)}
        </span>
        <button type="submit" class="action action--primary" disabled={blocked() !== null}>
          Send
        </button>
      </div>
    </form>
  );
}
