import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import type { Page } from "playwright";

import { capturePreview } from "../e2e/capture";

/** A controlled browser peer: only the test can publish a compositor frame. */
class Preview extends EventEmitter {
  captures = 0;
  stops = 0;
  frame?: NonNullable<Parameters<Page["screencast"]["start"]>[0]>["onFrame"];
  viewportSize = () => ({ width: 390, height: 844 });
  screencast = {
    start: async (options: NonNullable<Parameters<Page["screencast"]["start"]>[0]>) => {
      this.frame = options.onFrame;
      return { async [Symbol.asyncDispose]() {} };
    },
    stop: async () => { this.stops++; },
  };
  screenshot = async () => { this.captures++; return Buffer.from("png fixture"); };
  page = () => this as unknown as Page;
  paint(width = 390, data = Buffer.from("encoded compositor frame")) {
    this.frame?.({ data, timestamp: 1, viewportWidth: width, viewportHeight: 844 });
  }
}

describe("preview compositor capture", () => {
  test("does not request a screenshot before the compositor frame, then captures once", async () => {
    const peer = new Preview();
    const capture = capturePreview(peer.page(), "unused.png");
    await Promise.resolve();
    expect(peer.captures).toBe(0);
    peer.paint();
    expect(await capture).toEqual(Buffer.from("png fixture"));
    expect(peer.captures).toBe(1);
    expect(peer.stops).toBe(1);
    expect(peer.listenerCount("close") + peer.listenerCount("crash")).toBe(0);
  });

  for (const event of ["close", "crash"] as const) {
    test(`page ${event} before painting fails without capture`, async () => {
      const peer = new Preview();
      const capture = capturePreview(peer.page(), "unused.png");
      peer.emit(event);
      await expect(capture).rejects.toThrow(event);
      expect(peer.captures).toBe(0);
      expect(peer.stops).toBe(1);
    });
  }

  test("an alive but silent compositor fails at the injected deadline", async () => {
    const peer = new Preview();
    const deadline = new AbortController();
    const capture = capturePreview(peer.page(), "unused.png", deadline.signal);
    deadline.abort(new Error("compositor deadline"));
    await expect(capture).rejects.toThrow("compositor deadline");
    expect(peer.captures).toBe(0);
    expect(peer.stops).toBe(1);
  });

  for (const invalid of ["dimensions", "empty"] as const) {
    test(`a frame with ${invalid} fails instead of waiting for another frame`, async () => {
      const peer = new Preview();
      const capture = capturePreview(peer.page(), "unused.png");
      peer.paint(invalid === "dimensions" ? 1280 : 390, invalid === "empty" ? Buffer.alloc(0) : undefined);
      await expect(capture).rejects.toThrow("compositor frame");
      expect(peer.captures).toBe(0);
    });
  }
});
