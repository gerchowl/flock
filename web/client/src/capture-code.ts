/**
 * Takes the one-time sign-in code out of the fragment before any other
 * module runs or any request goes out (ADR-0038 §3). `src/main.tsx`
 * imports this module first, and ES modules evaluate in import order. A
 * code that arrives later, in a pasted link that changes only the
 * fragment, is taken the same way on `hashchange` (`src/App.tsx`).
 *
 * @module
 */
import { takeCode, type CodeHistory, type CodeLocation } from "./wire/token";

/** The parts of the page the code is taken from. */
export type CodePage = { readonly location: CodeLocation; readonly history: CodeHistory };

/** Takes the code out of `page`'s fragment (see {@link takeCode}). */
export function captureCode(page: CodePage): string | null {
  return takeCode(page.location, page.history);
}

/** The code `flk client open` put in the fragment, already removed from it, or null. */
export const capturedCode: string | null = typeof window === "undefined" ? null : captureCode(window);
