/** Capture boundary for the owned Chromium fixture preview (#2522). */
import type { Page } from "playwright";

// The existing Bun browser-test ceiling is five seconds. The same deadline
// bounds frame readiness, the single screenshot, and screencast cleanup.
const CAPTURE_LIMIT_MS = 5_000;

/**
 * Saves one PNG after Chromium supplies a non-empty compositor frame at the
 * requested viewport. DOM visibility and font readiness do not prove a surface
 * exists: Chromium's Page.captureScreenshot rejects an empty image copy.
 * The first frame is a readiness signal, not the artifact or a screenshot retry.
 */
export async function capturePreview(
  page: Page,
  path: string,
  signal = AbortSignal.timeout(CAPTURE_LIMIT_MS),
): Promise<Buffer> {
  const viewport = page.viewportSize();
  if (!viewport) throw new Error("preview capture requires a fixed viewport");
  let painted!: () => void;
  const ready = new Promise<void>((resolve) => { painted = resolve; });
  let reject!: (error: Error) => void;
  const failed = new Promise<never>((_, failure) => { reject = failure; });
  let failure: Error | undefined;
  let finished = false;
  const fail = (error: Error) => { failure ??= error; reject(failure); };
  const close = () => fail(new Error("preview page close before capture completed"));
  const crash = () => fail(new Error("preview page crash before capture completed"));
  const abort = () => fail(signal.reason instanceof Error ? signal.reason : new Error("preview compositor deadline"));
  page.on("close", close);
  page.on("crash", crash);
  signal.addEventListener("abort", abort, { once: true });
  if (signal.aborted) abort();
  try {
    return await Promise.race([
      (async () => {
        await page.screencast.start({
          size: viewport,
          onFrame: ({ data, viewportWidth, viewportHeight }) => {
            if (finished || failure) return;
            if (!data.length || viewportWidth !== viewport.width || viewportHeight !== viewport.height) {
              fail(new Error("unexpected preview compositor frame: empty data or wrong viewport"));
              return;
            }
            painted();
          },
        });
        await ready;
        if (failure) throw failure;
        return page.screenshot({ path, timeout: CAPTURE_LIMIT_MS });
      })(),
      failed,
    ]);
  } finally {
    finished = true;
    try {
      await Promise.race([page.screencast.stop(), failed]);
    } finally {
      page.off("close", close);
      page.off("crash", crash);
      signal.removeEventListener("abort", abort);
    }
  }
}
