import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import type { SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps } from "./common";

export type StackProps = LayoutProps & {
  /** The gap between children, a step of the spacing scale. Default `"4"` (16 px). */
  readonly space?: SpaceStep;
};

/** Every Layout's Stack: children in a column, `space` apart. */
export function Stack(props: StackProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "space"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-stack", own.class)}
      style={{ "--l-space": space(own.space ?? "4") }}
    />
  );
}
