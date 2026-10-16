/**
 * The application root: picks what to show from the shell configuration.
 *
 * @module
 */
import { Match, Switch, type JSX } from "solid-js";

import { ReplayBadge } from "./components/ReplayBadge";
import type { ConfigResult } from "./config";

/** Props of {@link App}. */
export type AppProps = { readonly config: ConfigResult };

/**
 * Renders the configured mode, or the configuration error as an explicit
 * error state. The live wire transport arrives with G3 (#1595); until then
 * a wire configuration is reported, never silently replaced by fixtures.
 */
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
          return current.transport === "fixture" ? (
            <>
              <ReplayBadge source={current.source} />
              <main class="app-main" />
            </>
          ) : (
            <main class="state-error" role="alert" data-testid="config-error">
              <h1>Live mode is not available yet</h1>
              <p>This build has no wire transport; it arrives with #1595.</p>
            </main>
          );
        }}
      </Match>
    </Switch>
  );
}
