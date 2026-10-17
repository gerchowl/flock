/**
 * The "fixture replay" badge (ADR-0038 §10, G1).
 *
 * Every view fed by the fixture transport shows it, so a preview is never
 * mistaken for live data. It is a status landmark, announced once by screen
 * readers, and stays visible while the transcript scrolls.
 *
 * @module
 */
import type { JSX } from "solid-js";

/** Props of {@link ReplayBadge}. */
export type ReplayBadgeProps = {
  /** Where the replayed recording came from, e.g. its repository path. */
  readonly source: string;
};

/** A persistent badge marking the page as a replay of recorded fixtures. */
export function ReplayBadge(props: ReplayBadgeProps): JSX.Element {
  return (
    <div class="replay-badge" role="status" data-testid="replay-badge">
      <strong>Fixture replay</strong>
      <span class="replay-badge__source">{props.source}</span>
    </div>
  );
}
