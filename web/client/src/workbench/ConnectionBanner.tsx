/**
 * The live connection's banner (ADR-0038 §3, ADR-0048 D7 amendment 5):
 * shown under the toolbar only while something needs the person, and empty
 * otherwise. It holds the state line while the connection has a problem
 * (reconnecting with the latest failure, read-only with the version gap, a
 * token needed), *Connect* when it stopped on a failure, in NeedsToken a field for a new
 * token (or, when the shell provides tokens, a *Connect* that asks it
 * again), what restoring the stored outbox had to drop, if anything, and
 * what a changed principal dropped, until dismissed or the next acked
 * command. The token is handed to the connection and never shown, logged or
 * stored by the page. The state itself, and *Disconnect*, live in the
 * toolbar's popover (`HostStatus.tsx`).
 *
 * @module
 */
import { Show, createSignal, type JSX } from "solid-js";

import { Cluster } from "../layout/primitives";
import { SPACE } from "../tokens";
import type { WireConnection } from "../wire/connection";
import type { HostView } from "./HostStatus";

/** Props of {@link ConnectionBanner}; `revision` changes with every connection change. */
export type ConnectionBannerProps = {
  readonly connection: WireConnection;
  readonly revision: () => number;
  /** The toolbar's view of the same connection: its health and state line. */
  readonly host: HostView;
  /** The shell provides tokens: NeedsToken asks it again with this, instead of prompting the person. */
  readonly ask?: (() => void) | undefined;
};

/** The banner under the toolbar; renders nothing while the connection needs nothing from the person. */
export function ConnectionBanner(props: ConnectionBannerProps): JSX.Element {
  const [token, setToken] = createSignal("");
  const state = (): WireConnection["state"] => (props.revision(), props.connection.state);
  const readOnly = (): string | null => (props.revision(), props.connection.readOnly);
  const outboxProblem = (): string | null => (props.revision(), props.connection.outboxProblem);
  const outboxDropped = (): string | null => (props.revision(), props.connection.outboxDropped);
  const problem = (): boolean => props.host.health() === "problem";
  return (
    <div role="status" data-testid="connection-banner">
      <Show when={problem() || outboxProblem() !== null || outboxDropped() !== null}>
        <Cluster class="banner" space={SPACE[3]}>
          <Show when={problem()}>
            <span data-testid="banner-detail">{props.host.detail()}</span>
          </Show>
          <Show when={readOnly()}>
            {(message) => (
              <span class="banner__error" data-testid="read-only">
                {message()}
              </span>
            )}
          </Show>
          <Show when={outboxProblem()}>
            {(problem) => (
              <span class="banner__error" data-testid="outbox-problem">
                {problem()}
              </span>
            )}
          </Show>
          <Show when={outboxDropped()}>
            {(dropped) => (
              <span class="banner__error" data-testid="outbox-dropped">
                {dropped()}
                <button type="button" class="bar-button" onClick={() => props.connection.dismissOutboxDropped()}>
                  Dismiss
                </button>
              </span>
            )}
          </Show>
          <Show when={problem() && state() === "Disconnected"}>
            <button type="button" class="bar-button" data-testid="banner-connect" onClick={() => props.connection.connect()}>
              Connect
            </button>
          </Show>
          <Show when={props.ask !== undefined && problem() && state() === "NeedsToken"}>
            <button type="button" class="bar-button" data-testid="shell-connect" onClick={() => props.ask?.()}>
              Connect
            </button>
          </Show>
          <Show when={props.ask === undefined && problem() && state() === "NeedsToken"}>
            <Cluster
              as="form"
              space={SPACE[2]}
              onSubmit={(event: SubmitEvent) => {
                event.preventDefault();
                if (token() === "") return;
                props.connection.setToken(token());
                setToken("");
              }}
            >
              <input class="banner__token" type="password" aria-label="New client token" autocomplete="off" value={token()} onInput={(event) => setToken(event.currentTarget.value)} />
              <button type="submit" class="bar-button">
                Use token
              </button>
            </Cluster>
          </Show>
        </Cluster>
      </Show>
    </div>
  );
}
