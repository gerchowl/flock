/**
 * The application root: picks what to show from the shell configuration.
 *
 * Both modes render the same workbench (ADR-0038 §2a):
 * - **fixture** replays the recorded session as the `fixture` agent, with
 *   sample rows and inbox cards, under the replay badge (the owner preview);
 * - **wire** connects to the configured server (G3a, #1747). Its agent list
 *   is fleet-lite (#1746); its inbox lists the open permission prompts (#1749).
 *   Its token is, in order: one exchanged for the fragment's one-time code, the
 *   one this tab already holds in `sessionStorage`, or the shell's fixed one.
 *   With none it waits in NeedsToken (ADR-0038 §3, `src/wire/token.ts`). A
 *   shell's token provider (Tauri, ADR-0038 §6) replaces all three
 *   (`src/wire/provider.ts`): its token is kept in memory only, a link's code is
 *   refused with a notice, and NeedsToken asks the shell instead of the person.
 * - **bridge** runs the same live app on sessions the shell's native side
 *   opens (ADR-0038 §6, #2500), driven by the provider's rules: no token in
 *   the webview at all, so nothing is prompted, exchanged or stored.
 *
 * Each mode creates its fleet source and transport once, outside JSX: a
 * prop expression is a getter that would run again on every read.
 *
 * @module
 */
import { Match, Show, Switch, createSignal, onCleanup, type JSX } from "solid-js";

import { ReplayBadge } from "./components/ReplayBadge";
import { pageSearch, type BridgeShellConfig, type ConfigResult, type FixtureShellConfig, type WireShellConfig } from "./config";
import { createFixtureFleet } from "./fleet";
import { emptyLayout } from "./layout/model";
import { createLayoutStore } from "./layout/persistence";
import { createSidebarStore } from "./layout/sidebar";
import { previewMode } from "./layout/phone";
import { createFixtureTransport } from "./transport/fixture";
import { WireConnection } from "./wire/connection";
import { browserClock, browserSockets, endpointUrl, type Clock, type KeyValueStore, type SocketFactory } from "./wire/env";
import { createWireFleet } from "./wire/fleet";
import { captureCode, type CodePage } from "./capture-code";
import { EXCHANGE_FAILED, RUN_CLIENT_OPEN, createTokenStore, exchangeCode, isCode, type ExchangeRequest, type SessionLike } from "./wire/token";
import { driveBridge, driveTokenProvider, type ShellTokens } from "./wire/provider";
import { createWireTransport } from "./wire/transport";
import { ConnectionBar } from "./workbench/ConnectionBar";
import { createWireSender } from "./workbench/sender";
import { SAMPLE_INBOX } from "./workbench/inbox";
import { SAMPLE_AGENTS, SAMPLE_ENTITIES } from "./workbench/samples";
import { Workbench } from "./workbench/Workbench";

/** What a live connection runs on; tests inject virtual ones. */
export type WireEnv = {
  readonly clock: Clock;
  /** The WebSocket factory; a bridge config never uses it. */
  readonly sockets: SocketFactory;
  readonly random: () => number;
  readonly store: KeyValueStore;
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

/** The layout's server key for the shell's bridge, which names no server URL. */
const BRIDGE_KEY = "bridge:local";

/** Why a link's code is refused when the shell provides tokens (ADR-0038 §6). */
export const SHELL_OWNS_TOKENS = "This app gets its token from its shell, so the link's code was not used.";

function FixtureApp(props: { readonly config: FixtureShellConfig }): JSX.Element {
  const store = createLayoutStore({ serverKey: "fixture" });
  const { layout, notice } = store.load();
  const sidebar = createSidebarStore({ serverKey: "fixture" });
  const fleet = createFixtureFleet(SAMPLE_AGENTS, SAMPLE_ENTITIES);
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
        sidebar={sidebar.load()}
        saveSidebar={sidebar.save}
        status={() => notice}
        mode={previewMode(pageSearch(globalThis))}
        source="replay"
      />
    </>
  );
}

function WireApp(props: { readonly config: WireShellConfig | BridgeShellConfig; readonly env: WireEnv; readonly code: string | null }): JSX.Element {
  const { session = () => sessionStorage, request = globalThis.fetch.bind(globalThis), page = window, sockets, ...options } = props.env;
  const wire = props.config.transport === "wire" ? props.config : null;
  const url = wire === null ? null : endpointUrl(wire.serverUrl);
  const tokens = createTokenStore(session);
  const provider = wire?.tokenProvider;
  /** The shell owns this app's tokens: a provider gives them, or a bridge keeps them in the shell. */
  const shellOwned = wire === null || provider !== undefined;
  /** The token to fall back on: this tab's stored one, or the shell's fixed one; none when the shell owns them. */
  const held = (): string | null => (shellOwned ? null : (tokens.get() ?? wire?.token ?? null));
  // With a code, nothing connects until its exchange settles (a shell that owns the tokens never exchanges one).
  let current = props.code === null ? held() : null;
  const connection = new WireConnection(url === null ? { bridged: true, client: CLIENT, ...options } : { url, token: current, sockets, client: CLIENT, ...options });
  const [exchanging, setExchanging] = createSignal(props.code !== null && !shellOwned);
  /** The last code failed: NeedsToken says so instead of only asking for one. */
  const [failed, setFailed] = createSignal(false);
  /** A failed code while a session works on: a dismissible notice, never NeedsToken. */
  const [linkNotice, setLinkNotice] = createSignal<string | null>(null);
  /** The shell's token provider, driven by NeedsToken; set below, once `refresh` exists. */
  let shell: ShellTokens | null = null;
  const describe = (): string => {
    const idle = connection.state === "Disconnected" || connection.state === "NeedsToken";
    // A shell still asked after a Disconnect is not signing in: its token waits for Connect.
    if ((exchanging() && idle) || (shell?.asking === true && connection.state === "NeedsToken")) return "Signing in…";
    const prompt = shell === null ? (failed() ? EXCHANGE_FAILED : RUN_CLIENT_OPEN) : shell.error;
    const ask = connection.state === "NeedsToken" ? prompt : null;
    const parts = [connection.state, connection.error, ask].filter((part) => part !== null);
    return `${parts[0]}${parts.length > 1 ? ` · ${parts.slice(1).join(" ")}` : ""}`;
  };
  const [status, setStatus] = createSignal<string>(describe());
  /** A failed renewal while the session runs on (ADR-0038 §6): a dismissible notice, never NeedsToken. */
  const [renewalNotice, setRenewalNotice] = createSignal<string | null>(null);
  const refresh = (): void => {
    setStatus(describe());
    setRenewalNotice(shell?.notice ?? null);
  };
  // Before the first connect, so its first attempt already asks the shell.
  const driven =
    props.config.transport === "bridge"
      ? driveBridge(connection, props.config.openBridge, options.clock, options.random, refresh)
      : provider === undefined
        ? null
        : driveTokenProvider(connection, provider, options.clock, options.random, refresh);
  if (driven !== null) {
    shell = driven;
    onCleanup(() => driven.dispose());
  }
  onCleanup(connection.onChange(refresh));
  const [revision, setRevision] = createSignal(0);
  onCleanup(connection.onChange(() => setRevision(revision() + 1)));
  const sender = createWireSender(connection, revision);
  /** True while a failed code settles: its own move into NeedsToken keeps the message. */
  let settling = false;
  onCleanup(
    connection.onTransition((state) => {
      if (state !== "NeedsToken") return;
      tokens.clear();
      // A later NeedsToken (the server refused the token) is a new reason: the old link's message is stale.
      if (!settling) {
        setFailed(false);
        setLinkNotice(null);
      }
    }),
  );
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
  /** A code that failed, or never was one: keeps the token and a working session as they are. */
  const failExchange = (): void => {
    setExchanging(false);
    if (connection.state === "Disconnected" || connection.state === "NeedsToken") {
      setFailed(true);
      // Nothing works on yet: fall back on a held token, or wait in NeedsToken.
      const fallback = held();
      if (fallback !== null) setLinkNotice(EXCHANGE_FAILED);
      settling = true;
      try {
        use(fallback);
      } finally {
        settling = false;
      }
    } else setLinkNotice(EXCHANGE_FAILED);
    refresh();
  };
  /**
   * Exchanges `code`, exactly once: a failed code is never retried
   * (ADR-0038 §3). A new code replaces the stored token only on success; a
   * failure leaves the token and a working session as they are. A malformed
   * code is a failure without an exchange, and never supersedes one in flight.
   */
  let latest = 0;
  const signInWith = (code: string): void => {
    // The shell owns this app's tokens, in memory only: a link's code is never exchanged or stored.
    if (shell !== null || wire === null) return void setLinkNotice(SHELL_OWNS_TOKENS);
    if (!isCode(code)) {
      // A bad paste must not discard a good exchange that already consumed its code.
      if (!exchanging()) failExchange();
      return;
    }
    // Only the latest code's result counts: a slower, older exchange is ignored.
    const generation = ++latest;
    setExchanging(true);
    refresh();
    void exchangeCode(code, wire.serverUrl, request).then((exchanged) => {
      if (disposed || generation !== latest) return;
      if (exchanged === null) return failExchange();
      setExchanging(false);
      tokens.set(exchanged);
      setFailed(false);
      setLinkNotice(null);
      // A new token is a new identity (a changed principal drops the outbox).
      use(exchanged);
      refresh();
    });
  };
  if (props.code === null || shell !== null) connection.connect();
  if (props.code !== null) {
    if (shell !== null || isCode(props.code)) signInWith(props.code);
    else failExchange();
  }
  // A pasted `flk client open` link changes only the fragment: take its code the same way.
  const onHashChange = (): void => {
    const code = captureCode(page);
    if (code !== null) signInWith(code);
  };
  page.addEventListener("hashchange", onHashChange);
  onCleanup(() => page.removeEventListener("hashchange", onHashChange));
  const store = createLayoutStore({ serverKey: url ?? BRIDGE_KEY, initial: emptyLayout });
  const { layout, notice } = store.load();
  const sidebar = createSidebarStore({ serverKey: url ?? BRIDGE_KEY });
  // One fleet source and one transport per connection (ADR-0038 §3, sub ids).
  const fleet = createWireFleet(connection);
  const transport = createWireTransport(connection);
  return (
    <>
      <ConnectionBar connection={connection} revision={revision} ask={shell === null ? undefined : () => shell?.ask()} />
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
      <Show when={renewalNotice()}>
        {(message) => (
          <div class="notice" role="status" data-testid="renewal-notice">
            <span>{message()}</span>
            <button type="button" class="action" onClick={() => shell?.dismissNotice()}>
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
        sidebar={sidebar.load()}
        saveSidebar={sidebar.save}
        status={() => [notice, status()].filter((part) => part !== null).join(" · ")}
        mode={previewMode(pageSearch(globalThis))}
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
