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
import { Match, Switch, createSignal, onCleanup, type JSX } from "solid-js";

import { ReplayBadge } from "./components/ReplayBadge";
import type { ConfigResult, FixtureShellConfig, WireShellConfig } from "./config";
import { createFixtureFleet } from "./fleet";
import { emptyLayout } from "./layout/model";
import { createLayoutStore } from "./layout/persistence";
import { createFixtureTransport } from "./transport/fixture";
import { WireConnection, type ConnectionOptions } from "./wire/connection";
import { browserClock, browserSockets, endpointUrl } from "./wire/env";
import { createWireFleet } from "./wire/fleet";
import { EXCHANGE_FAILED, RUN_CLIENT_OPEN, createTokenStore, exchangeCode, type ExchangeRequest, type SessionLike } from "./wire/token";
import { createWireTransport } from "./wire/transport";
import { SAMPLE_INBOX } from "./workbench/inbox";
import { SAMPLE_AGENTS } from "./workbench/samples";
import { Workbench } from "./workbench/Workbench";

/** What a live connection runs on; tests inject virtual ones. */
export type WireEnv = Pick<ConnectionOptions, "clock" | "sockets" | "random" | "store"> & {
  /** This tab's session storage; reading it may throw. Defaults to `sessionStorage`. */
  readonly session?: () => SessionLike | undefined;
  /** The code exchange's request. Defaults to `fetch`. */
  readonly request?: ExchangeRequest;
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
        layout={layout}
        saveLayout={(next) => store.save(next)}
        status={() => notice}
        source="replay"
      />
    </>
  );
}

function WireApp(props: { readonly config: WireShellConfig; readonly env: WireEnv; readonly code: string | null }): JSX.Element {
  const { session = () => sessionStorage, request = globalThis.fetch.bind(globalThis), ...options } = props.env;
  const url = endpointUrl(props.config.serverUrl);
  const tokens = createTokenStore(session);
  // A new code replaces whatever token this tab held.
  if (props.code !== null) tokens.clear();
  const token = props.code === null ? (tokens.get() ?? props.config.token ?? null) : null;
  const connection = new WireConnection({ url, token, client: CLIENT, ...options });
  const [signIn, setSignIn] = createSignal<"exchanging" | "failed" | null>(props.code === null ? null : "exchanging");
  const describe = (): string => {
    if (signIn() === "failed") return EXCHANGE_FAILED;
    if (signIn() === "exchanging") return "Signing in…";
    const parts = [connection.state, connection.error, connection.state === "NeedsToken" ? RUN_CLIENT_OPEN : null].filter((part) => part !== null);
    return `${parts[0]}${parts.length > 1 ? ` · ${parts.slice(1).join(" ")}` : ""}`;
  };
  const [status, setStatus] = createSignal<string>(describe());
  onCleanup(connection.onChange(() => setStatus(describe())));
  onCleanup(connection.onTransition((state) => state === "NeedsToken" && tokens.clear()));
  let disposed = false;
  onCleanup(() => {
    disposed = true;
    connection.disconnect();
  });
  if (props.code === null) connection.connect();
  else {
    // Exactly once: a failed code is never retried (ADR-0038 §3).
    void exchangeCode(props.code, props.config.serverUrl, request).then((exchanged) => {
      if (disposed) return;
      if (exchanged === null) {
        setSignIn("failed");
        setStatus(describe());
        return;
      }
      tokens.set(exchanged);
      setSignIn(null);
      connection.setToken(exchanged);
      connection.connect();
      setStatus(describe());
    });
  }
  const store = createLayoutStore({ serverKey: url, initial: emptyLayout });
  const { layout, notice } = store.load();
  // One fleet source and one transport per connection (ADR-0038 §3, sub ids).
  const fleet = createWireFleet(connection);
  const transport = createWireTransport(connection);
  return (
    <Workbench
      fleet={fleet}
      transport={transport}
      inbox={[]}
      layout={layout}
      saveLayout={(next) => store.save(next)}
      status={() => [notice, status()].filter((part) => part !== null).join(" · ")}
      source="live"
    />
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
