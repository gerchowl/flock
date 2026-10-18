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

/** The same request, closed (decision, message, principal, source or a typed reason follow). */
export type ClosedPermissionRecord = {
  readonly type: "flk_permission_request";
  readonly state: "closed";
  readonly request_id: string;
};

/** One record, tagged on `state`. */
export type PermissionRecord = OpenPermissionRecord | ClosedPermissionRecord;

const isObject = (value: unknown): value is { readonly [key: string]: unknown } => typeof value === "object" && value !== null && !Array.isArray(value);

/** Reads one agent-stream payload as a {@link PermissionRecord}, or `null` when it is not one. */
export function permissionRecord(ev: unknown): PermissionRecord | null {
  if (!isObject(ev) || ev["type"] !== "flk_permission_request" || typeof ev["request_id"] !== "string") return null;
  if (ev["state"] === "closed") return { type: "flk_permission_request", state: "closed", request_id: ev["request_id"] };
  const request = ev["request"];
  if (ev["state"] !== "open" || typeof ev["request_seq"] !== "number" || !isObject(request) || typeof request["tool_name"] !== "string" || !isObject(request["input"])) return null;
  return ev as OpenPermissionRecord;
}
