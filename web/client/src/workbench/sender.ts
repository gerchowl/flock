/**
 * The workbench's {@link Sender} over a {@link WireConnection}: `agent.send`
 * and `agent.decide` both go through the outbox (ADR-0038 §3), so each has a
 * `cmd_id`, a resend within the window, and an explicit unknown outcome.
 *
 * @module
 */
import type { JsonValue } from "../gen/serde_json/JsonValue";
import type { WireConnection } from "../wire/connection";
import type { Sender } from "./Composer";

/** Creates the sender; `revision` is the signal that changes whenever the connection does. */
export function createWireSender(connection: WireConnection, revision: () => number): Sender {
  return {
    commands: () => (revision(), connection.commands()),
    // `submit` sends the message, not only types it (`AgentSendParams`; S7a's agent-send script).
    send: (agent, text) => connection.command("agent.send", { target: agent, text, submit: true }),
    // `AgentDecide` carries `request_seq` as the card's open record sent it; nothing here changes it.
    decide: (args) => connection.command("agent.decide", args satisfies { readonly [key: string]: JsonValue } as JsonValue),
    blocked: () => (revision(), connection.principal === null ? "Sending opens once the server has welcomed this client." : null),
  };
}
