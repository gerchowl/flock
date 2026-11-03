/**
 * The design-conformance suite's one allow-list (#2896): known violations,
 * each with the rule, the elements it covers (`selector`, matched against
 * the element itself), the pages it applies to and the issue that removes
 * it. One entry names one selector for one rule: `e2e/conformance.test.ts`
 * refuses wildcards and selector lists, fails on an entry that no longer
 * matches anything, and fails on an entry (or a page of one) that
 * `origin/dev`'s copy of this file does not have, so the list only shrinks.
 */
import type { Rule } from "./conformance-checks";

/** The pages the suite opens (see `PAGES` in `e2e/conformance.test.ts`). */
export type PageName = "gallery" | "workbench" | "workbench-phone" | "workbench-forced-phone";

/** One known violation. */
export type Allowance = {
  readonly rule: Rule;
  readonly selector: string;
  readonly pages: readonly PageName[];
  /** The filed issue that removes the entry. */
  readonly issue: number;
  readonly why: string;
};

/** The known violations, each removed by its issue. */
export const ALLOWED: readonly Allowance[] = [
  {
    rule: "typography",
    selector: "body",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css sets 14 px body text on desktop",
  },
  {
    rule: "typography",
    selector: ".mark__count",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css sizes the needs-you count at 0.6875rem (9.625 px)",
  },
  {
    rule: "hit-size",
    selector: ".group__head",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css stacks these rows with no gap (0.0 px from a neighbour)",
  },
  {
    rule: "hit-size",
    selector: '[data-testid="agent-row"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css stacks these rows with no gap (0.0 px from a neighbour)",
  },
  {
    rule: "grid",
    selector: '[data-testid="replay-badge"]',
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".topbar",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-right 14 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="palette-trigger"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-right 10.5 px)",
  },
  {
    rule: "grid",
    selector: ".waits",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (row-gap 1.75 px)",
  },
  {
    rule: "grid",
    selector: ".action",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-right 12.25 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="sidebar"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".scope",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 3.5 px)",
  },
  {
    rule: "grid",
    selector: ".group__head",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 3.5 px)",
  },
  {
    rule: "grid",
    selector: ".group__body",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-left 10.5 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="node-status"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (row-gap 7 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="agent-row"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 5.25 px)",
  },
  {
    rule: "grid",
    selector: ".mark__count",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-right 3.5 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="project-agent-row"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (padding-top 5.25 px)",
  },
  {
    rule: "grid",
    selector: ".focus__header",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".chip",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 1.75 px)",
  },
  {
    rule: "grid",
    selector: ".focus__actions",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (row-gap 7 px)",
  },
  {
    rule: "grid",
    selector: ".focus__body",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 14 px)",
  },
  {
    rule: "grid",
    selector: ".transcript",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (row-gap 10.5 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="turn"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (row-gap 10.5 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="known-event"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (padding-top 10.5 px)",
  },
  {
    rule: "grid",
    selector: ".event__label",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (margin-bottom 3.5 px)",
  },
  {
    rule: "grid",
    selector: ".text",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (margin-top 3.5 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="transcript-synced"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy styles.css spaces it in rem on a 14 px root (margin-top 7 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="composer"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".composer__sent",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (row-gap 3.5 px)",
  },
  {
    rule: "grid",
    selector: ".composer__input",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".composer__footer",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (row-gap 14 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="inbox"]',
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".inbox__title",
    pages: ["workbench"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".inbox__note",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 7 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="inbox-card"]',
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 7 px)",
  },
  {
    rule: "grid",
    selector: ".card__title",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 3.5 px)",
  },
  {
    rule: "grid",
    selector: ".card__choices",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 7 px)",
  },
  {
    rule: "hit-size",
    selector: ".tabbar__tab",
    pages: ["workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css stacks these rows with no gap (0.0 px from a neighbour)",
  },
  {
    rule: "grid",
    selector: ".phone__screen",
    pages: ["workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-top 11.25 px)",
  },
  {
    rule: "grid",
    selector: ".phone__title",
    pages: ["workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-top 3.75 px)",
  },
  {
    rule: "grid",
    selector: '[data-testid="inbox-headline"]',
    pages: ["workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (margin-bottom 7.5 px)",
  },
  {
    rule: "grid",
    selector: ".tabbar__tab",
    pages: ["workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy workbench.css spaces it in rem on a 14 px root (padding-right 6 px)",
  },
  {
    rule: "structure",
    selector: '[data-testid="group-project"] > .group__head',
    pages: ["workbench"],
    // Unfiled: tsc refuses this until the issue drafted in the PR body has a number.
    issue: "UNFILED: GUI sidebar: flatten the Fleet tree to two disclosure levels",
    why: "the Fleet tree nests node, then project: three disclosure levels",
  },
];
