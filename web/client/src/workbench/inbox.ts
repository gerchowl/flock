/**
 * Inbox cards (ADR-0038 §2a, right region; phone Inbox tab).
 *
 * {@link InboxCard} is a presentation type. Live cards come from the inbox
 * stream (G5 #1597) and S6-MVP's permission prompts (#1429); until then the
 * workbench shows the sample cards below, labelled as samples, with their
 * answer buttons disabled and naming what they wait for.
 *
 * @module
 */

/** One card in the attention queue. */
export type InboxCard = {
  readonly id: string;
  readonly kind: "permission" | "question";
  /** The agent the card belongs to, as in `agent/<id>`. */
  readonly agent: string;
  readonly title: string;
  readonly detail: string;
  /** One button per choice, in the server's order. */
  readonly choices: readonly string[];
  /** Whether the agent is blocked on this card. Blocking cards sort first. */
  readonly blocking: boolean;
};

/** Sample cards until G5. */
export const SAMPLE_INBOX: readonly InboxCard[] = [
  {
    id: "sample-permission",
    kind: "permission",
    agent: "fixture",
    title: "Run Bash?",
    detail: "just check",
    choices: ["Allow once", "Allow always", "Deny"],
    blocking: true,
  },
  {
    id: "sample-question",
    kind: "question",
    agent: "atlas",
    title: "Which fixture set should the replay use?",
    detail: "The recorded S1 session, or the S7a wire scripts.",
    choices: ["Recorded session", "Wire scripts"],
    blocking: false,
  },
];

/** Blocking cards first, otherwise in the given order. */
export function sortInbox(cards: readonly InboxCard[]): InboxCard[] {
  return [...cards].sort((a, b) => Number(b.blocking) - Number(a.blocking));
}
