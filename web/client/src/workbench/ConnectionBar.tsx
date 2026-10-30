/**
 * The live connection's strip (ADR-0038 §3): its state, the latest failure
 * while reconnecting or stopped, in ReadOnly the banner naming the version
 * gap, the always-offered *Disconnect* (or
 * *Connect*), in NeedsToken a field for a new token (or, when the shell
 * provides tokens, a *Connect* that asks it again), what restoring
 * the stored outbox had to drop, if anything, and what a changed principal
 * dropped, until dismissed or the next acked command. The token is
 * handed to the connection and never shown, logged or stored by the page.
 *
 * @module
 */
import { Show, createSignal, type JSX } from "solid-js";

import type { WireConnection } from "../wire/connection";

/** Props of {@link ConnectionBar}; `revision` changes with every connection change. */
export type ConnectionBarProps = {
  readonly connection: WireConnection;
  readonly revision: () => number;
  /** The shell provides tokens: NeedsToken asks it again with this, instead of prompting the person. */
  readonly ask?: (() => void) | undefined;
};

/** The connection strip above the workbench. */
export function ConnectionBar(props: ConnectionBarProps): JSX.Element {
  const [token, setToken] = createSignal("");
  const state = (): WireConnection["state"] => (props.revision(), props.connection.state);
  const error = (): string | null => (props.revision(), props.connection.error);
  const readOnly = (): string | null => (props.revision(), props.connection.readOnly);
  const outboxProblem = (): string | null => (props.revision(), props.connection.outboxProblem);
  const outboxDropped = (): string | null => (props.revision(), props.connection.outboxDropped);
  return (
    <div class="connection-bar" classList={{ "connection-bar--live": state() === "Live" }} role="status" data-testid="connection-bar">
      <strong class="connection-bar__state">{state()}</strong>
      <Show when={error()}>{(message) => <span class="connection-bar__error">{message()}</span>}</Show>
      <Show when={readOnly()}>
        {(message) => (
          <span class="connection-bar__error" data-testid="read-only">
            {message()}
          </span>
        )}
      </Show>
      <Show when={outboxProblem()}>
        {(problem) => (
          <span class="connection-bar__error" data-testid="outbox-problem">
            {problem()}
          </span>
        )}
      </Show>
      <Show when={outboxDropped()}>
        {(dropped) => (
          <span class="connection-bar__error" data-testid="outbox-dropped">
            {dropped()}
            <button type="button" class="action" onClick={() => props.connection.dismissOutboxDropped()}>
              Dismiss
            </button>
          </span>
        )}
      </Show>
      <Show when={props.ask !== undefined && state() === "NeedsToken"}>
        <button type="button" class="action" data-testid="shell-connect" onClick={() => props.ask?.()}>
          Connect
        </button>
      </Show>
      <Show when={props.ask === undefined && state() === "NeedsToken"}>
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
