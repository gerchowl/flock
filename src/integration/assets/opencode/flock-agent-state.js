// installed by flock
// managed by flock; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// FLOCK_INTEGRATION_ID=opencode
// FLOCK_INTEGRATION_VERSION=5
//
// Thin plugin (#158). The wire protocol lives once in the flk binary at
// `flk hook opencode <action>` (Rust, cli::hook). This plugin is just the
// opencode-side adapter: it maps opencode's session lifecycle events to a
// `session` action and hands the sessionID to flk over stdin. No socket code.

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
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ session_id: sessionID }));
  } catch {
    // spawn threw synchronously (e.g. bad bin) — no-op.
  }
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
  };
};
