/** Minimal structured launch sheet over the command outbox (#3095). */
import { For, Show, createMemo, createSignal, createUniqueId, onCleanup, onMount, type JSX } from "solid-js";

import type { FleetNodeRow } from "../gen/FleetNodeRow";
import type { FleetProjectRow } from "../gen/FleetProjectRow";
import { Cluster, Stack } from "../layout/primitives";
import { SPACE } from "../tokens";
import { STATUS_LABELS, type Sender } from "./Composer";
import { pushLayer } from "./keys";
import { RefusalDetails, explain } from "./refusals";
import { nodeLabel } from "./trees";
import "./sheet.css";

/**
 * Working directory is scoped to the selected node. Its first reported project
 * is the default, and the operator may choose another project path or type an
 * absolute path. Only the server validates whether the directory exists.
 */
export function NewAgent(props: {
  readonly sender: Sender;
  readonly nodes: readonly FleetNodeRow[];
  readonly projects: readonly FleetProjectRow[];
  readonly home: string | null;
  readonly close: () => void;
}): JSX.Element {
  let dialog: HTMLDialogElement | undefined;
  const title = createUniqueId();
  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const [name, setName] = createSignal("");
  const [node, setNode] = createSignal(props.home ?? "");
  const projects = createMemo(() => props.projects.filter(project => project.node_id === node()));
  const [directory, setDirectory] = createSignal<string | null>(null);
  const cwd = (): string => directory() ?? projects()[0]?.id ?? "";
  const [command, setCommand] = createSignal<string | null>(null);
  const [problem, setProblem] = createSignal<string | null>(null);
  const entry = createMemo(() => props.sender.commands().find(entry => entry.cmd_id === command()));
  const locked = (): boolean => command() !== null && (entry()?.status !== "failed" || entry()?.error?.code === "session_unconfirmed");
  const refusal = createMemo(() => {
    const current = entry();
    if (current?.status !== "failed") return null;
    const error = current.error ?? { code: "", message: "The server refused the start." };
    const lines: Readonly<Record<string, string>> = {
      unsupported_for_agent: "Agents cannot be started on another node yet. Choose the home node.",
      session_unconfirmed: "The server could not confirm whether the agent started. Check the agent list before starting another one.",
      invalid_params: "The agent could not be started with these settings. Check the name and working directory, then try again.",
      invalid_agent_name: "This agent name is not allowed. Choose another name.",
      agent_name_taken: "This agent name is already in use. Choose another name.",
      structured_needs_agent_user: "This server is not ready to start structured agents. Ask the operator to configure agent isolation.",
      structured_profiles_disabled: "Structured agent profiles are disabled on this server. Ask the operator to enable them.",
      structured_profiles_not_ready: "This server cannot run structured agent profiles yet. Ask the operator to check its setup.",
    };
    const line = Object.hasOwn(lines, error.code) ? lines[error.code] : undefined;
    return line === undefined ? explain(error) : { ...error, line };
  });
  // Only a definitive refusal permits another start; an unresolved outcome stays held.
  const submit = (event: SubmitEvent): void => {
    event.preventDefault();
    if (locked() || props.sender.blocked() !== null || !name().trim() || !node() || !cwd().startsWith("/")) return;
    try {
      const id = props.sender.start?.(name().trim(), node(), cwd());
      if (id !== undefined) setCommand(id);
    } catch {
      setProblem("The start could not be queued. Reconnect, then try again.");
    }
  };
  onMount(() => dialog?.showModal());
  onCleanup(() => { if (opener?.isConnected) opener.focus(); });
  const cycle = (by: 1 | -1): void => {
    const controls = [...(dialog?.querySelectorAll<HTMLElement>("button:not(:disabled), input:not(:disabled), select:not(:disabled)") ?? [])];
    const at = controls.findIndex(control => control === document.activeElement);
    controls[(at + by + controls.length) % controls.length]?.focus();
  };
  onCleanup(pushLayer({ "layer.close": () => dialog?.close(), "layer.next": () => cycle(1), "layer.previous": () => cycle(-1) }, { modal: true }));
  return (
    <dialog ref={dialog} class="sheet" aria-labelledby={title} onClose={props.close}>
      <Stack as="form" space={SPACE[4]} onSubmit={submit}>
        <h2 id={title} class="sheet__title">New agent</h2>
        <label>Name <input required value={name()} disabled={locked()} onInput={event => setName(event.currentTarget.value)} /></label>
        <label>Runtime <select aria-label="Runtime" disabled><option>Structured</option></select></label>
        <label>Harness <select aria-label="Harness" disabled><option>Claude Code</option></select></label>
        <label>Node <select aria-label="Node" required value={node()} disabled={locked()} onChange={event => { setNode(event.currentTarget.value); setDirectory(null); }}>
          <option value="" disabled>Choose a node</option>
          <For each={props.nodes}>{item => <option value={item.node_id}>{nodeLabel(props.nodes, item.node_id)}{item.node_id === props.home ? " (home)" : ""}</option>}</For>
        </select></label>
        <Show when={projects().length > 0}>
          <label>Project <select aria-label="Project" value={projects().some(project => project.id === cwd()) ? cwd() : ""} disabled={locked()} onChange={event => setDirectory(event.currentTarget.value)}>
            <option value="">Other directory…</option>
            <For each={projects()}>{project => <option value={project.id}>{project.id}</option>}</For>
          </select></label>
        </Show>
        <label>Working directory <input required value={cwd()} disabled={locked()} onInput={event => setDirectory(event.currentTarget.value)} /></label>
        <Show when={props.sender.blocked()}>{line => <p role="status">{line()}</p>}</Show>
        <Show when={problem()}>{line => <p role="alert">{line()}</p>}</Show>
        <Show when={entry()}>{current => <p role="status">{current().status === "acked" ? "Agent started." : refusal()?.line ?? STATUS_LABELS[current().status]}</p>}</Show>
        <Show when={refusal()}>{error => <RefusalDetails code={error().code} message={error().message} />}</Show>
        <Cluster justify="flex-end" space={SPACE[2]}>
          <button type="button" class="sheet__done" onClick={() => dialog?.close()}>Done</button>
          <button type="submit" class="sheet__done" disabled={locked() || props.sender.start === undefined || props.sender.blocked() !== null || !name().trim() || !node() || !cwd().startsWith("/")}>Start agent</button>
        </Cluster>
      </Stack>
    </dialog>
  );
}
