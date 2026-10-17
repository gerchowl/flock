/**
 * The composer (ADR-0038 §2a): it states the agent's reported phase, sends
 * a message as `cmd agent.send` through the outbox (§3, G3a #1747), and
 * shows each message's outbox state.
 *
 * The client never derives queue order or delivery (§3), so nothing here
 * predicts what will happen: the phase line is the server's last report,
 * worded as a fact ({@link phaseLine}), and each message's line says what
 * has happened to it so far ({@link STATUS_LABELS}). The `ack` is the
 * authority on what happened. Optimism is visual only: a message never
 * appears in the transcript until the server's event does, and once a
 * person's turn has arrived after an accepted message, it leaves "Your
 * messages", so it shows once.
 *
 * Folding needs the transcript's cursor at the send, recorded only while
 * the transcript is Current; a message sent earlier is not foldable and
 * stays listed. Each person's turn after the cursor folds one message: one
 * with the same text first, then any, since the echo's text may differ
 * (whitespace, for example).
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

/** What has happened to a message, for every outbox state; never a prediction. */
export const STATUS_LABELS: { readonly [S in OutboxStatus]: string } = {
  pending: "not sent yet",
  sent: "sent; no answer from the server yet",
  uncertain: "no answer from the server in 30 s",
  unknown_outcome: "outcome unknown",
  acked: "accepted by the server",
  failed: "refused by the server",
};

/** The `target` and `text` of an `agent.send` command, or null for other verbs. */
function sentText(entry: OutboxEntry, agent: string): string | null {
  const args = entry.args;
  if (entry.verb !== "agent.send" || typeof args !== "object" || args === null || Array.isArray(args)) return null;
  return args["target"] === agent && typeof args["text"] === "string" ? args["text"] : null;
}

/** The agent's reported phase as the composer states it, and whether sending is open. */
export type PhaseLine = { readonly line: string; readonly open: boolean };

/**
 * The line for every {@link Phase} (ADR-0028's phase table), stated as the
 * server's report and never as what will happen to a message; a new phase
 * fails `tsc` here. A `gone` agent has ended: sending is closed.
 */
export function phaseLine(state: Phase): PhaseLine {
  switch (state) {
    case "working":
      return { line: "Agent working", open: true };
    case "settling":
      return { line: "Agent finishing its turn", open: true };
    case "finished":
      return { line: "Agent idle", open: true };
    case "waiting_on_event":
      return { line: "Agent waiting on an event", open: true };
    case "finished_with_background":
      return { line: "Agent idle, background work running", open: true };
    case "blocked":
      return { line: "Agent blocked", open: true };
    case "needs_human":
      return { line: "Agent needs you", open: true };
    case "offline":
      return { line: "Agent offline", open: true };
    case "hibernated":
      return { line: "Agent hibernated", open: true };
    case "limit_reached":
      return { line: "Agent at its usage limit", open: true };
    case "unknown":
      return { line: "Agent state unknown", open: true };
    case "gone":
      return { line: "Agent ended", open: false };
  }
}

/** The phase line for `row`, which may not be listed. */
export function rowLine(row: AgentRow | undefined): PhaseLine {
  return row === undefined ? { line: "Agent not in the agent list", open: true } : phaseLine(row.state);
}

/** The send shortcut as this platform writes it. */
export function sendHint(platform: string): string {
  return `${/Mac|iPhone|iPad|iPod/.test(platform) ? "⌘" : "Ctrl"} + Enter to send`;
}

/**
 * What one agent's transcript has shown: its highest seq, its people's
 * messages by seq, and whether its stream is Current.
 */
type Seen = { readonly cursor: number; readonly texts: ReadonlyMap<number, string>; readonly current: boolean };

/**
 * Composer state that outlives one agent's composer: drafts by agent, what
 * each agent's transcript has shown, and the transcript cursor each sent
 * message was sent at.
 */
export type ComposerState = {
  readonly drafts: Map<string, string>;
  /** The transcript applied `seq`, a person's message when `text` is not null. */
  readonly observe: (agent: string, seq: number, text: string | null) => void;
  /** The agent's transcript stream became Current, or (re)started replaying. */
  readonly current: (agent: string, current: boolean) => void;
  /** `cmdId` was just sent to `agent`. */
  readonly sent: (agent: string, cmdId: string) => void;
  /** Which accepted commands' events the transcript has shown (see the module doc). */
  readonly echoed: (agent: string, accepted: readonly { readonly cmdId: string; readonly text: string }[]) => ReadonlySet<string>;
};

/** Creates the {@link ComposerState} of one workbench. */
export function createComposerState(): ComposerState {
  const [seen, setSeen] = createSignal<ReadonlyMap<string, Seen>>(new Map());
  const sentAt = new Map<string, number>();
  const of = (agent: string): Seen => seen().get(agent) ?? { cursor: 0, texts: new Map(), current: false };
  return {
    drafts: new Map(),
    observe: (agent, seq, text) => {
      const before = of(agent);
      if (seq <= before.cursor && (text === null || before.texts.get(seq) === text)) return;
      const texts = text === null ? before.texts : new Map(before.texts).set(seq, text);
      setSeen(new Map(seen()).set(agent, { ...before, cursor: Math.max(before.cursor, seq), texts }));
    },
    current: (agent, current) => {
      if (of(agent).current !== current) setSeen(new Map(seen()).set(agent, { ...of(agent), current }));
    },
    sent: (agent, cmdId) => {
      // Before the transcript is Current its cursor is not the stream's: the message stays unfoldable.
      const now = of(agent);
      if (now.current) sentAt.set(cmdId, now.cursor);
    },
    echoed: (agent, accepted) => {
      const turns = [...of(agent).texts].sort(([a], [b]) => a - b);
      const used = new Set<number>();
      const matched = new Set<string>();
      // A command restored from storage, or sent before Current, has no send cursor: it stays listed.
      const foldable = accepted.flatMap((command) => {
        const after = sentAt.get(command.cmdId);
        return after === undefined ? [] : [{ ...command, after }];
      });
      const fold = (same: boolean): void => {
        for (const { cmdId, text, after } of foldable) {
          if (matched.has(cmdId)) continue;
          const echo = turns.find(([seq, shown]) => seq > after && !used.has(seq) && (!same || shown === text));
          if (echo === undefined) continue;
          used.add(echo[0]);
          matched.add(cmdId);
        }
      };
      fold(true);
      fold(false);
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
    const mode = rowLine(props.row);
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
        <span class="composer__mode" data-testid="agent-phase">
          {blocked() ?? error() ?? rowLine(props.row).line}
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
