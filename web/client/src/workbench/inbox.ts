/**
 * Inbox cards (ADR-0038 §2a, right region; phone Inbox tab).
 *
 * {@link InboxCard} is a presentation type. Fixture mode shows the sample
 * cards below, labelled as samples. Live mode shows open permission prompts
 * (`workbench/permissions.ts`, #1749 PR 1; the server's open/closed record, `wire/permission-record.ts`), which answer for real.
 * A sample card's answers are not built: hidden, and in a development build
 * disabled, naming what they wait for.
 *
 * @module
 */

/** One card in the attention queue. */
export type InboxCard = {
  readonly id: string;
  readonly kind: "permission" | "question";
  /** The agent the card belongs to, as in `agent/<id>`. */
  readonly agent: string;
  /** The agent's name in the sidebar. */
  readonly agentLabel: string;
  readonly title: string;
  readonly detail: string;
  /** One button per choice, in the server's order. */
  readonly choices: readonly string[];
  /** Whether the agent is blocked on this card. Blocking cards sort first. */
  readonly blocking: boolean;
  /** What answering waits for, shown beside a sample card's disabled choices; live permission cards answer for real. */
  readonly waitsFor?: string;
  /** A permission card's server `request_seq`, verbatim, for PR 2's decide; absent on samples and pending cards. */
  readonly requestSeq?: number;
  /** The agent's stream had a history gap that nothing has followed yet: the card may be out of date. */
  readonly stale?: boolean;
  /** Why the card has no usable request, raw, for its Details; `detail` words it plainly (`refusals.tsx`). */
  readonly diagnosis?: { readonly code?: string; readonly message: string };
};

const SAMPLE_WAITS_FOR = "the inbox stream";

/** Sample cards until G5. */
export const SAMPLE_INBOX: readonly InboxCard[] = [
  {
    id: "sample-permission",
    kind: "permission",
    agent: "fixture",
    agentLabel: "fixture",
    title: "Run Bash?",
    detail: "just check",
    choices: ["Allow once", "Allow always", "Deny"],
    blocking: true,
    waitsFor: SAMPLE_WAITS_FOR,
  },
  {
    id: "sample-question",
    kind: "question",
    agent: "atlas",
    agentLabel: "atlas",
    title: "Which fixture set should the replay use?",
    detail: "The recorded session, or the wire scripts.",
    choices: ["Recorded session", "Wire scripts"],
    blocking: false,
    waitsFor: SAMPLE_WAITS_FOR,
  },
];

/** Blocking cards first, otherwise in the given order. */
export function sortInbox(cards: readonly InboxCard[]): InboxCard[] {
  return [...cards].sort((a, b) => Number(b.blocking) - Number(a.blocking));
}
