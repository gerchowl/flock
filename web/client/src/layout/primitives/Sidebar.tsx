import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import { SPACE, type SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps, type Measure } from "./common";

export type SidebarProps = LayoutProps & {
  /** Which child is the side: the first (`"left"`, default) or the last. */
  readonly side?: "left" | "right";
  /** The side's width while the two sit side by side. Default: its content's width. */
  readonly sideWidth?: Measure;
  /** The narrowest the content may get before the two stack. Default `"50%"`. */
  readonly contentMin?: Measure;
  /** The gap between side and content. Default `"4"` (16 px). */
  readonly space?: SpaceStep;
};

/**
 * Every Layout's Sidebar: exactly two children, a side of its own width
 * and content that takes the rest, stacking once the content would be
 * narrower than `contentMin`. No media query: it reacts to its container.
 */
export function Sidebar(props: SidebarProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "side", "sideWidth", "contentMin", "space"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-sidebar", own.class)}
      data-side={own.side ?? "left"}
      style={{
        "--l-space": space(own.space ?? SPACE[4]),
        "--l-side": own.sideWidth ?? "auto",
        "--l-content-min": own.contentMin ?? "50%",
      }}
    />
  );
}
