/** The platform's command modifier for a synthetic key event: ⌘ (Meta) on Apple platforms, Ctrl elsewhere, as `src/workbench/keys.ts` reads it. */
import { SHORTCUTS, isApple } from "../src/workbench/keys";

export const COMMAND: KeyboardEventInit = isApple() ? { metaKey: true } : { ctrlKey: true };

/** The keyboard layer's ⌘K commands, which follow the toolbar's in the palette. */
export const LAYER_COMMANDS: readonly string[] = SHORTCUTS.filter((entry) => entry.command).map((entry) => entry.label);
