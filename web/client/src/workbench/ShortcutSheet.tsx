/**
 * The shortcut sheet (#2904): `?`, or the palette's *Keyboard shortcuts*,
 * opens a modal sheet listing every shortcut of the keyboard layer's
 * registry (`keys.ts`), so the sheet and the layer cannot disagree. It is a
 * native modal `<dialog>`: the page behind it is inert, Esc closes it, Tab
 * stays inside it, and closing returns focus to where it was.
 *
 * @module
 */
import { For, createUniqueId, onCleanup, type JSX } from "solid-js";

import { Cluster, Stack } from "../layout/primitives";
import { SPACE } from "../tokens";
import { SHORTCUTS, bindKeys, formatCombo } from "./keys";
import "./sheet.css";

/** Every shortcut, its label and its keys; the sheet's body, and the gallery's. */
export function ShortcutList(): JSX.Element {
  return (
    <Stack as="ul" space={SPACE[1]} class="shortcuts">
      <For each={SHORTCUTS}>
        {(entry) => (
          <Cluster as="li" justify="space-between" space={SPACE[6]}>
            <span>{entry.label}</span>
            <kbd class="shortcuts__keys">{formatCombo(entry.combo)}</kbd>
          </Cluster>
        )}
      </For>
    </Stack>
  );
}

/** The sheet; `?` opens it while it is mounted. */
export function ShortcutSheet(): JSX.Element {
  let dialog: HTMLDialogElement | undefined;
  let opener: HTMLElement | null = null;
  const title = createUniqueId();
  const open = (): void => {
    if (dialog === undefined || dialog.open) return;
    opener = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
    dialog.showModal();
  };
  onCleanup(bindKeys(document.body, { "sheet.open": open }));
  // Tab and Shift+Tab cycle through the sheet's controls, never out to the page or the browser.
  const trap = (event: KeyboardEvent): void => {
    if (event.key !== "Tab" || dialog === undefined) return;
    const controls = [...dialog.querySelectorAll<HTMLElement>("button:not(:disabled), [href], input, [tabindex]:not([tabindex='-1'])")];
    if (controls.length === 0) return;
    const at = controls.findIndex((control) => control === document.activeElement);
    event.preventDefault();
    controls[(at + (event.shiftKey ? -1 : 1) + controls.length) % controls.length]?.focus();
  };
  return (
    <dialog ref={dialog} class="sheet" aria-labelledby={title} data-testid="shortcut-sheet" onKeyDown={trap} onClose={() => opener?.isConnected && opener.focus()}>
      <Stack space={SPACE[4]}>
        <h2 id={title} class="sheet__title">
          Keyboard shortcuts
        </h2>
        <ShortcutList />
        <Cluster justify="flex-end" space={SPACE[2]}>
          <button type="button" class="sheet__done" onClick={() => dialog?.close()}>
            Done
          </button>
        </Cluster>
      </Stack>
    </dialog>
  );
}
