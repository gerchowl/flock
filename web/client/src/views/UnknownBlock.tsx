/**
 * The collapsed raw block for an unknown event (ADR-0036 §8), built by
 * `describeUnknown` in `src/unknown.ts`.
 *
 * @module
 */
import type { JSX } from "solid-js";

import { describeUnknown, type UnknownInput } from "../unknown";

/** Renders `props.input` collapsed, with its shape key as the title. */
export function UnknownBlock(props: { readonly input: UnknownInput }): JSX.Element {
  const view = (): ReturnType<typeof describeUnknown> => describeUnknown(props.input);
  return (
    <details class="event event--unknown" data-testid="unknown-event">
      <summary>{view().title}</summary>
      <pre class="code">{view().body}</pre>
    </details>
  );
}
