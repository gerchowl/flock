/** Test helper: a Vite build of the client into a directory of its own. */
import { readdir } from "node:fs/promises";

import { build } from "vite";

/** The client's root directory. */
export const CLIENT_ROOT = `${import.meta.dir}/..`;

/** Builds the client in `mode` into `outDir` and lists the emitted files. */
export async function buildClient(mode: "production" | "fixture", outDir: string): Promise<string[]> {
  await build({ root: CLIENT_ROOT, mode, logLevel: "error", build: { outDir, emptyOutDir: true } });
  const files = await readdir(outDir, { recursive: true, withFileTypes: true });
  return files.filter((file) => file.isFile()).map((file) => `${file.parentPath}/${file.name}`.slice(outDir.length + 1));
}

/**
 * Every URL the page would load at parse time that is absolute: a root
 * path (`/x`), a scheme-relative one (`//x`) or an `http(s):` URL, in an
 * HTML `src`/`href` attribute or a CSS `url(...)`/`@import`. Namespace
 * strings inside scripts are not loads and are not matched.
 */
export function absoluteUrls(text: string): string[] {
  const pattern = /(?:\b(?:src|href)\s*=\s*["']?|url\(\s*["']?|@import\s+["'])(\/|https?:)[^"')\s>]*/gi;
  return [...text.matchAll(pattern)].map((match) => match[0]);
}
