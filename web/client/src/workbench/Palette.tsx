/**
 * The command palette (ADR-0038 §2a): ⌘K or Ctrl+K, or the top bar's search
 * field. It is the only way to open a subject by name. The MVP searches
 * agents only; issues and PRs wait for a PR/issue source.
 *
 * fleet-lite lists structured agents only (S7-min, #1746), so a query that
 * matches no row is offered as an agent id to open.
 *
 * @module
 */
import { For, Show, createMemo, createSignal, createUniqueId, onCleanup, type JSX } from "solid-js";

import type { AgentRow } from "../fleet";

/** Props of {@link Palette}. */
export type PaletteProps = {
  readonly rows: () => readonly AgentRow[];
  readonly onOpen: (agentId: string) => void;
};

type Item = { readonly id: string; readonly label: string; readonly hint: string };

/** The shortcut as this platform writes it: ⌘K on Apple devices, Ctrl K elsewhere. */
export function shortcutLabel(platform: string): string {
  return /Mac|iPhone|iPad|iPod/.test(platform) ? "⌘K" : "Ctrl K";
}

/** The top bar's search field and the palette it opens. */
export function Palette(props: PaletteProps): JSX.Element {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  const [selected, setSelected] = createSignal(0);
  let input: HTMLInputElement | undefined;
  let trigger: HTMLButtonElement | undefined;
  const listId = createUniqueId();
  const optionId = (index: number): string => `${listId}-${index}`;
  const items = createMemo<Item[]>(() => {
    const text = query().trim().toLowerCase();
    const matches = props.rows()
      .filter((row) => text === "" || row.label.toLowerCase().includes(text) || row.id.toLowerCase().includes(text))
      .map((row) => ({ id: row.id, label: row.label, hint: row.activity }));
    if (matches.length === 0 && /^[\w.-]+$/.test(query().trim())) {
      return [{ id: query().trim(), label: `Open agent ${query().trim()}`, hint: "by id" }];
    }
    return matches;
  });
  const show = (): void => {
    setOpen(true);
    setSelected(0);
    queueMicrotask(() => input?.focus());
  };
  const hide = (): void => {
    setOpen(false);
    setQuery("");
    trigger?.focus();
  };
  const choose = (item: Item | undefined): void => {
    if (item === undefined) return;
    props.onOpen(item.id);
    hide();
  };
  const onGlobalKey = (event: KeyboardEvent): void => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      show();
    }
  };
  document.addEventListener("keydown", onGlobalKey);
  onCleanup(() => document.removeEventListener("keydown", onGlobalKey));
  const onKey = (event: KeyboardEvent): void => {
    const count = items().length;
    if (event.key === "Escape") hide();
    else if (event.key === "ArrowDown" && count > 0) setSelected((selected() + 1) % count);
    else if (event.key === "ArrowUp" && count > 0) setSelected((selected() + count - 1) % count);
    else if (event.key === "Enter") choose(items()[selected()]);
    else return;
    event.preventDefault();
  };
  return (
    <>
      <button ref={trigger} type="button" class="palette-trigger" onClick={show} data-testid="palette-trigger">
        Search agents… <kbd>{shortcutLabel(navigator.platform || navigator.userAgent)}</kbd>
      </button>
      <Show when={open()}>
        <div class="palette-backdrop" onClick={(event) => event.target === event.currentTarget && hide()}>
          <div
            class="palette"
            role="dialog"
            aria-modal="true"
            aria-label="Command palette"
            data-testid="palette"
            onKeyDown={(event) => {
              // The search field is the dialog's only focusable control: Tab and Shift+Tab stay on it.
              if (event.key !== "Tab") return;
              event.preventDefault();
              input?.focus();
            }}
          >
            <input
              ref={input}
              class="palette__input"
              type="search"
              role="combobox"
              aria-label="Search agents"
              aria-expanded="true"
              aria-controls={listId}
              aria-activedescendant={items().length > 0 ? optionId(selected()) : undefined}
              value={query()}
              onInput={(event) => {
                setQuery(event.currentTarget.value);
                setSelected(0);
              }}
              onKeyDown={onKey}
            />
            <ul id={listId} class="palette__list" role="listbox">
              <For each={items()}>
                {(item, index) => (
                  <li
                    id={optionId(index())}
                    role="option"
                    class="palette__item"
                    aria-selected={index() === selected()}
                    onClick={() => choose(item)}
                    data-testid="palette-item"
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
