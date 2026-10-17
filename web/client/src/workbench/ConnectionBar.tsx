/**
 * The live connection's strip (ADR-0038 §3): its state, the latest failure
 * while reconnecting or stopped, the always-offered *Disconnect* (or
 * *Connect*), and in NeedsToken a field for a new token. The token is
 * handed to the connection and never shown, logged or stored by the page.
 *
 * @module
 */
import { Show, createSignal, type JSX } from "solid-js";

import type { WireConnection } from "../wire/connection";

/** Props of {@link ConnectionBar}; `revision` changes with every connection change. */
export type ConnectionBarProps = { readonly connection: WireConnection; readonly revision: () => number };

/** The connection strip above the workbench. */
export function ConnectionBar(props: ConnectionBarProps): JSX.Element {
  const [token, setToken] = createSignal("");
  const state = (): WireConnection["state"] => (props.revision(), props.connection.state);
  const error = (): string | null => (props.revision(), props.connection.error);
  return (
    <div class="connection-bar" classList={{ "connection-bar--live": state() === "Live" }} role="status" data-testid="connection-bar">
      <strong class="connection-bar__state">{state()}</strong>
      <Show when={error()}>{(message) => <span class="connection-bar__error">{message()}</span>}</Show>
      <Show when={state() === "NeedsToken"}>
        <form
          class="connection-bar__token"
          onSubmit={(event) => {
            event.preventDefault();
            if (token() === "") return;
            props.connection.setToken(token());
            setToken("");
          }}
        >
          <input type="password" aria-label="New client token" autocomplete="off" value={token()} onInput={(event) => setToken(event.currentTarget.value)} />
          <button type="submit" class="action">
            Use token
          </button>
        </form>
      </Show>
      <Show
        when={state() === "Disconnected"}
        fallback={
          <button type="button" class="action connection-bar__toggle" onClick={() => props.connection.disconnect()}>
            Disconnect
          </button>
        }
      >
        <button type="button" class="action connection-bar__toggle" onClick={() => props.connection.connect()}>
          Connect
        </button>
      </Show>
    </div>
  );
}
