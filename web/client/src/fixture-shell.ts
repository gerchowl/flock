/**
 * The fixture build's shell (ADR-0038 §6, `just client-preview`).
 *
 * A fixture build has no host to inject {@link SHELL_CONFIG_GLOBAL}, so this
 * module plays the shell's part: `vite.config.ts` loads it ahead of
 * `main.tsx` in `--mode fixture` only, and it assigns a fixture
 * configuration with the recorded S1 session (#1379) bundled in. Production
 * builds never include it, so a live bundle cannot replay fixtures.
 *
 * The recording is imported from its checked-in path at build time, never
 * copied into the client tree.
 *
 * @module
 */
import fixtureJsonl from "../../../tests/fixtures/claude-session/events.jsonl?raw";

import { SHELL_CONFIG_GLOBAL, type FixtureShellConfig } from "./config";

const config: FixtureShellConfig = {
  transport: "fixture",
  fixtureJsonl,
  source: "tests/fixtures/claude-session/events.jsonl",
};

Reflect.set(globalThis, SHELL_CONFIG_GLOBAL, config);
