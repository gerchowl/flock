/** Serializes native writes and coalesces pending counts to the latest value. */
import type { SetBadgeCount } from "../config";

/** Disposal must enqueue zero through this same writer, after any in-flight write. */
export function createBadgeWriter(set: SetBadgeCount | undefined): (count: number, label?: string | null) => void {
  let pending: { readonly count: number; readonly label: string | null } | undefined;
  let writing = false;
  const drain = async (): Promise<void> => {
    writing = true;
    while (pending !== undefined) {
      const value = pending;
      pending = undefined;
      try {
        await set?.(value.count, value.label);
      } catch (error: unknown) {
        console.error("Could not update the app badge", error);
      }
    }
    writing = false;
  };
  return (count, label = null) => {
    pending = { count, label };
    if (!writing) void drain();
  };
}
