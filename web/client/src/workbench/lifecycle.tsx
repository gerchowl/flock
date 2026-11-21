/** Structured stop and resume over the command outbox (#3096). */
import { Show, createEffect, createSignal, type JSX } from "solid-js";

import type { AgentRow } from "../fleet";
import { Stack } from "../layout/primitives";
import { SPACE } from "../tokens";
import { STATUS_LABELS, type Sender } from "./Composer";
import type { PaletteCommand } from "./Palette";
import { RefusalDetails, explain } from "./refusals";

/** One controller survives view switches; confirmations bind to the displayed agent. */
export function createLifecycle(sender: () => Sender | null, row: () => AgentRow | undefined): {
  readonly commands: () => PaletteCommand[];
  readonly view: () => JSX.Element;
} {
  const [confirming, setConfirming] = createSignal<AgentRow | null>(null);
  const [problem, setProblem] = createSignal<string | null>(null);
  const available = (): boolean => sender()?.restart !== undefined && sender()?.stop !== undefined && row() !== undefined;
  const entry = () => sender()?.commands().filter(entry => {
    const args = entry.args;
    return (entry.verb === "agent.restart" || entry.verb === "agent.stop") && typeof args === "object" && args !== null && !Array.isArray(args) && args["target"] === row()?.id;
  }).at(-1);
  const held = (): boolean => {
    const current = entry();
    return current !== undefined && current.status !== "acked" && (current.status !== "failed" || current.error?.code === "session_unconfirmed");
  };
  const blocked = (): string | null => sender()?.blocked() ?? (held() ? "Waiting for the server to confirm this action. Check the agent's state before trying again." : null);
  const canRestart = (): boolean => available() && blocked() === null && row()?.state === "offline";
  const canStop = (): boolean => available() && blocked() === null && row()?.state !== "offline" && row()?.state !== "gone";
  const send = (verb: "restart" | "stop", target: AgentRow): void => {
    if (target.id !== row()?.id || !(verb === "restart" ? canRestart() : canStop())) return;
    setProblem(null);
    try { sender()?.[verb]?.(target.id); }
    catch { setProblem("The action could not be queued. Reconnect, then try again."); }
    setConfirming(null);
  };
  const restart = (): void => { const target = row(); if (target) send("restart", target); };
  let stopButton: HTMLButtonElement | undefined;
  let cancelButton: HTMLButtonElement | undefined;
  const askStop = (): void => {
    if (!canStop()) return;
    setConfirming(row() ?? null);
    cancelButton?.focus();
  };
  const cancel = (): void => { setConfirming(null); stopButton?.focus(); };
  let previousAgent: string | undefined;
  createEffect(() => {
    const current = row();
    if (current?.id !== previousAgent) {
      previousAgent = current?.id;
      setConfirming(null);
      setProblem(null);
    }
    if (current?.state === "offline" || current?.state === "gone") setConfirming(null);
  });
  const refusal = () => {
    const current = entry();
    if (current?.status !== "failed") return null;
    const error = current.error ?? { code: "", message: "The server refused this action." };
    const lines: Readonly<Record<string, string>> = {
      operator_only: "Stopping or restarting requires the operator's app on the agent's home node. Open the app there.",
      unsupported_for_agent: "This action is only available on the agent's home node. Connect there to continue.",
      runtime_unsupported: "This connection cannot restart this agent. Use the operator's app on its home node.",
      session_busy: "This agent is still running or finishing cleanup. Stop it and wait until it is offline before restarting.",
      session_unconfirmed: "The server could not confirm this action. Check the agent's state before trying again.",
      structured_recovery_required: "This agent's process ownership is uncertain. Ask the operator to recover it before trying again.",
      structured_profile_mismatch: "This agent belongs to a different profile. Restore its original profile before restarting.",
      structured_needs_agent_user: "This server is not ready to run structured agents. Ask the operator to configure agent isolation.",
      structured_endpoint_restart_required: "This server's agent connection needs to be refreshed. Ask the operator to restart the server.",
      structured_profiles_disabled: "Structured agent profiles are disabled. Ask the operator to enable them.",
      structured_profiles_not_ready: "This server cannot run structured agent profiles yet. Ask the operator to check its setup.",
      invalid_params: "The server could not accept this action. Reconnect with an updated app, then try again.",
    };
    const line = Object.hasOwn(lines, error.code) ? lines[error.code] : undefined;
    return line === undefined ? explain(error) : { ...error, line };
  };
  const status = (): string | null => {
    if (problem() !== null) return problem();
    if (refusal() !== null) return refusal()!.line;
    const current = entry();
    if (current?.status === "acked") return current.verb === "agent.stop" ? (row()?.state === "offline" ? "Agent stopped." : "Stop requested; waiting for the agent to go offline.") : "Agent restarted.";
    return blocked() ?? (current ? STATUS_LABELS[current.status] : null);
  };
  return {
    commands: () => available() ? [
      { label: "Restart agent", hint: row()?.label ?? "", available: canRestart, run: restart },
      { label: "Stop agent", hint: row()?.label ?? "", available: canStop, run: askStop },
    ] : [],
    view: () => <Show when={available()}>
      <Stack space={SPACE[2]}>
        <Show when={confirming()} fallback={<div class="card__choices">
          <button type="button" class="action" disabled={!canRestart()} title={row()?.state === "offline" ? "Resume the same session" : "Stop the agent before restarting"} onClick={restart}>Restart agent</button>
          <button ref={stopButton} type="button" class="action" disabled={!canStop()} onClick={askStop}>Stop agent</button>
        </div>}>{target => <div class="decide__confirm" role="group" aria-label="Confirm stop agent">
          <p>Stop {target().label}? You can restart this session later.</p>
          <div class="card__choices">
            <button type="button" class="action action--primary" disabled={!canStop()} onClick={() => send("stop", target())}>Confirm stop</button>
            <button ref={cancelButton} type="button" class="action" onClick={cancel}>Cancel</button>
          </div>
        </div>}</Show>
        <Show when={entry() === undefined && row()?.state !== "offline"}><p class="waits__reason">Stop the agent before restarting.</p></Show>
        <Show when={status()}>{line => <p role="status">{line()}</p>}</Show>
        <Show when={refusal()}>{error => <RefusalDetails code={error().code} message={error().message} />}</Show>
      </Stack>
    </Show>,
  };
}
