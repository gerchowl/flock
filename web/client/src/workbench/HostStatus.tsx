/**
 * The toolbar's connection control (ADR-0048 D7 amendment 5, #2901): a
 * status dot with the server's name, and a popover holding the connection's
 * state line and *Disconnect* (or *Connect*). It shows the state and never
 * names it twice: problems that need the person also raise a banner
 * ({@link ConnectionBanner}), and nothing else does.
 *
 * @module
 */
import { Show, createEffect, createUniqueId, on, onCleanup, type JSX } from "solid-js";

import { Icon } from "../components/Icon";
import { Anchor, AnchorPanel, Stack } from "../layout/primitives";
import { SPACE } from "../tokens";
import type { ConnectionState } from "../wire/connection";
import { pushLayer } from "./keys";
import { RefusalDetails } from "./refusals";
import "./toolbar.css";

/**
 * How the connection reads at a glance: `live`, `pending` (connecting or
 * signing in, nothing failed yet), `problem` (it needs the person, or it
 * failed and retries) or `off` (the person disconnected).
 */
export type Health = "live" | "pending" | "problem" | "off";

/**
 * The connection's {@link Health}. `asking` is true while a token is on its
 * way (a code exchange, or the shell asked for one), which is no problem
 * until a failure says so.
 */
export function healthOf(state: ConnectionState, error: string | null, asking: boolean): Health {
  if (state === "Live") return "live";
  if (error !== null || state === "ReadOnly" || state === "Stale" || state === "Reconnecting" || (state === "NeedsToken" && !asking)) return "problem";
  return state === "Disconnected" && !asking ? "off" : "pending";
}

/** What the dot means, for the trigger's description and the palette. */
export const HEALTH_LABELS: Readonly<Record<Health, string>> = { live: "Connected", pending: "Connecting", problem: "Needs attention", off: "Disconnected" };

/** The live connection as the toolbar shows it; `App.tsx` builds it from its `WireConnection`. */
export type HostView = {
  /** What to call the server while the fleet names no home node: its address, or the shell's machine. */
  readonly name: string;
  readonly health: () => Health;
  /** The state line in plain words (`refusals.tsx`), then what it asks for. */
  readonly detail: () => string;
  /** The raw diagnosis behind the state line's Details, or null. */
  readonly diagnosis: () => string | null;
  /** Whether there is a connection to drop (any state but Disconnected); otherwise *Connect* is offered. */
  readonly connected: () => boolean;
  readonly connect: () => void;
  readonly disconnect: () => void;
};

/** Props of {@link HostStatus}; the open state lives with the caller, so a palette command can open it too. */
export type HostStatusProps = {
  readonly view: HostView;
  /** The server's name: the home node's, or the view's own. */
  readonly name: string;
  readonly open: () => boolean;
  readonly setOpen: (open: boolean) => void;
};

/** The status dot, the server's name and its connection popover. */
export function HostStatus(props: HostStatusProps): JSX.Element {
  const panelId = createUniqueId();
  const detailId = createUniqueId();
  let trigger: HTMLButtonElement | undefined;
  let panel: HTMLElement | undefined;
  let toggle: HTMLButtonElement | undefined;
  const close = (refocus: boolean): void => {
    props.setOpen(false);
    if (refocus) trigger?.focus();
  };
  // Opening moves focus into the popover; Esc closes it and returns focus to the dot, and a click elsewhere closes it.
  createEffect(on(props.open, (open) => open && queueMicrotask(() => toggle?.focus()), { defer: true }));
  // While open, its Esc is a key layer (`keys.ts`); it is not modal, so the page's shortcuts still act.
  createEffect(() => {
    if (!props.open()) return;
    onCleanup(pushLayer({ "layer.close": () => close(true) }, { modal: false }));
  });
  const onPointer = (event: PointerEvent): void => {
    const target = event.target instanceof Node ? event.target : null;
    if (props.open() && !trigger?.contains(target) && !panel?.contains(target)) close(false);
  };
  document.addEventListener("pointerdown", onPointer);
  onCleanup(() => document.removeEventListener("pointerdown", onPointer));
  const health = (): Health => props.view.health();
  return (
    <Anchor class="host">
      <button
        ref={trigger}
        type="button"
        class="host__trigger"
        aria-label={`Connection to ${props.name}`}
        aria-describedby={detailId}
        aria-expanded={props.open()}
        aria-controls={panelId}
        data-testid="host-status"
        data-health={health()}
        title={HEALTH_LABELS[health()]}
        onClick={() => props.setOpen(!props.open())}
      >
        <svg class={`host__dot host__dot--${health()}`} viewBox="0 0 10 10" width="10" height="10" aria-hidden="true">
          <circle cx="5" cy="5" r="4" />
        </svg>
        <span class="host__name">{props.name}</span>
        <span class="host__chevron">
          <Icon name="chevron-down" size={12} />
        </span>
      </button>
      <AnchorPanel ref={panel} as="section" id={panelId} class="host__panel" aria-label={`Connection to ${props.name}`} hidden={!props.open()} data-testid="host-panel">
        <Stack space={SPACE[2]}>
          <strong>{props.name}</strong>
          <span id={detailId} class="host__detail" data-testid="host-detail">
            {props.view.detail()}
          </span>
          <Show when={props.view.diagnosis()}>{(diagnosis) => <RefusalDetails message={diagnosis()} />}</Show>
          <button
            ref={toggle}
            type="button"
            class="bar-button"
            data-testid="host-toggle"
            onClick={() => (props.view.connected() ? props.view.disconnect() : props.view.connect())}
          >
            {props.view.connected() ? "Disconnect" : "Connect"}
          </button>
        </Stack>
      </AnchorPanel>
    </Anchor>
  );
}
