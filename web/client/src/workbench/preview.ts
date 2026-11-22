/**
 * Unbuilt controls (ADR-0038 §2a, as amended by ADR-0048 D7): hidden in
 * production builds, visible only in development builds, where each shows
 * disabled with what it waits for. A control disabled for a runtime reason
 * the server reports is not one of these and shows either way.
 *
 * @module
 */

/** How this bundle was built: Vite replaces both flags in builds; under `bun test` neither is set. */
export type Build = { readonly dev: boolean; readonly production: boolean };

const BUILD: Build = { dev: import.meta.env.DEV === true, production: import.meta.env.PROD === true };

/**
 * Whether to show the unbuilt controls: always in a development build
 * (the Vite dev server), with `?preview` (any value) in a test build, and
 * never in a production build, whatever the query string says.
 */
export function previewControls(search: string, build: Build = BUILD): boolean {
  if (build.production) return false;
  return build.dev || new URLSearchParams(search).has("preview");
}
