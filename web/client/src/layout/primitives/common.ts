/**
 * What the layout primitives share (#2895, web/DESIGN.md "Layout").
 *
 * The primitives follow Every Layout (Pickering and Bell): each one owns a
 * single layout behaviour, takes its spacing from the 4 px scale, and
 * passes its per-instance values to `layout.css` as custom properties on
 * its own element. Only `src/layout/` may set `style` or use flex, grid,
 * gap, margins and `@media` (stylelint and ESLint fence the rest).
 *
 * @module
 */
import type { JSX } from "solid-js";

import type { SpaceStep } from "../../tokens";
import "./layout.css";

/** The elements a primitive may render as. */
export type LayoutTag = "div" | "section" | "article" | "aside" | "header" | "footer" | "main" | "nav" | "form" | "ul" | "ol" | "li";

/**
 * The props every primitive accepts: the element's own attributes except
 * `style`, which the primitive owns, plus the element to render as.
 */
export type LayoutProps = Omit<JSX.HTMLAttributes<HTMLElement>, "style"> & { readonly as?: LayoutTag };

/** A length a primitive passes through as a threshold or measure. */
export type Measure = `${number}${"px" | "rem" | "em" | "ch" | "%"}`;

/** The CSS value of a spacing step. */
export function space(step: SpaceStep): string {
  return `var(--space-${step})`;
}

/** The primitive's class followed by the caller's, if any. */
export function classes(own: string, extra: string | undefined): string {
  return extra === undefined ? own : `${own} ${extra}`;
}
