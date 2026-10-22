/**
 * The application root: picks what to show from the shell configuration.
 *
 * Both modes render the same workbench (ADR-0038 §2a):
 * - **fixture** replays the recorded session as the `fixture` agent, with
 *   sample rows and inbox cards, under the replay badge (the owner preview);
 * - **wire** connects to the configured server (G3a, #1747). Its agent list
 *   is fleet-lite (#1746); the inbox stays empty until G5 (#1597). Its
 *   token is, in order: one exchanged for the fragment's one-time code, the
 *   one this tab already holds in `sessionStorage`, or the shell's (Tauri).
 *   With none it waits in NeedsToken (ADR-0038 §3, `src/wire/token.ts`).
 *
 * Each mode creates its fleet source and transport once, outside JSX: a
 * prop expression is a getter that would run again on every read.
 *
 * @module
 */
import { Match, Show, Switch, createSignal, onCleanup, type JSX } from "solid-js";

import { ReplayBadge } from "./components/ReplayBadge";
import type { ConfigResult, FixtureShellConfig, WireShellConfig } from "./config";
import { createFixtureFleet } from "./fleet";
import { emptyLayout } from "./layout/model";
import { createLayoutStore } from "./layout/persistence";
import { previewMode } from "./layout/phone";
import { createFixtureTransport } from "./transport/fixture";
import { WireConnection, type ConnectionOptions } from "./wire/connection";
import { browserClock, browserSockets, endpointUrl } from "./wire/env";
import { createWireFleet } from "./wire/fleet";
import { captureCode, type CodePage } from "./capture-code";
import { EXCHANGE_FAILED, RUN_CLIENT_OPEN, createTokenStore, exchangeCode, type ExchangeRequest, type SessionLike } from "./wire/token";
import { createWireTransport } from "./wire/transport";
import { ConnectionBar } from "./workbench/ConnectionBar";
import type { Sender } from "./workbench/Composer";
import { SAMPLE_INBOX } from "./workbench/inbox";
import { SAMPLE_AGENTS } from "./workbench/samples";
import { Workbench } from "./workbench/Workbench";

/** What a live connection runs on; tests inject virtual ones. */
export type WireEnv = Pick<ConnectionOptions, "clock" | "sockets" | "random" | "store"> & {
  /** This tab's session storage; reading it may throw. Defaults to `sessionStorage`. */
  readonly session?: () => SessionLike | undefined;
  /** The code exchange's request. Defaults to `fetch`. */
  readonly request?: ExchangeRequest;
  /** Where a pasted link's code arrives (`hashchange`). Defaults to `window`. */
  readonly page?: CodePage & {
    addEventListener(type: "hashchange", listener: () => void): void;
    removeEventListener(type: "hashchange", listener: () => void): void;
  };
};

/** Props of {@link App}. `env` defaults to the browser's. */
export type AppProps = {
  readonly config: ConfigResult;
  readonly env?: WireEnv;
  /** The one-time code already taken out of the fragment (`src/capture-code.ts`), or null. */
  readonly code?: string | null;
};

/** This bundle's identity in `hello` (diagnostic only, ADR-0036 §5). */
const CLIENT = { name: "flock-web", version: "0.0.0" } as const;

function FixtureApp(props: { readonly config: FixtureShellConfig }): JSX.Element {
  const store = createLayoutStore({ serverKey: "fixture" });
  const { layout, notice } = store.load();
  const fleet = createFixtureFleet(SAMPLE_AGENTS);
  const transport = createFixtureTransport(props.config.fixtureJsonl);
  return (
    <>
      <ReplayBadge source={props.config.source} />
      <Workbench
        fleet={fleet}
        transport={transport}
        inbox={SAMPLE_INBOX}
        sender={null}
        layout={layout}
        saveLayout={(next) => store.save(next)}
        status={() => notice}
        mode={previewMode(location.search)}
        source="replay"
      />
    </>
  );
}

function WireApp(props: { readonly config: WireShellConfig; readonly env: WireEnv; readonly code: string | null }): JSX.Element {
  const { session = () => sessionStorage, request = globalThis.fetch.bind(globalThis), page = window, ...options } = props.env;
  const url = endpointUrl(props.config.serverUrl);
  const tokens = createTokenStore(session);
  /** The token to fall back on: this tab's stored one, or the shell's (Tauri). */
  const held = (): string | null => tokens.get() ?? props.config.token ?? null;
  // With a code, nothing connects until its exchange settles.
  let current = props.code === null ? held() : null;
  const connection = new WireConnection({ url, token: current, client: CLIENT, ...options });
  const [exchanging, setExchanging] = createSignal(props.code !== null);
  /** The last code failed: NeedsToken says so instead of only asking for one. */
  const [failed, setFailed] = createSignal(false);
  /** A failed code while a session works on: a dismissible notice, never NeedsToken. */
  const [linkNotice, setLinkNotice] = createSignal<string | null>(null);
  const describe = (): string => {
    if (exchanging() && (connection.state === "Disconnected" || connection.state === "NeedsToken")) return "Signing in…";
    const ask = connection.state === "NeedsToken" ? (failed() ? EXCHANGE_FAILED : RUN_CLIENT_OPEN) : null;
    const parts = [connection.state, connection.error, ask].filter((part) => part !== null);
    return `${parts[0]}${parts.length > 1 ? ` · ${parts.slice(1).join(" ")}` : ""}`;
  };
  const [status, setStatus] = createSignal<string>(describe());
  const refresh = (): void => void setStatus(describe());
  onCleanup(connection.onChange(refresh));
  const [revision, setRevision] = createSignal(0);
  onCleanup(connection.onChange(() => setRevision(revision() + 1)));
  const sender: Sender = {
    commands: () => (revision(), connection.commands()),
    // `submit` sends the message, not only types it (`AgentSendParams`; S7a's agent-send script).
    send: (agent, text) => connection.command("agent.send", { target: agent, text, submit: true }),
    blocked: () => (revision(), connection.principal === null ? "Sending opens once the server has welcomed this client." : null),
  };
  onCleanup(connection.onTransition((state) => state === "NeedsToken" && tokens.clear()));
  let disposed = false;
  onCleanup(() => {
    disposed = true;
    connection.disconnect();
  });
  /** Connects with `token`, reconnecting when the connection runs on another one. */
  const use = (token: string | null): void => {
    if (connection.state !== "Disconnected" && connection.state !== "NeedsToken" && token === current) return;
    current = token;
    if (connection.state !== "Disconnected" && connection.state !== "NeedsToken") connection.disconnect();
    if (token !== null) connection.setToken(token);
    if (connection.state === "Disconnected") connection.connect();
  };
  /**
   * Exchanges `code`, exactly once: a failed code is never retried
   * (ADR-0038 §3). A new code replaces the stored token only on success; a
   * failure leaves the token and a working session as they are.
   */
  let latest = 0;
  const signInWith = (code: string): void => {
    // Only the latest code's result counts: a slower, older exchange is ignored.
    const generation = ++latest;
    setExchanging(true);
    refresh();
    void exchangeCode(code, props.config.serverUrl, request).then((exchanged) => {
      if (disposed || generation !== latest) return;
      setExchanging(false);
      if (exchanged !== null) {
        tokens.set(exchanged);
        setFailed(false);
        setLinkNotice(null);
        // A new token is a new identity (a changed principal drops the outbox).
        use(exchanged);
      } else if (connection.state === "Disconnected" || connection.state === "NeedsToken") {
        setFailed(true);
        // Nothing works on yet: fall back on a held token, or wait in NeedsToken.
        const fallback = held();
        if (fallback !== null) setLinkNotice(EXCHANGE_FAILED);
        use(fallback);
      } else setLinkNotice(EXCHANGE_FAILED);
      refresh();
    });
  };
  if (props.code === null) connection.connect();
  else signInWith(props.code);
  // A pasted `flk client open` link changes only the fragment: take its code the same way.
  const onHashChange = (): void => {
    const code = captureCode(page);
    if (code !== null) signInWith(code);
  };
  page.addEventListener("hashchange", onHashChange);
  onCleanup(() => page.removeEventListener("hashchange", onHashChange));
  const store = createLayoutStore({ serverKey: url, initial: emptyLayout });
  const { layout, notice } = store.load();
  // One fleet source and one transport per connection (ADR-0038 §3, sub ids).
  const fleet = createWireFleet(connection);
  const transport = createWireTransport(connection);
  return (
    <>
      <ConnectionBar connection={connection} revision={revision} />
      <Show when={linkNotice()}>
        {(message) => (
          <div class="notice" role="status" data-testid="exchange-notice">
            <span>{message()}</span>
            <button type="button" class="action" onClick={() => setLinkNotice(null)}>
              Dismiss
            </button>
          </div>
        )}
      </Show>
      <Workbench
        fleet={fleet}
        transport={transport}
        inbox={[]}
        sender={sender}
        layout={layout}
        saveLayout={(next) => store.save(next)}
        status={() => [notice, status()].filter((part) => part !== null).join(" · ")}
        mode={previewMode(location.search)}
        source="live"
      />
    </>
  );
}

/** Renders the configured mode, or the configuration error as an explicit error state. */
export function App(props: AppProps): JSX.Element {
  return (
    <Switch>
      <Match when={!props.config.ok && props.config.error}>
        {(error) => (
          <main class="state-error" role="alert" data-testid="config-error">
            <h1>Cannot start the client</h1>
            <p>{error().message}</p>
          </main>
        )}
      </Match>
      <Match when={props.config.ok && props.config.config}>
        {(config) => {
          const current = config();
          if (current.transport === "fixture") return <FixtureApp config={current} />;
          return <WireApp config={current} env={props.env ?? { clock: browserClock, sockets: browserSockets, random: Math.random, store: localStorage }} code={props.code ?? null} />;
        }}
      </Match>
    </Switch>
  );
}
