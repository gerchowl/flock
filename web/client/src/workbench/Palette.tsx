/**
 * The command palette (ADR-0038 §2a): ⌘K or Ctrl+K, or the top bar's search
 * field. It is the only way to open a subject by name, and it runs
 * commands: the toolbar's, and every shortcut the keyboard layer marks as
 * one (`keys.ts`, #2904). The MVP searches agents only; issues and PRs wait
 * for a PR/issue source.
 *
 * fleet-lite lists structured agents only (S7-min, #1746), so a query that
 * matches no row is offered as an agent id to open.
 *
 * @module
 */
import { For, Show, createMemo, createSignal, createUniqueId, onCleanup, type JSX } from "solid-js";

import { Icon } from "../components/Icon";
import type { AgentRow } from "../fleet";
import { bindKeys, formatCombo, isApple, pushLayer, shortcut } from "./keys";

/** Props of {@link Palette}. */
export type PaletteProps = {
  readonly rows: () => readonly AgentRow[];
  readonly onOpen: (agentId: string) => void;
  /**
   * The toolbars' actions that wait for a later slice: listed after the
   * agents, dimmed, so every toolbar action has its command (HIG: a menu
   * shows an unavailable command disabled rather than hiding it).
   */
  readonly waiting?: readonly WaitingAction[];
  /** Commands that run now, such as the toolbar's connection actions; listed after the agents. */
  readonly commands?: readonly PaletteCommand[];
};

/** A toolbar action that is shown but not yet available, and what it waits for. */
export type WaitingAction = { readonly label: string; readonly waitsFor: string };

/** A command the palette runs when chosen. */
export type PaletteCommand = { readonly label: string; readonly hint: string; readonly run: () => void; readonly available?: () => boolean };

type Item = { readonly id: string; readonly label: string; readonly hint: string; readonly disabled?: true; readonly run?: () => void };

/** The shortcut as this platform writes it: ⌘K on Apple devices, Ctrl+K elsewhere. */
export function shortcutLabel(platform: string): string {
  return formatCombo(shortcut("palette.open").combo, isApple(platform));
}

/** The top bar's search field and the palette it opens. */
export function Palette(props: PaletteProps): JSX.Element {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [selected, setSelected] = createSignal(0);
  let input: HTMLInputElement | undefined;
  let trigger: HTMLButtonElement | undefined;
  // Where focus was when the palette opened: Esc returns it there (HIG: a dismissed popover restores focus).
  let opener: HTMLElement | null = null;
  const listId = createUniqueId();
  const optionId = (index: number): string => `${listId}-${index}`;
  const items = createMemo<Item[]>(() => {
    const text = query().trim().toLowerCase();
    const matches = props.rows()
      .filter((row) => text === "" || row.label.toLowerCase().includes(text) || row.id.toLowerCase().includes(text))
      .map((row) => ({ id: row.id, label: row.label, hint: row.activity }));
    const commands = (props.commands ?? [])
      .filter((command) => text === "" || command.label.toLowerCase().includes(text))
      .map((command): Item => ({ id: `run:${command.label}`, label: command.label, hint: command.hint, run: command.run, ...(command.available?.() === false ? { disabled: true as const } : {}) }));
    const waiting = (props.waiting ?? [])
      .filter((action) => text === "" || action.label.toLowerCase().includes(text))
      .map((action): Item => ({ id: `waits:${action.label}`, label: action.label, hint: `Waits for ${action.waitsFor}`, disabled: true }));
    if (matches.length === 0 && /^[\w.-]+$/.test(query().trim())) {
      return [{ id: query().trim(), label: `Open agent ${query().trim()}`, hint: "by id" }, ...commands, ...waiting];
    }
    return [...matches, ...commands, ...waiting];
  });
  /** The palette's modal key layer while it is open: Esc, Tab, and the list's arrows and Enter (`keys.ts`). */
  let closeLayer: (() => void) | undefined;
  const step = (by: 1 | -1): void => {
    const count = items().length;
    if (count > 0) setSelected((selected() + by + count) % count);
  };
  const show = (): void => {
    if (!open()) {
      opener = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
      // The search field is the dialog's only focusable control: Tab and Shift+Tab stay on it.
      const stay = (): void => input?.focus();
      closeLayer = pushLayer(
        { "layer.close": () => hide(), "layer.next": stay, "layer.previous": stay, "list.next": () => step(1), "list.previous": () => step(-1), "list.choose": () => choose(items()[selected()]) },
        { modal: true },
      );
    }
    setOpen(true);
    setSelected(0);
    queueMicrotask(() => input?.focus());
  };
  /** Closes the palette; a dismissal returns focus where it was, a choice to the trigger. */
  const hide = (dismissed = true): void => {
    closeLayer?.();
    closeLayer = undefined;
    setOpen(false);
    setQuery("");
    (dismissed && opener?.isConnected ? opener : trigger)?.focus();
  };
  const choose = (item: Item | undefined): void => {
    if (item === undefined || item.disabled) return;
    // A command runs after the palette closed, so it may move focus itself.
    if (item.run !== undefined) {
      hide(false);
      item.run();
      return;
    }
    props.onOpen(item.id);
    hide(false);
  };
  onCleanup(bindKeys(document.body, { "palette.open": show }));
  onCleanup(() => closeLayer?.());
  return (
    <>
      <button ref={trigger} type="button" class="palette-trigger" onClick={show} data-testid="palette-trigger">
        <Icon name="search" size={14} /> Search or run… <kbd>{shortcutLabel(navigator.platform || navigator.userAgent)}</kbd>
      </button>
      <Show when={open()}>
        <div class="palette-backdrop" onClick={(event) => event.target === event.currentTarget && hide()}>
          <div
            class="palette"
            role="dialog"
            aria-modal="true"
            aria-label="Command palette"
            data-testid="palette"
          >
            <input
              ref={input}
              class="palette__input"
              type="search"
              role="combobox"
              aria-label="Search agents or run a command"
              aria-expanded="true"
              aria-controls={listId}
              aria-activedescendant={items().length > 0 ? optionId(selected()) : undefined}
              value={query()}
              onInput={(event) => {
                setQuery(event.currentTarget.value);
                setSelected(0);
              }}
            />
            <ul id={listId} class="palette__list" role="listbox">
              <For each={items()}>
                {(item, index) => (
                  <li
                    id={optionId(index())}
                    role="option"
                    class="palette__item"
                    aria-selected={index() === selected()}
                    aria-disabled={item.disabled}
                    onClick={() => choose(item)}
                    data-testid={item.disabled || item.run !== undefined ? "palette-command" : "palette-item"}
                  >
                    <span>{item.label}</span>
                    <span class="palette__hint">{item.hint}</span>
                  </li>
                )}
              </For>
            </ul>
          </div>
        </div>
      </Show>
    </>
  );
}
