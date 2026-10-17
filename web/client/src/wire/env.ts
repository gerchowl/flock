/**
 * What the wire connection needs from its host: a clock, sockets, randomness
 * and storage (ADR-0038 §3, ADR-0037). Every one is injected, so tests drive
 * the whole lifecycle with a virtual clock and scripted sockets, and no test
 * waits on wall-clock time.
 *
 * @module
 */

/** An opaque timer handle from {@link Clock.setTimeout}. */
export type TimerHandle = { readonly __timer: unique symbol };

/**
 * Monotonic time for deadlines, and wall time for persisted stamps.
 * `now` must never go backwards; `wallNow` is epoch milliseconds.
 */
export interface Clock {
  now(): number;
  wallNow(): number;
  setTimeout(callback: () => void, ms: number): TimerHandle;
  clearTimeout(handle: TimerHandle): void;
}

/** The browser's clock: `performance.now()` and the global timers. */
export const browserClock: Clock = {
  now: () => performance.now(),
  wallNow: () => Date.now(),
  setTimeout: (callback, ms) => globalThis.setTimeout(callback, ms) as unknown as TimerHandle,
  clearTimeout: (handle) => globalThis.clearTimeout(handle as unknown as ReturnType<typeof globalThis.setTimeout>),
};

/**
 * The part of a browser `WebSocket` the connection uses. The connection
 * assigns the four handlers once, right after creating the socket.
 */
export interface SocketLike {
  send(text: string): void;
  close(code: number, reason: string): void;
  onopen: (() => void) | null;
  onmessage: ((event: { readonly data: unknown }) => void) | null;
  onclose: ((event: { readonly code: number; readonly reason: string }) => void) | null;
  onerror: (() => void) | null;
}

/** Opens one socket per connection attempt. */
export type SocketFactory = (url: string) => SocketLike;

/**
 * The browser's `WebSocket`. Browsers decide `permessage-deflate` themselves;
 * the server declines it on upgrade (ADR-0036 §5a), so the `hello` carrying
 * the token is never compressed.
 */
export const browserSockets: SocketFactory = (url) => new WebSocket(url) as unknown as SocketLike;

/** String storage for the outbox. `localStorage` satisfies it. */
export interface KeyValueStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** An in-memory {@link KeyValueStore}, for tests and for hosts without storage. */
export function memoryStore(): KeyValueStore & { readonly entries: Map<string, string> } {
  const entries = new Map<string, string>();
  return {
    entries,
    getItem: (key) => entries.get(key) ?? null,
    setItem: (key, value) => void entries.set(key, value),
  };
}

/**
 * The `/v1/ws` endpoint for a shell's `serverUrl` (ADR-0036 §5). The path is
 * resolved against the configured URL, never against the page's origin, and
 * `http:`/`https:` map to `ws:`/`wss:`. The token never goes into the URL.
 */
export function endpointUrl(serverUrl: string): string {
  const url = new URL("/v1/ws", serverUrl);
  if (url.protocol === "http:") url.protocol = "ws:";
  if (url.protocol === "https:") url.protocol = "wss:";
  return url.href;
}

/**
 * Reconnect backoff for attempt `n` (from 0): an integer in `[c/2, c]` with
 * `c = min(30 s, 500 ms × 2ⁿ)`, using `random` in `[0, 1)` (ADR-0036 §5a).
 */
export function backoffDelay(n: number, random: () => number): number {
  const ceiling = Math.min(30_000, 500 * 2 ** Math.min(n, 16));
  return Math.floor(ceiling / 2 + random() * (ceiling / 2 + 1));
}

/** A UUIDv7 (RFC 9562): 48 bits of `wallMs`, then random bits. */
export function uuidv7(wallMs: number, random: Uint8Array = crypto.getRandomValues(new Uint8Array(10))): string {
  const bytes = new Uint8Array(16);
  let ms = Math.floor(wallMs);
  for (let index = 5; index >= 0; index--) {
    bytes[index] = ms % 256;
    ms = Math.floor(ms / 256);
  }
  bytes.set(random.subarray(0, 10), 6);
  bytes[6] = 0x70 | ((bytes[6] ?? 0) & 0x0f);
  bytes[8] = 0x80 | ((bytes[8] ?? 0) & 0x3f);
  const hex = [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
