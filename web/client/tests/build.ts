/**
 * Test helper: a Vite build of the client into a directory of its own.
 *
 * The build runs as a child process: the test process carries happy-dom's
 * globals (tests/setup.ts), and Vite's Node API must not load into it.
 */
import { readdir } from "node:fs/promises";

/** The client's root directory. */
export const CLIENT_ROOT = `${import.meta.dir}/..`;

/** Builds the client in `mode` into `outDir` and lists the emitted files. */
export async function buildClient(mode: "production" | "fixture", outDir: string): Promise<string[]> {
  const child = Bun.spawn(
    [process.execPath, "x", "vite", "build", "--mode", mode, "--outDir", outDir, "--emptyOutDir", "--logLevel", "error"],
    { cwd: CLIENT_ROOT, stdout: "pipe", stderr: "pipe" },
  );
  const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
  if (code !== 0) {
    throw new Error(`vite build --mode ${mode} exited ${code}: ${stderr}`);
  }
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
