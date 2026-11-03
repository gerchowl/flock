import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import type { SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps, type Measure } from "./common";

export type SwitcherProps = LayoutProps & {
  /** The container width below which every child takes a full row. Default `"30rem"`. */
  readonly threshold?: Measure;
  /** The gap between children. Default `"4"` (16 px). */
  readonly space?: SpaceStep;
};

/**
 * Every Layout's Switcher: children share one row while the container is
 * wider than `threshold`, and stack below it, never in between.
 */
export function Switcher(props: SwitcherProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "threshold", "space"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-switcher", own.class)}
      style={{ "--l-space": space(own.space ?? "4"), "--l-threshold": own.threshold ?? "30rem" }}
    />
  );
}
