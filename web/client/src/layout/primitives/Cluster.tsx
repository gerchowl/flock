import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import type { SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps } from "./common";

export type ClusterProps = LayoutProps & {
  /** The gap between children, across and between rows. Default `"2"` (8 px). */
  readonly space?: SpaceStep;
  /** Horizontal distribution of each row. Default `"flex-start"`. */
  readonly justify?: "flex-start" | "center" | "flex-end" | "space-between";
  /** Vertical alignment within a row. Default `"center"`. */
  readonly align?: "flex-start" | "center" | "flex-end" | "baseline" | "stretch";
};

/** Every Layout's Cluster: children in a row that wraps, such as toolbar items or tags. */
export function Cluster(props: ClusterProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "space", "justify", "align"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-cluster", own.class)}
      style={{
        "--l-space": space(own.space ?? "2"),
        "--l-justify": own.justify ?? "flex-start",
        "--l-align": own.align ?? "center",
      }}
    />
  );
}
