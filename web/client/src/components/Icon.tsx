/**
 * The client's one icon set (#2903): SF-Symbols-style line glyphs drawn
 * inline on a 16-unit grid, stroked in the text colour so they follow the
 * colour mode and the selection like text does. An icon is decoration: it
 * is always `aria-hidden`, and the control around it carries the name.
 * {@link IconButton} is the only icon-only control, and its `label` is
 * required, so an icon button without an accessible name does not type
 * check (the conformance suite's keyboard check enforces it on the page).
 *
 * A new glyph goes in {@link ICON_NAMES} and `GLYPHS`, drawn with round caps and joins at
 * stroke width 1.5 on the 16-unit grid, like the others.
 *
 * @module
 */
import { splitProps, type JSX } from "solid-js";

import "./icon.css";

/** Every glyph's name, in the gallery's order. */
export const ICON_NAMES = ["send", "chevron-down", "chevron-right", "chevron-left", "search"] as const;

/** The name of a glyph in the set. */
export type IconName = (typeof ICON_NAMES)[number];

/** Each glyph's strokes, as factories: one DOM node cannot render in two places. */
const GLYPHS: { readonly [N in IconName]: () => JSX.Element } = {
  /** `arrow.up`: send a message. */
  send: () => <path d="M8 13V3.5M4 7.5 8 3.5l4 4" />,
  /** `chevron.down`: a menu or popover opens below. */
  "chevron-down": () => <path d="M4.5 6.5 8 10l3.5-3.5" />,
  /** `chevron.right`: a collapsed group. */
  "chevron-right": () => <path d="M6.5 4.5 10 8l-3.5 3.5" />,
  /** `chevron.left`: back. */
  "chevron-left": () => <path d="M9.5 4.5 6 8l3.5 3.5" />,
  /** `magnifyingglass`: search. */
  search: () => (
    <>
      <circle cx="7" cy="7" r="4.25" />
      <path d="M10.25 10.25 13.5 13.5" />
    </>
  ),
};

/** One glyph, `size` px square (default 16), hidden from assistive technology. */
export function Icon(props: { readonly name: IconName; readonly size?: 12 | 14 | 16 }): JSX.Element {
  return (
    <svg
      class="icon"
      viewBox="0 0 16 16"
      width={props.size ?? 16}
      height={props.size ?? 16}
      aria-hidden="true"
      stroke-width="1.5"
      stroke-linecap="round"
      stroke-linejoin="round"
      data-icon={props.name}
    >
      {GLYPHS[props.name]()}
    </svg>
  );
}

/** Props of {@link IconButton}: a button's own attributes, its glyph and its required name. */
export type IconButtonProps = Omit<JSX.ButtonHTMLAttributes<HTMLButtonElement>, "aria-label" | "children" | "style"> & {
  readonly icon: IconName;
  /** The accessible name, also shown as the tooltip. Shortcuts go in `title` separately when there is one. */
  readonly label: string;
};

/** A button that shows only a glyph; `label` is its accessible name. */
export function IconButton(props: IconButtonProps): JSX.Element {
  const [own, rest] = splitProps(props, ["icon", "label", "class", "title", "type"]);
  return (
    <button {...rest} type={own.type ?? "button"} class={own.class === undefined ? "icon-button" : `icon-button ${own.class}`} aria-label={own.label} title={own.title ?? own.label}>
      <Icon name={own.icon} />
    </button>
  );
}
