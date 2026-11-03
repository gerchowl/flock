import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import type { SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps, type Measure } from "./common";

export type GridProps = LayoutProps & {
  /** The narrowest a column may get; as many as fit share the row. Default `"16rem"`. */
  readonly min?: Measure;
  /** The gap between cells. Default `"4"` (16 px). */
  readonly space?: SpaceStep;
};

/** Every Layout's Grid: equal columns, as many of at least `min` as fit, one on narrow screens. */
export function Grid(props: GridProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "min", "space"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-grid", own.class)}
      style={{ "--l-space": space(own.space ?? "4"), "--l-min": own.min ?? "16rem" }}
    />
  );
}
