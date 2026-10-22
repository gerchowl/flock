/**
 * The runtime guard for the server's permission record on `agent/<id>`
 * (#2224). The shape is the generated {@link PermissionRecord}; the server
 * sends it as a known session event. A record that does not match fails
 * visibly instead of being dropped: {@link permissionRecord} names what is
 * wrong, and the inbox shows it. The guard does not check the optional
 * `grant_request_id`, `grant_request_seq` and `ignored_suggestions` fields, and
 * no card reads them.
 *
 * @module
 */
import type { KnownSessionEvent } from "../gen/KnownSessionEvent";
import type { PermissionCloseReason } from "../gen/PermissionCloseReason";
import type { PermissionDecision } from "../gen/PermissionDecision";
import type { PermissionRecord } from "../gen/PermissionRecord";

/** An open permission request, with its server-assigned `request_seq`. */
export type OpenPermissionRecord = Extract<PermissionRecord, { state: "open" }>;

/**
 * A permission record that does not match the generated shape. It is shown,
 * never dropped; `request_id` is `null` only when it cannot be read.
 */
export type InvalidPermissionRecord = { readonly state: "invalid"; readonly request_id: string | null; readonly problem: string };

/** Typed against the generated unions, so a new decision or reason fails `tsc` until listed. */
const DECISIONS: { readonly [D in PermissionDecision]: true } = { allow_once: true, allow_always: true, deny: true };
const REASONS: { readonly [R in PermissionCloseReason]: true } = {
  ended: true,
  cancelled: true,
  result: true,
  interrupted: true,
  session_ended: true,
  permission_request_limit: true,
  permission_request_too_large: true,
};

type Json = { readonly [key: string]: unknown };
const isObject = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);
const oneOf = (set: object, value: unknown): boolean => typeof value === "string" && Object.hasOwn(set, value);
const listed = (set: object): string => Object.keys(set).join(", ");

/** The first thing wrong with an open record, or `null`. */
function openProblem(ev: Json): string | null {
  const request = ev["request"];
  if (!isObject(request)) return "request is missing";
  if (typeof request["tool_name"] !== "string") return "request.tool_name is missing";
  if (!isObject(request["input"])) return "request.input is missing";
  return null;
}

/** The first thing wrong with a closed record, or `null`: an answer, or a typed reason. */
function closedProblem(ev: Json): string | null {
  const reasonProblem = (key: "reason" | "error_code"): string | null => (oneOf(REASONS, ev[key]) ? null : `${key} ${JSON.stringify(ev[key])} is not one of ${listed(REASONS)}`);
  if (!("decision" in ev)) return "reason" in ev ? reasonProblem("reason") : "closed record has neither decision nor reason";
  if (!oneOf(DECISIONS, ev["decision"])) return `decision ${JSON.stringify(ev["decision"])} is not one of ${listed(DECISIONS)}`;
  if (typeof ev["principal"] !== "string") return "principal is missing";
  if (typeof ev["source"] !== "string") return "source is missing";
  if (ev["message"] !== null && typeof ev["message"] !== "string") return "message is not a string or null";
  for (const key of ["reason", "error_code"] as const) {
    const problem = key in ev ? reasonProblem(key) : null;
    if (problem !== null) return problem;
  }
  return null;
}

/**
 * Reads one known session event: the generated {@link PermissionRecord}, an
 * {@link InvalidPermissionRecord} naming what is wrong when it is a
 * permission record that does not match, or `null` for any other event.
 */
export function permissionRecord(ev: KnownSessionEvent): PermissionRecord | InvalidPermissionRecord | null {
  if (ev.type !== "flk_permission_request") return null;
  const raw: Json = ev;
  const id = typeof raw["request_id"] === "string" ? raw["request_id"] : null;
  const invalid = (problem: string): InvalidPermissionRecord => ({ state: "invalid", request_id: id, problem });
  if (id === null) return invalid("request_id is missing");
  if (raw["state"] !== "open" && raw["state"] !== "closed") return invalid(`state ${JSON.stringify(raw["state"])} is not open or closed`);
  if (typeof raw["request_seq"] !== "number" || !Number.isSafeInteger(raw["request_seq"])) return invalid("request_seq is missing or not an integer");
  const problem = raw["state"] === "open" ? openProblem(raw) : closedProblem(raw);
  return problem === null ? ev : invalid(problem);
}
