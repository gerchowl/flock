/**
 * Unbuilt controls (ADR-0038 §2a, as amended by #2900): hidden in production,
 * visible only with `?preview`, where each shows disabled with what it waits
 * for. A control disabled for a runtime reason the server reports is not
 * one of these and shows either way.
 *
 * @module
 */

/** Whether the page's query string asks for the unbuilt controls (`?preview`, any value). */
export function previewControls(search: string): boolean {
  return new URLSearchParams(search).has("preview");
}
