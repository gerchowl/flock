/** Fixed widths keep the canvas useful while giving long names or inbox cards more room. */
export const PRESETS = ["narrow", "default", "wide"] as const;
export type SplitPreset = typeof PRESETS[number];
export type SplitState = { readonly preset: SplitPreset; readonly collapsed: boolean };
export type SplitRegions = { readonly sidebar: SplitState; readonly inspector: SplitState };
export const DEFAULT_SPLIT: SplitState = { preset: "default", collapsed: false };

/** Unknown future fields survive saves; invalid regions fall back independently without losing tiles. */
export function splitState(value: unknown): SplitState {
  if (value === null || typeof value !== "object") return DEFAULT_SPLIT;
  const raw = value as Partial<SplitState>;
  return { ...value, preset: PRESETS.find(preset => preset === raw.preset) ?? "default", collapsed: raw.collapsed === true };
}

/** The Compact canvas starts below 1024 px; the static media query in split.css uses this boundary. */
export const isCompact = (width: number): boolean => width < 1024;
