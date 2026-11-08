/**
 * The bootstrap baseline for the allow-list guard (#2896): every entry of
 * `e2e/conformance-allow.ts` as this suite was merged. `origin/dev` has no
 * allow-list before that merge, so the guard in `e2e/conformance.test.ts`
 * compares against this frozen copy instead, and it always compares: an
 * entry, or a page of one, that is not here fails. Never add to it; once
 * `origin/dev` has the allow-list, the guard compares against that.
 */
import type { PageName } from "./conformance-allow";
import type { Rule } from "./conformance-checks";

export const BASELINE: readonly { readonly rule: Rule; readonly selector: string; readonly pages: readonly PageName[] }[] = [
  { rule: "typography", selector: "body", pages: ["workbench"] },
  { rule: "typography", selector: ".mark__count", pages: ["workbench"] },
  { rule: "hit-size", selector: ".group__head", pages: ["workbench"] },
  { rule: "hit-size", selector: "[data-testid=\"agent-row\"]", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"replay-badge\"]", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".topbar", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"palette-trigger\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".waits", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".action", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: "[data-testid=\"sidebar\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".scope", pages: ["workbench"] },
  { rule: "grid", selector: ".group__head", pages: ["workbench"] },
  { rule: "grid", selector: ".group__body", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"node-status\"]", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"agent-row\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".mark__count", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"project-agent-row\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".focus__header", pages: ["workbench"] },
  { rule: "grid", selector: ".chip", pages: ["workbench"] },
  { rule: "grid", selector: ".focus__actions", pages: ["workbench"] },
  { rule: "grid", selector: ".focus__body", pages: ["workbench"] },
  { rule: "grid", selector: ".transcript", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"turn\"]", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"known-event\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".event__label", pages: ["workbench"] },
  { rule: "grid", selector: ".text", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"transcript-synced\"]", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"composer\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".composer__sent", pages: ["workbench"] },
  { rule: "grid", selector: ".composer__input", pages: ["workbench"] },
  { rule: "grid", selector: ".composer__footer", pages: ["workbench"] },
  { rule: "grid", selector: "[data-testid=\"inbox\"]", pages: ["workbench"] },
  { rule: "grid", selector: ".inbox__title", pages: ["workbench"] },
  { rule: "grid", selector: ".inbox__note", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: "[data-testid=\"inbox-card\"]", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".card__title", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".card__choices", pages: ["workbench", "workbench-phone", "workbench-forced-phone"] },
  { rule: "hit-size", selector: ".tabbar__tab", pages: ["workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".phone__screen", pages: ["workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".phone__title", pages: ["workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: "[data-testid=\"inbox-headline\"]", pages: ["workbench-phone", "workbench-forced-phone"] },
  { rule: "grid", selector: ".tabbar__tab", pages: ["workbench-phone", "workbench-forced-phone"] },
  { rule: "structure", selector: "[data-testid=\"group-project\"] > .group__head", pages: ["workbench"] },
];
