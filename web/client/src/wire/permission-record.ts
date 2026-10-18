/**
 * The server's permission record on `agent/<id>` (#2224): the one place that
 * declares its shape. Hand-typed until #2224 lands; replace with gen/ then (#2224).
 *
 * `{"type":"flk_permission_request","state":"open"|"closed","request_id",
 * "request_seq","request":ToolRequest,…}`. A record is open until a closed
 * record with the same `request_id` arrives; there is no "decided" state.
 *
 * @module
 */
import type { ToolRequest } from "../gen/ToolRequest";

/** An open permission request, with its server-assigned `request_seq`. */
export type OpenPermissionRecord = {
  readonly type: "flk_permission_request";
  readonly state: "open";
  readonly request_id: string;
  /** Echoed verbatim by `agent.decide`; never computed here. */
  readonly request_seq: number;
  readonly request: ToolRequest;
};

/** The operator's decision on a closed record (`agent.decide`'s vocabulary). */
export type PermissionDecision = "allow_once" | "allow_always" | "deny";
/** Why a host fact closed a request without a decision. */
export type CloseReason = "ended" | "cancelled" | "result" | "interrupted" | "session_ended";

/** The same request, closed: by a decision with its principal and source, or by a typed reason. */
export type ClosedPermissionRecord = {
  readonly type: "flk_permission_request";
  readonly state: "closed";
  readonly request_id: string;
} & (
  | { readonly decision: PermissionDecision; readonly principal: string; readonly source: string; readonly message?: string | null }
  | { readonly reason: CloseReason }
);

/** One record, tagged on `state`. */
export type PermissionRecord = OpenPermissionRecord | ClosedPermissionRecord;

/**
 * A `flk_permission_request` payload that does not match the shape above. It
 * is shown, never dropped: `request_id` is `null` only when it cannot be read.
 */
export type InvalidPermissionRecord = { readonly state: "invalid"; readonly request_id: string | null; readonly problem: string };

const DECISIONS: readonly string[] = ["allow_once", "allow_always", "deny"] satisfies readonly PermissionDecision[];
const REASONS: readonly string[] = ["ended", "cancelled", "result", "interrupted", "session_ended"] satisfies readonly CloseReason[];

const isObject = (value: unknown): value is { readonly [key: string]: unknown } => typeof value === "object" && value !== null && !Array.isArray(value);

/** The first thing wrong with an open record, or `null`. */
function openProblem(ev: { readonly [key: string]: unknown }): string | null {
  if (typeof ev["request_seq"] !== "number" || !Number.isSafeInteger(ev["request_seq"])) return "request_seq is missing or not an integer";
  const request = ev["request"];
  if (!isObject(request)) return "request is missing";
  if (typeof request["tool_name"] !== "string") return "request.tool_name is missing";
  if (!isObject(request["input"])) return "request.input is missing";
  return null;
}

/** The first thing wrong with a closed record, or `null`. */
function closedProblem(ev: { readonly [key: string]: unknown }): string | null {
  const [decided, reasoned] = ["decision" in ev, "reason" in ev];
  if (decided === reasoned) return decided ? "closed record has both decision and reason" : "closed record has neither decision nor reason";
  if (reasoned) return typeof ev["reason"] === "string" && REASONS.includes(ev["reason"]) ? null : `reason ${JSON.stringify(ev["reason"])} is not one of ${REASONS.join(", ")}`;
  if (typeof ev["decision"] !== "string" || !DECISIONS.includes(ev["decision"])) return `decision ${JSON.stringify(ev["decision"])} is not one of ${DECISIONS.join(", ")}`;
  if (typeof ev["principal"] !== "string") return "principal is missing";
  if (typeof ev["source"] !== "string") return "source is missing";
  const message = ev["message"];
  return message === undefined || message === null || typeof message === "string" ? null : "message is not a string";
}

/**
 * Reads one agent-stream payload: a {@link PermissionRecord}, an
 * {@link InvalidPermissionRecord} naming what is wrong when it is a permission
 * record that does not match, or `null` when it is some other event.
 */
export function permissionRecord(ev: unknown): PermissionRecord | InvalidPermissionRecord | null {
  if (!isObject(ev) || ev["type"] !== "flk_permission_request") return null;
  const id = typeof ev["request_id"] === "string" ? ev["request_id"] : null;
  const invalid = (problem: string): InvalidPermissionRecord => ({ state: "invalid", request_id: id, problem });
  if (id === null) return invalid("request_id is missing");
  if (ev["state"] !== "open" && ev["state"] !== "closed") return invalid(`state ${JSON.stringify(ev["state"])} is not open or closed`);
  const problem = ev["state"] === "open" ? openProblem(ev) : closedProblem(ev);
  return problem === null ? (ev as PermissionRecord) : invalid(problem);
}
