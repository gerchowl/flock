/**
 * The workbench's keyboard layer (#2904): one registry of shortcuts
 * ({@link SHORTCUTS}), which the shortcut sheet (`?`) and the ⌘K palette
 * both read, and one document listener that dispatches every key the client
 * handles. Components never listen for keys themselves: they bind what a shortcut does on an element
 * ({@link bindKeys}), and the listener decides from the shortcut's
 * {@link Scope} and the focus whether it runs.
 *
 * A palette, sheet or popover pushes a {@link pushLayer} while it is open.
 * The top layer's keys (Esc, Tab, a list's arrows and Enter) win; a modal
 * layer is exclusive, so nothing behind it acts, even when focus is forced
 * behind it.
 *
 * The one listener carries {@link LAYER_BRAND}. In every browser test a
 * guard (`e2e/key-guard.ts`) records any other key listener, `onkey*`
 * property or JSX key handler, and the run fails: that guard is the
 * authority, ESLint's `design/keys` rule the early warning.
 *
 * Standard shortcuts keep their meaning. In a text field, ↑/↓, Esc, ⌘⌫ and
 * ⌃K edit text as the platform does; the composer's Esc (leave the field)
 * and ⌘↩ (send) are bound on the composer itself. ⌘ is the platform's
 * command key: ⌘ on Apple platforms, Ctrl elsewhere, so ⌃K on a Mac stays
 * the text field's "delete to the end of the line". Nothing fires while an
 * input method is composing (#2967): its Enter commits the composition.
 *
 * Labels list modifiers in ⌃⌥⇧⌘ order (HIG Keyboards).
 *
 * @module
 */

/** A key and its modifiers. */
export type Combo = {
  /** `KeyboardEvent.key`; a letter compares case-insensitively. */
  readonly key: string;
  /** The platform's command key: ⌘ on Apple platforms, Ctrl elsewhere. */
  readonly mod?: true;
  /** The Control key itself (⌃ on Apple platforms). */
  readonly ctrl?: true;
  readonly alt?: true;
  readonly shift?: true;
};

/**
 * Where a shortcut acts:
 * - `anywhere`: from any focus, text fields included (⌘K, web/DESIGN.md rule 8).
 * - `outside-text`: from any focus but a text field.
 * - `within`: when focus is inside an element bound for it, or on nothing
 *   (then the page's binding, on `document.body`); never in a text field.
 * - `focused`: only on the bound element itself, when it has the focus.
 * - `layer`: only in an open layer that binds it ({@link pushLayer}).
 */
export type Scope = "anywhere" | "outside-text" | "within" | "focused" | "layer";

/** Every shortcut's id. */
export type ShortcutId =
  | "agent.previous"
  | "agent.next"
  | "palette.open"
  | "sheet.open"
  | "composer.send"
  | "composer.leave"
  | "card.allow"
  | "card.deny"
  | "layer.close"
  | "layer.next"
  | "layer.previous"
  | "list.next"
  | "list.previous"
  | "list.choose"
  | "listbox.next"
  | "listbox.previous";

/**
 * One shortcut: what it does, its keys, and where it acts. A `command` is
 * also a ⌘K palette command; an `unlisted` one (Tab, a list's own keys)
 * is left off the shortcut sheet.
 */
export type Shortcut = { readonly id: ShortcutId; readonly label: string; readonly combo: Combo; readonly scope: Scope; readonly command?: true; readonly unlisted?: true };

/** The registry, in the order the shortcut sheet lists it. */
export const SHORTCUTS: readonly Shortcut[] = [
  // A focused list widget's own arrows; before the page's ↑/↓, so the focused list wins.
  { id: "listbox.next", label: "Next item in a list", combo: { key: "ArrowDown" }, scope: "focused", unlisted: true },
  { id: "listbox.previous", label: "Previous item in a list", combo: { key: "ArrowUp" }, scope: "focused", unlisted: true },
  { id: "palette.open", label: "Search or run a command", combo: { key: "k", mod: true }, scope: "anywhere" },
  { id: "agent.previous", label: "Previous agent", combo: { key: "ArrowUp" }, scope: "within", command: true },
  { id: "agent.next", label: "Next agent", combo: { key: "ArrowDown" }, scope: "within", command: true },
  { id: "card.allow", label: "Allow once, on a focused request", combo: { key: "Enter", mod: true }, scope: "focused" },
  { id: "card.deny", label: "Deny, on a focused request", combo: { key: "Backspace", mod: true }, scope: "focused" },
  { id: "composer.send", label: "Send the message", combo: { key: "Enter", mod: true }, scope: "focused" },
  { id: "composer.leave", label: "Leave the message field", combo: { key: "Escape" }, scope: "focused" },
  { id: "sheet.open", label: "Keyboard shortcuts", combo: { key: "?" }, scope: "outside-text", command: true },
  { id: "layer.close", label: "Close the palette, this sheet or a popover", combo: { key: "Escape" }, scope: "layer" },
  { id: "layer.next", label: "Next control in a dialog", combo: { key: "Tab" }, scope: "layer", unlisted: true },
  { id: "layer.previous", label: "Previous control in a dialog", combo: { key: "Tab", shift: true }, scope: "layer", unlisted: true },
  { id: "list.next", label: "Next item", combo: { key: "ArrowDown" }, scope: "layer", unlisted: true },
  { id: "list.previous", label: "Previous item", combo: { key: "ArrowUp" }, scope: "layer", unlisted: true },
  { id: "list.choose", label: "Choose the item", combo: { key: "Enter" }, scope: "layer", unlisted: true },
];

/** The registry entry for `id`. */
export function shortcut(id: ShortcutId): Shortcut {
  const found = SHORTCUTS.find((each) => each.id === id);
  if (found === undefined) throw new Error(`no shortcut ${id}`);
  return found;
}

/** Whether `platform` (`navigator.platform`, else the user agent) is Apple's, where ⌘ is the command key. */
export function isApple(platform: string = navigator.platform || navigator.userAgent): boolean {
  return /Mac|iPhone|iPad|iPod/.test(platform);
}

const GLYPHS: Readonly<Record<string, readonly [apple: string, other: string]>> = {
  Enter: ["↩", "Enter"],
  Backspace: ["⌫", "Backspace"],
  Escape: ["Esc", "Esc"],
  ArrowUp: ["↑", "↑"],
  ArrowDown: ["↓", "↓"],
};

/** The combo as the platform writes it, modifiers in ⌃⌥⇧⌘ order: `⌃⌥⇧⌘K` on Apple platforms, `Ctrl+Alt+Shift+K` elsewhere. */
export function formatCombo(combo: Combo, apple: boolean = isApple()): string {
  const key = GLYPHS[combo.key]?.[apple ? 0 : 1] ?? combo.key.toUpperCase();
  if (apple) return [combo.ctrl && "⌃", combo.alt && "⌥", combo.shift && "⇧", combo.mod && "⌘", key].filter(Boolean).join("");
  return [(combo.ctrl || combo.mod) && "Ctrl", combo.alt && "Alt", combo.shift && "Shift", key].filter(Boolean).join("+");
}

/** The combo in `aria-keyshortcuts` syntax. */
export function ariaKeys(combo: Combo, apple: boolean = isApple()): string {
  const control = combo.ctrl || (combo.mod && !apple);
  return [control && "Control", combo.alt && "Alt", combo.shift && "Shift", combo.mod && apple && "Meta", combo.key].filter(Boolean).join("+");
}

/** Whether `event` is `combo` on this platform. A symbol such as `?` is typed with Shift, so Shift is compared only for letters and named keys. */
export function matches(event: KeyboardEvent, combo: Combo, apple: boolean = isApple()): boolean {
  const control = apple ? combo.ctrl === true : combo.ctrl === true || combo.mod === true;
  const meta = apple && combo.mod === true;
  if (event.ctrlKey !== control || event.metaKey !== meta || event.altKey !== (combo.alt === true)) return false;
  if ((combo.key.length > 1 || /^[a-z]$/i.test(combo.key)) && event.shiftKey !== (combo.shift === true)) return false;
  return event.key.toLowerCase() === combo.key.toLowerCase();
}

/** Whether `event` belongs to an input method's composition (#2967): `isComposing`, or Safari's keyCode 229 for the key that ends one. */
export function composing(event: KeyboardEvent): boolean {
  return event.isComposing || event.keyCode === 229;
}

/** Whether `element` edits text, where the keys keep their editing meaning. */
export function editsText(element: Element | null): boolean {
  if (element === null) return false;
  if (element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement || (element instanceof HTMLElement && element.isContentEditable)) return true;
  return element instanceof HTMLInputElement && !["button", "checkbox", "radio", "range", "color", "file", "submit", "reset", "image"].includes(element.type);
}

type Handlers = Map<ShortcutId, () => void>;

const bindings = new WeakMap<Element, Handlers>();
/** An open palette, sheet or popover: its keys, and whether it is modal (exclusive). */
type Layer = { readonly handlers: Handlers; readonly modal: boolean };
const layers: Layer[] = [];
let bound = 0;

function retain(): void {
  if (bound++ === 0) document.addEventListener("keydown", onKey);
}

function release(): void {
  if (--bound === 0) document.removeEventListener("keydown", onKey);
}

/** The handlers in `handlers` that name a shortcut, as entries. */
function entriesOf(handlers: Partial<Record<ShortcutId, () => void>>): (readonly [ShortcutId, () => void])[] {
  return SHORTCUTS.flatMap(({ id }) => {
    const run = handlers[id];
    return run === undefined ? [] : [[id, run] as const];
  });
}

/** The handler `element` has bound for `id`, if any. */
function handlerOf(element: Element | null, id: ShortcutId): (() => void) | undefined {
  return element === null ? undefined : bindings.get(element)?.get(id);
}

/** The handler that runs for `entry` with focus on `target`, if any. */
function resolve(entry: Shortcut, target: Element | null): (() => void) | undefined {
  const page = handlerOf(document.body, entry.id);
  if (entry.scope === "layer") return undefined;
  if (entry.scope === "focused") return handlerOf(target, entry.id);
  if (entry.scope === "anywhere") return page;
  if (editsText(target)) return undefined;
  if (entry.scope === "outside-text") return page;
  if (target === null || target === document.body) return page;
  for (let at: Element | null = target; at !== null && at !== document.body; at = at.parentElement) {
    const found = handlerOf(at, entry.id);
    if (found !== undefined) return found;
  }
  return undefined;
}

/** The brand on the keyboard layer's one listener, which the browser tests' key guard accepts. */
export const LAYER_BRAND: unique symbol = Symbol.for("flock.keyboard-layer");

/** The one composition guard (#2967): no key the client handles acts while an input method is composing. */
function onKey(event: KeyboardEvent): void {
  if (event.defaultPrevented || composing(event)) return;
  const run = (handler: () => void): void => {
    event.preventDefault();
    handler();
  };
  // Open layers first, top down; a modal one ends the search, so the page behind it is inert.
  for (const layer of [...layers].reverse()) {
    const entry = SHORTCUTS.find((each) => each.scope === "layer" && layer.handlers.has(each.id) && matches(event, each.combo));
    const handler = entry === undefined ? undefined : layer.handlers.get(entry.id);
    if (handler !== undefined) return run(handler);
    if (layer.modal) return;
  }
  const target = event.target instanceof Element ? event.target : null;
  for (const entry of SHORTCUTS) {
    if (!matches(event, entry.combo)) continue;
    const handler = resolve(entry, target);
    if (handler !== undefined) return run(handler);
  }
}

Object.defineProperty(onKey, LAYER_BRAND, { value: true });

/**
 * Binds what shortcuts do on `element` (`document.body` for the page's own);
 * returns the unbinding. The document listener is installed while anything
 * is bound.
 */
export function bindKeys(element: Element, handlers: Partial<Record<ShortcutId, () => void>>): () => void {
  const own: Handlers = bindings.get(element) ?? new Map();
  bindings.set(element, own);
  const entries = entriesOf(handlers);
  for (const [id, run] of entries) own.set(id, run);
  retain();
  return () => {
    for (const [id, run] of entries) if (own.get(id) === run) own.delete(id);
    release();
  };
}

/**
 * Opens a layer with its own `layer` keys, above the others; returns its
 * closing, which is safe to call twice. A `modal` layer makes every key
 * binding behind it inert while it is open.
 */
export function pushLayer(handlers: Partial<Record<ShortcutId, () => void>>, options: { readonly modal: boolean }): () => void {
  const layer: Layer = { handlers: new Map(entriesOf(handlers)), modal: options.modal };
  layers.push(layer);
  retain();
  return () => {
    const at = layers.indexOf(layer);
    if (at < 0) return;
    layers.splice(at, 1);
    release();
  };
}

/** Runs the page's binding for `id`, as a ⌘K command does; false when nothing is bound. */
export function runPageShortcut(id: ShortcutId): boolean {
  const run = handlerOf(document.body, id);
  run?.();
  return run !== undefined;
}
