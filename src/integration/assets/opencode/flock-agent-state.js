// installed by flock
// managed by flock; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// FLOCK_INTEGRATION_ID=opencode
// FLOCK_INTEGRATION_VERSION=6
//
// Thin plugin (#158). The wire protocol lives once in the flk binary at
// `flk hook opencode <action>` (Rust, cli::hook). This plugin is just the
// opencode-side adapter: it maps opencode's session lifecycle events to a
// `session` action and hands the sessionID to flk over stdin. No socket code.
// On compaction it asks `flk hook opencode compacting` for the architect's
// delegate table and adds it to the compaction context (#962).

import { spawn } from "node:child_process";

// Forward a session id to `flk hook opencode session`. Fire-and-forget: a
// plugin must never block or fail the agent, so every error is swallowed.
function reportSession(sessionID) {
  if (!sessionID) {
    return;
  }
  const bin = process.env.FLOCK_BIN || "flk";
  try {
    const child = spawn(bin, ["hook", "opencode", "session"], {
      stdio: ["pipe", "ignore", "ignore"],
    });
    // If the child exits before we finish writing, node surfaces EPIPE on
    // stdin (and ENOENT/EACCES as a spawn `error`). Swallow both so a failed
    // report never throws into the agent.
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ session_id: sessionID }));
  } catch {
    // spawn threw synchronously (e.g. bad bin) — no-op.
  }
}

// Bound on the compaction hook: opencode awaits it before compacting, so a
// stalled flk must never hold the compaction up for long.
const DELEGATE_TABLE_TIMEOUT_MS = 2000;

// Read the delegate table `flk hook opencode compacting` prints. Resolves to
// "" on no edges, a failure, or the timeout; never rejects.
function delegateTable() {
  const bin = process.env.FLOCK_BIN || "flk";
  return new Promise((resolve) => {
    let out = "";
    let child;
    const done = () => {
      clearTimeout(timer);
      resolve(out.trim());
    };
    const timer = setTimeout(() => {
      out = "";
      try {
        child?.kill();
      } catch {}
      resolve("");
    }, DELEGATE_TABLE_TIMEOUT_MS);
    try {
      child = spawn(bin, ["hook", "opencode", "compacting"], {
        stdio: ["pipe", "pipe", "ignore"],
      });
      child.on("error", () => {
        out = "";
        done();
      });
      child.stdin.on("error", () => {});
      child.stdout.on("data", (chunk) => {
        out += chunk;
      });
      child.on("close", done);
      child.stdin.end("{}");
    } catch {
      out = "";
      done();
    }
  });
}

export const FlockAgentStatePlugin = async () => {
  if (
    process.env.FLOCK_ENV !== "1" ||
    !process.env.FLOCK_SOCKET_PATH ||
    !process.env.FLOCK_PANE_ID
  ) {
    return {};
  }

  return {
    event: async ({ event }) => {
      switch (event?.type) {
        case "session.created":
        case "session.updated":
        case "session.status": {
          const sessionID = event?.properties?.sessionID;
          if (typeof sessionID === "string" && sessionID) {
            reportSession(sessionID);
          }
          break;
        }
        default:
          break;
      }
    },
    // ADR-0027 §4: the table rides in the compaction context, never typed.
    "experimental.session.compacting": async (_input, output) => {
      const table = await delegateTable();
      if (table && Array.isArray(output?.context)) {
        output.context.push(table);
      }
    },
  };
};
