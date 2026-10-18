/**
 * The application root: picks what to show from the shell configuration.
 *
 * Both modes render the same workbench (ADR-0038 §2a):
 * - **fixture** replays the recorded session as the `fixture` agent, with
 *   sample rows and inbox cards, under the replay badge (the owner preview);
 * - **wire** connects to the configured server (G3a, #1747). Its agent list
 *   is fleet-lite (#1746); its inbox lists the open permission prompts (#1749).
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
import { previewMode } from "./layout/phone";
import { createFixtureTransport } from "./transport/fixture";
import { WireConnection, type ConnectionOptions } from "./wire/connection";
import { browserClock, browserSockets, endpointUrl } from "./wire/env";
import { createWireFleet } from "./wire/fleet";
import { createWireTransport } from "./wire/transport";
import { ConnectionBar } from "./workbench/ConnectionBar";
import { createWireSender } from "./workbench/sender";
import { SAMPLE_INBOX } from "./workbench/inbox";
import { SAMPLE_AGENTS } from "./workbench/samples";
import { Workbench } from "./workbench/Workbench";

/** What a live connection runs on; tests inject virtual ones. */
export type WireEnv = Pick<ConnectionOptions, "clock" | "sockets" | "random" | "store">;

/** Props of {@link App}. `env` defaults to the browser's. */
export type AppProps = { readonly config: ConfigResult; readonly env?: WireEnv };

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

function WireApp(props: { readonly config: WireShellConfig; readonly env: WireEnv }): JSX.Element {
  const url = endpointUrl(props.config.serverUrl);
  const connection = new WireConnection({ url, token: props.config.token, client: CLIENT, ...props.env });
  const [revision, setRevision] = createSignal(0);
  onCleanup(connection.onChange(() => setRevision(revision() + 1)));
  const sender = createWireSender(connection, revision);
  connection.connect();
  onCleanup(() => connection.disconnect());
  const store = createLayoutStore({ serverKey: url, initial: emptyLayout });
  const { layout, notice } = store.load();
  // One fleet source and one transport per connection (ADR-0038 §3, sub ids).
  const fleet = createWireFleet(connection);
  const transport = createWireTransport(connection);
  return (
    <>
      <ConnectionBar connection={connection} revision={revision} />
      <Workbench
        fleet={fleet}
        transport={transport}
        inbox={[]}
        sender={sender}
        layout={layout}
        saveLayout={(next) => store.save(next)}
        status={() => notice}
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
          return <WireApp config={current} env={props.env ?? { clock: browserClock, sockets: browserSockets, random: Math.random, store: localStorage }} />;
        }}
      </Match>
    </Switch>
  );
}
