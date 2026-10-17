/**
 * Test helpers: the recorded session, read from its checked-in path, and a
 * bounded replay. `bun test`'s per-test timeout fails a replay that never
 * syncs; nothing here waits on wall-clock time.
 */
import type { SessionEntry, SessionStream, Transport, TransportFailure } from "../src/transport";

const FIXTURE_DIR = `${import.meta.dir}/../../../tests/fixtures/claude-session`;

/** Path of the recording; FLOCK_CLIENT_FIXTURE overrides it for fault injection. */
export const FIXTURE_PATH = process.env["FLOCK_CLIENT_FIXTURE"] ?? `${FIXTURE_DIR}/events.jsonl`;

/** The recording's text. */
export function readFixture(): Promise<string> {
  return Bun.file(FIXTURE_PATH).text();
}

/** The expected variant of each line, in order (`variants.txt`). */
export async function readVariants(): Promise<string[]> {
  const text = await Bun.file(`${FIXTURE_DIR}/variants.txt`).text();
  return text.split("\n").filter((line) => line !== "");
}

/** Subscribes from the start and resolves with every entry once synced. */
export function replay(transport: Transport, stream: SessionStream): Promise<SessionEntry[]> {
  return new Promise((resolve, reject) => {
    const entries: SessionEntry[] = [];
    transport.subscribe(stream, null, {
      entry: (entry) => entries.push(entry),
      synced: () => resolve(entries),
      failed: (failure: TransportFailure) => reject(new Error(failure.message)),
    });
  });
}
