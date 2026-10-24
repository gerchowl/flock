/**
 * Answering permission prompts from the inbox cards (#1749 PR 2): `agent.decide`
 * through the outbox, over the scripted fake server. Virtual time, no sleeps.
 */
import { afterEach, describe, expect, test } from "bun:test";

import type { ServerFrame } from "../src/gen/ServerFrame";
import type { OutboxStatus } from "../src/gen/OutboxStatus";
import type { OutboxEntry } from "../src/wire/outbox";
import { MESSAGE_MAX_BYTES, REFUSALS, decideState } from "../src/workbench/decide";
import { STATUS_LABELS } from "../src/workbench/Composer";
import { WELCOME, sentFrames, step } from "./wire/harness";
import { $$, cards, frames, mountInbox, unmountInbox, type App } from "./wire/inbox-app";

afterEach(unmountInbox);

type Cmd = { cmd_id: string; verb: string; args: Record<string, unknown>; resume_only?: boolean };
const cmds = (app: App): Cmd[] => frames(app.h, "cmd") as unknown as Cmd[];
const button = (app: App, label: string): HTMLButtonElement => {
  const found = [...app.root.querySelectorAll<HTMLButtonElement>('[data-testid="inbox-card"] button')].find((b) => b.textContent === label);
  if (found === undefined) throw new Error(`no ${label} button`);
  return found;
};
const status = (app: App): string => app.root.querySelector('[data-testid="decide-status"]')?.textContent ?? "";
const ack = (app: App, frame: Partial<Extract<ServerFrame, { t: "ack" }>> & { cmd_id: string; ok: boolean }): void => app.h.step(step({ step: "send", frame: { t: "ack", ...frame } as ServerFrame }));
const refusal = (code: string) => ({ code, message: `server said ${code}`, exit: 1, retry: "never" as const });

/** A desktop or phone workbench with one open prompt `r1` (request_seq 41) for atlas. */
function prompt(mode: "desktop" | "phone" = "desktop"): App {
  const app = mountInbox(mode, { sender: true });
  app.fleet("atlas", ["r1"]);
  app.event("atlas", "r1", "Bash", { command: "just check" }, 41);
  // Commands are sent in Live, which needs the agent stream to have synced.
  app.synced("atlas");
  return app;
}

for (const mode of ["desktop", "phone"] as const) {
  describe(`answering a prompt (${mode})`, () => {
    test("Allow once sends exactly one agent.decide with the record's request_seq verbatim and no empty message", () => {
      const app = prompt(mode);
      button(app, "Allow once").click();
      button(app, "Allow once").click();
      expect(cmds(app)).toHaveLength(1);
      expect(cmds(app)[0]?.verb).toBe("agent.decide");
      expect(cmds(app)[0]?.args).toEqual({ agent: "atlas", request_id: "r1", request_seq: 41, decision: "allow_once" });
      expect("message" in (cmds(app)[0]?.args ?? {})).toBe(false);
    });

    test("after an ack the card says it was answered and leaves only on the closed record", () => {
      const app = prompt(mode);
      button(app, "Deny").click();
      const id = cmds(app)[0]?.cmd_id ?? "";
      expect(status(app)).toBe(STATUS_LABELS.sent);
      ack(app, { cmd_id: id, ok: true, result: { request_id: "r1", decision: "deny" } });
      expect(cards(app.root)).toHaveLength(1);
      expect(status(app)).toBe("Answered; waiting for the agent");
      expect(button(app, "Allow once").hasAttribute("disabled")).toBe(true);
      app.close("atlas", "r1");
      expect(cards(app.root)).toEqual([]);
    });

    test("a permission_request_closed refusal is shown, never resent, and the buttons wait for a new seq", () => {
      const app = prompt(mode);
      button(app, "Allow once").click();
      const id = cmds(app)[0]?.cmd_id ?? "";
      ack(app, { cmd_id: id, ok: false, error: refusal("permission_request_closed") });
      expect(status(app)).toBe(REFUSALS.permission_request_closed);
      expect(cards(app.root)).toHaveLength(1);
      for (const label of ["Allow once", "Always allow", "Deny"]) expect(button(app, label).hasAttribute("disabled")).toBe(true);
      button(app, "Deny").click();
      expect(cmds(app)).toHaveLength(1);
      app.event("atlas", "r1", "Bash", { command: "just check" }, 42);
      expect(button(app, "Allow once").hasAttribute("disabled")).toBe(true);
      button(app, "Review new request").click();
      expect(button(app, "Allow once").hasAttribute("disabled")).toBe(false);
      button(app, "Allow once").click();
      expect(cmds(app).map((c) => c.args["request_seq"])).toEqual([41, 42]);
    });
  });
}

describe("the card's state is the outbox entry's", () => {
  const card = { id: "r1", kind: "permission", agent: "atlas", agentLabel: "ATLAS", title: "Bash", detail: "", choices: [], blocking: true, requestSeq: 41 } as const;
  const entry = (status: OutboxStatus, requestSeq = 41): OutboxEntry => ({ cmd_id: "c1", verb: "agent.decide", args: { agent: "atlas", request_id: "r1", request_seq: requestSeq, decision: "deny" }, principal: "p", created_ms: 0, status });

  test("every outbox state has a line; only the entry for this request_seq counts", () => {
    expect((["pending", "sent", "uncertain", "unknown_outcome"] as const).map((status) => decideState([entry(status)], card))).toEqual(
      (["pending", "sent", "uncertain", "unknown_outcome"] as const).map((status) => ({ kind: "inflight", line: STATUS_LABELS[status] })),
    );
    expect(decideState([entry("acked")], card)).toEqual({ kind: "answered" });
    expect(decideState([entry("failed")], card)).toEqual({ kind: "refused", line: `${STATUS_LABELS.failed}: no reason given` });
    expect(decideState([entry("failed", 40)], card)).toEqual({ kind: "idle" });
    expect(decideState([], card)).toEqual({ kind: "idle" });
  });
});

describe("answering a prompt", () => {
  test("a refusal code outside AgentDecideErrorCode, like invalid_params, shows its message, is never retried, and waits for a new seq", () => {
    const app = prompt();
    button(app, "Allow once").click();
    ack(app, { cmd_id: cmds(app)[0]?.cmd_id ?? "", ok: false, error: refusal("invalid_params") });
    expect(status(app)).toBe(`${STATUS_LABELS.failed}: server said invalid_params`);
    for (const label of ["Allow once", "Always allow", "Deny"]) expect(button(app, label).hasAttribute("disabled")).toBe(true);
    button(app, "Allow once").click();
    expect(cmds(app)).toHaveLength(1);
    app.event("atlas", "r1", "Bash", { command: "just check" }, 42);
    button(app, "Review new request").click();
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(false);
  });

  test("a caught send error covers the status only until the status changes", () => {
    const app = prompt();
    const connection = app.h.connection as unknown as { command: (...args: unknown[]) => string };
    const real = connection.command.bind(connection);
    connection.command = () => {
      throw new Error("could not queue the answer");
    };
    button(app, "Allow once").click();
    expect(status(app)).toBe("could not queue the answer");
    expect(cmds(app)).toHaveLength(0);
    connection.command = real;
    app.gap("atlas");
    expect(status(app)).toBe("This agent's history has a gap; answering waits for it to catch up.");
  });

  test("every typed refusal has plain words", () => {
    const app = prompt();
    button(app, "Deny").click();
    ack(app, { cmd_id: cmds(app)[0]?.cmd_id ?? "", ok: false, error: refusal("not_authorized") });
    expect(status(app)).toBe(REFUSALS.not_authorized);
    expect(Object.keys(REFUSALS)).toEqual(["permission_request_unknown", "permission_request_closed", "not_authorized"]);
  });

  test("an untyped refusal shows the server's message", () => {
    const app = prompt();
    button(app, "Deny").click();
    ack(app, { cmd_id: cmds(app)[0]?.cmd_id ?? "", ok: false, error: refusal("something_new") });
    expect(status(app)).toBe(`${STATUS_LABELS.failed}: server said something_new`);
  });

  test("across a reconnect the decision is never sent again as a first send, and the card still shows the outbox state with its buttons disabled", () => {
    // The outbox's resume-only resend and window expiry are G3b (#1595); this covers what the client does today.
    const app = prompt();
    button(app, "Allow once").click();
    const id = cmds(app)[0]?.cmd_id ?? "";
    app.h.step(step({ step: "drop" }));
    app.h.step(step({ step: "expect_connect", within_ms: 500, not_before_ms: 250 }));
    app.h.step(step({ step: "open" }));
    app.h.step(step({ step: "send", frame: WELCOME }));
    const subs = frames(app.h, "sub") as unknown as { sub_id: string; stream: string }[];
    const fleet = subs.filter((f) => f.stream === "fleet").at(-1);
    const agent = subs.filter((f) => f.stream === "agent/atlas").at(-1);
    app.h.step(step({ step: "send", frame: { t: "gap", sub_id: fleet?.sub_id ?? "", from: 1, to: 0, snapshot_to: 1 } }));
    app.h.step(step({ step: "send", frame: { t: "ev", sub_id: fleet?.sub_id ?? "", stream: "fleet", seq: 1, ts_ms: 0, ev: { type: "fleet.remove", id: "nobody" } } }));
    app.h.step(step({ step: "send", frame: { t: "synced", sub_id: fleet?.sub_id ?? "", seq: 1 } }));
    app.h.step(step({ step: "send", frame: { t: "synced", sub_id: agent?.sub_id ?? "", seq: 2 } }));
    expect(app.h.connection.state).toBe("Live");
    expect(cmds(app).map((c) => [c.cmd_id, c.resume_only])).toEqual([[id, false]]);
    expect(button(app, "Deny").hasAttribute("disabled")).toBe(true);
    expect(cards(app.root)).toHaveLength(1);
    expect(status(app)).toBe(STATUS_LABELS.sent);
  });

  test("a new request_seq while the Always-allow confirm is open cancels it, keeps the draft, and sends nothing until the review is acknowledged", () => {
    const app = prompt();
    const box = app.root.querySelector<HTMLInputElement>(".decide__message");
    if (box === null) throw new Error("no message box");
    box.value = "keep me";
    box.dispatchEvent(new Event("input", { bubbles: true }));
    button(app, "Always allow").click();
    expect($$(app.root, "decide")[0]?.textContent).toContain("Confirm always allow");
    app.event("atlas", "r1", "Edit", { file_path: "other.rs" }, 42);
    expect($$(app.root, "decide")[0]?.textContent).not.toContain("Confirm always allow");
    expect(app.root.querySelector<HTMLInputElement>(".decide__message")?.value).toBe("keep me");
    expect(status(app)).toBe("Request changed — review it.");
    for (const label of ["Allow once", "Always allow", "Deny"]) {
      expect(button(app, label).hasAttribute("disabled")).toBe(true);
      button(app, label).click();
    }
    expect(cmds(app)).toHaveLength(0);
    button(app, "Review new request").click();
    button(app, "Always allow").click();
    expect($$(app.root, "decide")[0]?.textContent).toContain("Always allow Edit");
    button(app, "Confirm always allow").click();
    expect(cmds(app).map((c) => c.args["request_seq"])).toEqual([42]);
  });

  for (const [label, decision] of [
    ["Allow once", "allow_once"],
    ["Deny", "deny"],
  ] as const) {
    test(`the 41 → 42 race: ${label} sends nothing for 42 until the review is acknowledged`, () => {
      const app = prompt();
      app.event("atlas", "r1", "Edit", { file_path: "other.rs" }, 42);
      expect(button(app, label).hasAttribute("disabled")).toBe(true);
      button(app, label).click();
      expect(cmds(app)).toHaveLength(0);
      button(app, "Review new request").click();
      button(app, label).click();
      expect(cmds(app).map((c) => [c.args["request_seq"], c.args["decision"]])).toEqual([[42, decision]]);
    });
  }

  test("an open confirm that becomes disabled (over the message limit) cannot be confirmed", () => {
    const app = prompt();
    button(app, "Always allow").click();
    const box = app.root.querySelector<HTMLInputElement>(".decide__message");
    if (box === null) throw new Error("no message box");
    box.value = "x".repeat(MESSAGE_MAX_BYTES + 1);
    box.dispatchEvent(new Event("input", { bubbles: true }));
    expect(button(app, "Confirm always allow").hasAttribute("disabled")).toBe(true);
    button(app, "Confirm always allow").click();
    expect(cmds(app)).toHaveLength(0);
    expect(status(app)).toContain("over the 2048-byte limit");
  });

  test("a send error for one request never shows on the next", () => {
    const app = prompt();
    const connection = app.h.connection as unknown as { command: (...args: unknown[]) => string };
    const real = connection.command.bind(connection);
    connection.command = () => {
      throw new Error("could not queue the answer");
    };
    button(app, "Allow once").click();
    expect(status(app)).toBe("could not queue the answer");
    connection.command = real;
    app.event("atlas", "r1", "Bash", { command: "just check" }, 42);
    expect(status(app)).toBe("Request changed — review it.");
    button(app, "Review new request").click();
    expect(status(app)).toBe("Answering sends your decision to the agent.");
  });

  test("a stale card, after a history gap, has its buttons disabled with the reason until it resyncs", () => {
    const app = prompt();
    app.gap("atlas");
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(true);
    expect(status(app)).toBe("This agent's history has a gap; answering waits for it to catch up.");
    button(app, "Allow once").click();
    expect(cmds(app)).toHaveLength(0);
    app.event("atlas", "r1", "Bash", { command: "just check" }, 41);
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(false);
  });

  test("the message is bounded in UTF-8 bytes, not characters, and over the limit disables the answer", () => {
    const type = (app: App, text: string): void => {
      const input = app.root.querySelector<HTMLInputElement>(".decide__message");
      if (input === null) throw new Error("no message box");
      input.value = text;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    };
    const app = prompt();
    const fits = "€".repeat(Math.floor(MESSAGE_MAX_BYTES / 3));
    type(app, `  ${fits}  `);
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(false);
    expect($$(app.root, "decide-count")[0]?.textContent).toBe(`${MESSAGE_MAX_BYTES - (fits.length * 3)} bytes left`);
    type(app, `${fits}😀`);
    expect(fits.length + 2).toBeLessThan(MESSAGE_MAX_BYTES);
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(true);
    expect($$(app.root, "decide-count")[0]?.textContent).toBe(`${fits.length * 3 + 4 - MESSAGE_MAX_BYTES} bytes over the limit`);
    expect(status(app)).toBe(`The message is ${fits.length * 3 + 4 - MESSAGE_MAX_BYTES} bytes over the ${MESSAGE_MAX_BYTES}-byte limit.`);
    button(app, "Allow once").click();
    expect(cmds(app)).toHaveLength(0);
    type(app, fits);
    button(app, "Allow once").click();
    expect(cmds(app)[0]?.args["message"]).toBe(fits);
  });

  test("a closed request drops its draft: nothing typed for it comes back with a later card", () => {
    const app = prompt();
    const box = (): HTMLInputElement => {
      const found = app.root.querySelector<HTMLInputElement>(".decide__message");
      if (found === null) throw new Error("no message box");
      return found;
    };
    box().value = "stale draft";
    box().dispatchEvent(new Event("input", { bubbles: true }));
    app.close("atlas", "r1");
    expect(cards(app.root)).toEqual([]);
    app.event("atlas", "r1", "Bash", { command: "just check" }, 50);
    expect(box().value).toBe("");
  });

  test("a card that is pending first and gets its open record later sends exactly one decide for that seq", () => {
    const app = mountInbox("desktop", { sender: true });
    app.fleet("atlas", ["r1"]);
    app.synced("atlas");
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(true);
    app.event("atlas", "r1", "Bash", { command: "just check" }, 77);
    expect(button(app, "Allow once").hasAttribute("disabled")).toBe(false);
    button(app, "Allow once").click();
    expect(cmds(app)).toHaveLength(1);
    expect(cmds(app)[0]?.args).toEqual({ agent: "atlas", request_id: "r1", request_seq: 77, decision: "allow_once" });
  });

  test("Always allow needs the confirm step, naming the tool, and a given message is sent", () => {
    const app = prompt();
    const input = app.root.querySelector<HTMLInputElement>(".decide__message");
    if (input === null) throw new Error("no message box");
    input.value = "  only for this repo  ";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    button(app, "Always allow").click();
    expect(cmds(app)).toHaveLength(0);
    expect($$(app.root, "decide")[0]?.textContent).toContain("Always allow Bash");
    button(app, "Cancel").click();
    expect(cmds(app)).toHaveLength(0);
    button(app, "Always allow").click();
    button(app, "Confirm always allow").click();
    expect(cmds(app)).toHaveLength(1);
    expect(cmds(app)[0]?.args).toEqual({ agent: "atlas", request_id: "r1", request_seq: 41, decision: "allow_always", message: "only for this repo" });
  });
});
