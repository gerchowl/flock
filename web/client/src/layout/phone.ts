/** Below 768 CSS pixels the workbench uses a single tile and a bottom-sheet rail. */
export const PHONE_BREAKPOINT = 768;
/** Shell or test override; auto follows viewport width. */
export type Mode = "auto" | "phone" | "desktop";
/** Explicit props override viewport width in both directions. */
export function isPhone(mode: Mode, width: number): boolean {
  return mode === "phone" || (mode === "auto" && width < PHONE_BREAKPOINT);
}
/** Owner preview override; unknown values use automatic detection. */
export function previewMode(search: string): Mode {
  const mode = new URLSearchParams(search).get("layout");
  return mode === "phone" || mode === "desktop" ? mode : "auto";
}
