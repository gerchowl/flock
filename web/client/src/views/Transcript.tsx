/**
 * The transcript view: one session stream rendered as a list of events
 * (G1; ADR-0038 §4).
 *
 * It subscribes through a {@link Transport}, so the same view runs on the
 * fixture replay now and on G3's wire adapter later. It derives presentation
 * only (ADR-0038 §3): labels, collapse and layout, never agent state.
 *
 * Every row is isolated: an unknown entry renders through `UnknownBlock`,
 * and a known event whose content the view cannot render falls back to the
 * same block instead of breaking the list.
 *
 * It reads as turns (ADR-0038 §2a, `turns.ts`): a tool run is one line that
 * expands on demand, and all but the last {@link VISIBLE_TURNS} turns fold
 * into one "N earlier turns" control.
 *
 * Replay is linear (#1782): each entry is folded into the last turn as it
 * arrives ({@link foldEntry}), and only the unfolded turns are read to
 * render, so a long history costs O(1) per entry, not a regroup of it all.
 *
 * Known limits, left to ADR-0036's S9a and S18 (tracked in #1307): no
 * virtualisation, no in-flight streaming block; stream deltas are listed as
 * their own rows.
 *
 * @module
 */
import { ErrorBoundary, For, Match, Show, Switch, createSignal, onCleanup, type JSX } from "solid-js";
import { createStore, produce, reconcile } from "solid-js/store";

import type { JsonValue } from "../gen/serde_json/JsonValue";
import type { KnownSessionEvent } from "../gen/KnownSessionEvent";
import type { SessionEntry, SessionStream, Transport, TransportFailure } from "../transport";
import { emptyFold, foldAll, foldEntry, type ToolRun, type TurnFold } from "./turns";
import { UnknownBlock } from "./UnknownBlock";

/**
 * What the stream is: a recorded session replayed (fixture mode), or a
 * live agent's stream from the server (wire mode). Only the status
 * wording differs.
 */
export type TranscriptSource = "replay" | "live";

/** How many of the latest turns stay unfolded. */
export const VISIBLE_TURNS = 3;

/** Props of {@link Transcript}. */
export type TranscriptProps = {
  readonly transport: Transport;
  readonly stream: SessionStream;
  readonly source: TranscriptSource;
  /** Told each entry's seq as it is applied, with its text when it is a person's message. */
  readonly onEntry?: (seq: number, userText: string | null) => void;
  /** Told false when the stream starts replaying, and true once it is Current (`synced`). */
  readonly onCurrent?: (current: boolean) => void;
};

/** A person's message text: a `user` event's string content, or its text blocks. */
function userText(entry: SessionEntry): string | null {
  if (entry.kind !== "known" || entry.event.type !== "user") return null;
  const content = entry.event.message["content"];
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return null;
  const texts = content.flatMap((block) =>
    typeof block === "object" && block !== null && !Array.isArray(block) && block["type"] === "text" && typeof block["text"] === "string" ? [block["text"]] : [],
  );
  return texts.length > 0 ? texts.join("\n") : null;
}

const WORDING = {
  replay: { loading: "Replaying…", synced: (count: number) => `End of recording · ${count} events`, failed: "Replay failed" },
  live: { loading: "Loading history…", synced: () => "Up to date", failed: "Couldn't load this agent's history" },
} as const;

type Progress = { readonly phase: "replaying" | "synced" } | { readonly phase: "failed"; readonly failure: TransportFailure };

function json(value: JsonValue | undefined): string {
  return JSON.stringify(value, null, 2) ?? "";
}

/** A one-line label from the catalogue discriminators. */
function label(event: KnownSessionEvent): string {
  switch (event.type) {
    case "assistant":
      return "Assistant";
    case "user":
      return "User";
    case "system":
      return event.subtype === "init" ? `Session started · ${event.model}` : `System · ${event.subtype}`;
    case "result":
      return `Result · ${event.subtype}${event.terminal_reason ? ` · ${event.terminal_reason}` : ""}`;
    case "stream_event":
      return `Stream · ${event.event.type}`;
    case "control_request":
      return `Permission request · ${event.request.tool_name}`;
    case "control_cancel_request":
      return "Control request cancelled";
    case "control_response":
      return `Control response · ${event.response.subtype}`;
    case "command_lifecycle":
      return `Command · ${event.state}`;
    case "rate_limit_event":
      return "Rate limit";
  }
}

function Collapsed(props: { readonly title: string; readonly body: string }): JSX.Element {
  return (
    <details class="block">
      <summary>{props.title}</summary>
      <pre class="code">{props.body}</pre>
    </details>
  );
}

/** A tool run as one line: tool, command and outcome; its result expands. */
function ToolRunLine(props: { readonly run: ToolRun }): JSX.Element {
  // Read through `props` so a result arriving later updates this line in place.
  const outcome = (): string => (props.run.result === null ? "running" : props.run.result["is_error"] === true ? "error" : "ok");
  const content = (): JsonValue | undefined => props.run.result?.["content"];
  return (
    <details class="tool-run" data-testid="tool-run">
      <summary>
        <span class="tool-run__name">{props.run.name}</span>
        <code class="tool-run__command">{props.run.command}</code>
        <span class={`tool-run__outcome tool-run__outcome--${outcome()}`}>{outcome()}</span>
      </summary>
      <pre class="code">{typeof content() === "string" ? (content() as string) : json(content())}</pre>
    </details>
  );
}

/** One content block of an API message. */
function ContentBlock(props: { readonly block: JsonValue; readonly turns: TurnFold }): JSX.Element {
  const block = props.block;
  if (typeof block !== "object" || block === null || Array.isArray(block)) {
    throw new Error("content block is not an object");
  }
  switch (block["type"]) {
    case "text":
      return <p class="text">{String(block["text"] ?? "")}</p>;
    case "tool_use": {
      const id = block["id"];
      const run = typeof id === "string" ? props.turns.runs[id] : undefined;
      if (typeof id === "string" && run !== undefined) return <ToolRunLine run={props.turns.runs[id] ?? run} />;
      return <Collapsed title={`Tool · ${String(block["name"] ?? "?")}`} body={json(block["input"])} />;
    }
    case "tool_result": {
      const content = block["content"];
      const title = block["is_error"] === true ? "Tool result · error" : "Tool result";
      return (
        <Show when={props.turns.absorbed[String(block["tool_use_id"])] !== true}>
          <Collapsed title={title} body={typeof content === "string" ? content : json(content)} />
        </Show>
      );
    }
    case "thinking":
      return <Collapsed title="Thinking" body={String(block["thinking"] ?? "")} />;
    default:
      return <Collapsed title={`Block · ${String(block["type"])}`} body={json(block)} />;
  }
}

/** An API message's content: a string or an array of blocks. */
function MessageContent(props: { readonly message: { readonly [key: string]: JsonValue }; readonly turns: TurnFold }): JSX.Element {
  const content = props.message["content"];
  if (typeof content === "string") return <p class="text">{content}</p>;
  if (!Array.isArray(content)) throw new Error("message content is neither text nor blocks");
  return <For each={content}>{(block) => <ContentBlock block={block} turns={props.turns} />}</For>;
}

function KnownEvent(props: { readonly event: KnownSessionEvent; readonly turns: TurnFold }): JSX.Element {
  const event = props.event;
  return (
    <article class={`event event--${event.type}`} data-testid="known-event">
      <h2 class="event__label">{label(event)}</h2>
      {event.type === "assistant" || event.type === "user" ? (
        <MessageContent message={event.message} turns={props.turns} />
      ) : event.type === "result" && typeof event.result === "string" ? (
        <p class="text">{event.result}</p>
      ) : (
        <Collapsed title="Event JSON" body={json(event)} />
      )}
    </article>
  );
}

function Entry(props: { readonly entry: SessionEntry; readonly turns: TurnFold }): JSX.Element {
  const entry = props.entry;
  if (entry.kind === "unknown") return <UnknownBlock input={entry.unknown} />;
  if (entry.kind === "gap") {
    return (
      <p class="transcript__gap" role="note" data-testid="transcript-gap">
        Events {entry.from}–{entry.to} of this session are no longer stored.
      </p>
    );
  }
  return (
    <ErrorBoundary fallback={() => <UnknownBlock input={{ raw: entry.event, shape: null }} />}>
      <KnownEvent event={entry.event} turns={props.turns} />
    </ErrorBoundary>
  );
}

/** Renders a session stream from its first entry, as the transport delivers it. */
export function Transcript(props: TranscriptProps): JSX.Element {
  // Every entry, for the rare regroup; the store holds the turns the view reads.
  const entries: SessionEntry[] = [];
  const [count, setCount] = createSignal(0);
  const [turns, setTurns] = createStore<TurnFold>(emptyFold());
  const [progress, setProgress] = createSignal<Progress>({ phase: "replaying" });
  props.onCurrent?.(false);
  const subscription = props.transport.subscribe(props.stream, null, {
    entry: (entry) => {
      entries.push(entry);
      setCount(entries.length);
      let regroup = false;
      setTurns(produce((fold) => void (regroup = !foldEntry(fold, entry))));
      if (regroup) setTurns(reconcile(foldAll(entries), { key: "key" }));
      props.onEntry?.(entry.seq, userText(entry));
    },
    reset: () => {
      entries.length = 0;
      setCount(0);
      setTurns(emptyFold());
      // Entries restart from the stream's first retained seq: replaying again, not Current.
      props.onCurrent?.(false);
    },
    synced: () => {
      setProgress({ phase: "synced" });
      props.onCurrent?.(true);
    },
    failed: (failure) => setProgress({ phase: "failed", failure }),
  });
  onCleanup(subscription.close);
  const failure = (): TransportFailure | undefined => {
    const current = progress();
    return current.phase === "failed" ? current.failure : undefined;
  };
  const [unfolded, setUnfolded] = createSignal(false);
  const folded = (): number => (unfolded() ? 0 : Math.max(0, turns.turns.length - VISIBLE_TURNS));
  return (
    <section class="transcript" aria-label="Transcript">
      <Show when={folded() > 0}>
        <button type="button" class="transcript__fold" onClick={() => setUnfolded(true)} data-testid="earlier-turns">
          {folded()} earlier {folded() === 1 ? "turn" : "turns"}
        </button>
      </Show>
      {/* Only the unfolded turns are read. A turn keeps its identity as it grows, so appends keep it (and an open tool run) mounted. */}
      <For each={turns.turns.slice(folded())}>
        {(turn) => (
          <div class="turn" data-testid="turn">
            <For each={turn.entries}>{(entry) => <Entry entry={entry} turns={turns} />}</For>
          </div>
        )}
      </For>
      <Switch>
        <Match when={progress().phase === "replaying"}>
          <p class="transcript__status">{WORDING[props.source].loading}</p>
        </Match>
        <Match when={progress().phase === "synced"}>
          <p class="transcript__status" data-testid="transcript-synced">
            {WORDING[props.source].synced(count())}
          </p>
        </Match>
      </Switch>
      <Show when={failure()}>
        {(shown) => (
          <p class="transcript__status state-error" role="alert">
            {WORDING[props.source].failed}: {shown().message}
          </p>
        )}
      </Show>
    </section>
  );
}
