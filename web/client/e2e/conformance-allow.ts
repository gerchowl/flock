/**
 * The design-conformance suite's one allow-list (#2896): known violations,
 * each with the rule, the elements it covers (`selector`, matched against
 * the element itself), the pages it applies to and the issue that removes
 * it (`draft:` names a follow-up drafted in the PR that added the entry).
 * `e2e/conformance.test.ts` fails on an entry that no longer matches
 * anything, and on any entry not in its frozen list, so this list only
 * shrinks.
 */
import type { Rule } from "./conformance-checks";

/** The pages the suite opens (see `PAGES` in `e2e/conformance.test.ts`). */
export type PageName = "gallery" | "workbench" | "workbench-phone" | "workbench-forced-phone";

/** One known violation. */
export type Allowance = {
  readonly rule: Rule;
  readonly selector: string;
  readonly pages: readonly PageName[];
  readonly issue: number | `draft:${string}`;
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
    rule: "grid",
    selector: "#root, #root *",
    pages: ["workbench", "workbench-phone", "workbench-forced-phone"],
    issue: 2913,
    why: "the legacy styles.css and workbench.css space in rem on a 14 px root (3.5, 7, 10.5 px)",
  },
  {
    rule: "structure",
    selector: '[data-testid="group-project"] > .group__head',
    pages: ["workbench"],
    issue: "draft:sidebar-depth",
    why: "the Fleet tree nests node, then project: three disclosure levels",
  },
];
