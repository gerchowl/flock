import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import type { SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps, type Measure } from "./common";

export type CenterProps = LayoutProps & {
  /** The column's maximum content width. Default `"60ch"`, a readable measure. */
  readonly max?: Measure;
  /** The gutter on either side, kept on narrow screens. Default `"4"` (16 px). */
  readonly gutters?: SpaceStep;
  /** Also centre each child within the column by its own width. */
  readonly intrinsic?: boolean;
};

/** Every Layout's Center: a horizontally centred column no wider than `max`. */
export function Center(props: CenterProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "max", "gutters", "intrinsic"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-center", own.class)}
      data-intrinsic={own.intrinsic === true ? "" : undefined}
      style={{ "--l-space": space(own.gutters ?? "4"), "--l-max": own.max ?? "60ch" }}
    />
  );
}
