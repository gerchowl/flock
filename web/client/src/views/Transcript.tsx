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
 * Known limits, left to ADR-0036's S9a and S18 (tracked in #1307): no turn
 * grouping (ADR-0036 §10a), no virtualisation, no in-flight streaming
 * block; stream deltas are listed as their own rows.
 *
 * @module
 */
import { ErrorBoundary, For, Match, Show, Switch, createSignal, onCleanup, type JSX } from "solid-js";
import { createStore } from "solid-js/store";

import type { JsonValue } from "../gen/serde_json/JsonValue";
import type { KnownSessionEvent } from "../gen/KnownSessionEvent";
import type { SessionEntry, SessionStream, Transport, TransportFailure } from "../transport";
import { UnknownBlock } from "./UnknownBlock";

/**
 * What the stream is: a recorded session replayed (fixture mode), or a
 * live agent's stream from the server (wire mode). Only the status
 * wording differs.
 */
export type TranscriptSource = "replay" | "live";

/** Props of {@link Transcript}. */
export type TranscriptProps = {
  readonly transport: Transport;
  readonly stream: SessionStream;
  readonly source: TranscriptSource;
};

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

/** One content block of an API message. */
function ContentBlock(props: { readonly block: JsonValue }): JSX.Element {
  const block = props.block;
  if (typeof block !== "object" || block === null || Array.isArray(block)) {
    throw new Error("content block is not an object");
  }
  switch (block["type"]) {
    case "text":
      return <p class="text">{String(block["text"] ?? "")}</p>;
    case "tool_use":
      return <Collapsed title={`Tool · ${String(block["name"] ?? "?")}`} body={json(block["input"])} />;
    case "tool_result": {
      const content = block["content"];
      const title = block["is_error"] === true ? "Tool result · error" : "Tool result";
      return <Collapsed title={title} body={typeof content === "string" ? content : json(content)} />;
    }
    case "thinking":
      return <Collapsed title="Thinking" body={String(block["thinking"] ?? "")} />;
    default:
      return <Collapsed title={`Block · ${String(block["type"])}`} body={json(block)} />;
  }
}

/** An API message's content: a string or an array of blocks. */
function MessageContent(props: { readonly message: { readonly [key: string]: JsonValue } }): JSX.Element {
  const content = props.message["content"];
  if (typeof content === "string") return <p class="text">{content}</p>;
  if (!Array.isArray(content)) throw new Error("message content is neither text nor blocks");
  return <For each={content}>{(block) => <ContentBlock block={block} />}</For>;
}

function KnownEvent(props: { readonly event: KnownSessionEvent }): JSX.Element {
  const event = props.event;
  return (
    <article class={`event event--${event.type}`} data-testid="known-event">
      <h2 class="event__label">{label(event)}</h2>
      {event.type === "assistant" || event.type === "user" ? (
        <MessageContent message={event.message} />
      ) : event.type === "result" && typeof event.result === "string" ? (
        <p class="text">{event.result}</p>
      ) : (
        <Collapsed title="Event JSON" body={json(event)} />
      )}
    </article>
  );
}

function Entry(props: { readonly entry: SessionEntry }): JSX.Element {
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
      <KnownEvent event={entry.event} />
    </ErrorBoundary>
  );
}

/** Renders a session stream from its first entry, as the transport delivers it. */
export function Transcript(props: TranscriptProps): JSX.Element {
  const [entries, setEntries] = createStore<SessionEntry[]>([]);
  const [progress, setProgress] = createSignal<Progress>({ phase: "replaying" });
  const subscription = props.transport.subscribe(props.stream, null, {
    entry: (entry) => setEntries(entries.length, entry),
    reset: () => setEntries([]),
    synced: () => setProgress({ phase: "synced" }),
    failed: (failure) => setProgress({ phase: "failed", failure }),
  });
  onCleanup(subscription.close);
  const failure = (): TransportFailure | undefined => {
    const current = progress();
    return current.phase === "failed" ? current.failure : undefined;
  };
  return (
    <section class="transcript" aria-label="Transcript">
      <For each={entries}>{(entry) => <Entry entry={entry} />}</For>
      <Switch>
        <Match when={progress().phase === "replaying"}>
          <p class="transcript__status">{WORDING[props.source].loading}</p>
        </Match>
        <Match when={progress().phase === "synced"}>
          <p class="transcript__status" data-testid="transcript-synced">
            {WORDING[props.source].synced(entries.length)}
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
