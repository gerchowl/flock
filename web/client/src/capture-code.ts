/**
 * Takes the one-time sign-in code out of the fragment before any other
 * module runs or any request goes out (ADR-0038 §3). `src/main.tsx`
 * imports this module first, and ES modules evaluate in import order.
 *
 * @module
 */
import { takeCode } from "./wire/token";

/** The code `flk client open` put in the fragment, already removed from it, or null. */
export const capturedCode: string | null = typeof window === "undefined" ? null : takeCode(window.location, window.history);
