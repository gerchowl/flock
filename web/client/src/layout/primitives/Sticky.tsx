import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import { classes, type LayoutProps } from "./common";

export type StickyProps = LayoutProps;

/** Keep navigation or header chrome at the top of its scrolling container. */
export function Sticky(props: StickyProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class"]);
  return <Dynamic component={own.as ?? "div"} {...rest} class={classes("l-sticky", own.class)} />;
}
