import { splitProps, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";

import { SPACE, type SpaceStep } from "../../tokens";
import { classes, space, type LayoutProps } from "./common";

/** Every primitive's props; {@link AnchorPanel} adds where it sits. */
export type AnchorProps = LayoutProps;

export type AnchorPanelProps = LayoutProps & {
  /** The gap between the trigger and the panel. Default `"1"` (4 px). */
  readonly space?: SpaceStep;
  /** The trigger's edge the panel lines up with. Default `"start"`. */
  readonly align?: "start" | "end";
};

/**
 * A popover's anchor: an inline box holding a trigger and its
 * {@link AnchorPanel}, which it positions. Not one of Every Layout's; the
 * one primitive that takes content out of the flow.
 */
export function Anchor(props: AnchorProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class"]);
  return <Dynamic component={own.as ?? "div"} {...rest} class={classes("l-anchor", own.class)} />;
}

/** The panel of an {@link Anchor}: below its trigger, above the content, out of the flow. Hide it with `hidden`. */
export function AnchorPanel(props: AnchorPanelProps): JSX.Element {
  const [own, rest] = splitProps(props, ["as", "class", "space", "align"]);
  return (
    <Dynamic
      component={own.as ?? "div"}
      {...rest}
      class={classes("l-anchor__panel", own.class)}
      data-align={own.align ?? "start"}
      style={{ "--l-space": space(own.space ?? SPACE[1]) }}
    />
  );
}
