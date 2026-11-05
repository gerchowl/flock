/** Serializes native writes and coalesces pending counts to the latest value. */
import type { SetBadgeCount } from "../config";

/** Disposal must enqueue zero through this same writer, after any in-flight write. */
export function createBadgeWriter(set: SetBadgeCount | undefined): (count: number) => void {
  let pending: number | undefined;
  let writing = false;
  const drain = async (): Promise<void> => {
    writing = true;
    while (pending !== undefined) {
      const value = pending;
      pending = undefined;
      try {
        await set?.(value);
      } catch (error: unknown) {
        console.error("Could not update the app badge", error);
      }
    }
    writing = false;
  };
  return (count) => {
    pending = count;
    if (!writing) void drain();
  };
}
