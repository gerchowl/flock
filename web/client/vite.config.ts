/**
 * Vite build for the flock client (ADR-0038 §6, ADR-0036 §7).
 *
 * - `base: "./"`: every emitted URL is relative, so the same `dist/` loads
 *   from `flk`'s HTTP server, from the Tauri shell and from a static host.
 * - `--mode fixture` (`just client-preview`) adds two plugins:
 *   `fixtureShell` loads `src/fixture-shell.ts` ahead of the entry, and
 *   `inlineFixtureBundle` inlines the script and stylesheet into
 *   `index.html`. Browsers refuse module scripts from `file:` URLs, so the
 *   fixture page is one self-contained file that opens without any server.
 *   Production builds keep separate files, because the Tauri shell's
 *   `default-src 'self'` policy refuses inline scripts.
 * - `vite preview` binds to the loopback address unless `--host` is given.
 *
 * @module
 */
import { fileURLToPath } from "node:url";

import { defineConfig, type Plugin } from "vite";
import solid from "vite-plugin-solid";

const repoRoot = fileURLToPath(new URL("../..", import.meta.url));

/** Loads the fixture shell before the entry module in fixture mode. */
function fixtureShell(): Plugin {
  return {
    name: "flock-fixture-shell",
    transformIndexHtml: {
      order: "pre",
      handler: (html) => {
        const entry = '<script type="module" src="./src/main.tsx"></script>';
        if (!html.includes(entry)) {
          throw new Error("fixture build: index.html no longer loads ./src/main.tsx");
        }
        return html.replace(
          entry,
          () => `<script type="module" src="./src/fixture-shell.ts"></script>\n    ${entry}`,
        );
      },
    },
  };
}

/** Replaces the one tag matching `pattern`, and fails if there is none. */
function replaceTag(html: string, pattern: RegExp, replacement: string): string {
  if (!pattern.test(html)) {
    throw new Error(`fixture build: no tag matches ${pattern}`);
  }
  return html.replace(pattern, () => replacement);
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Inlines every emitted chunk and stylesheet into `index.html`. */
function inlineFixtureBundle(): Plugin {
  return {
    name: "flock-inline-fixture-bundle",
    enforce: "post",
    generateBundle(_options, bundle) {
      const page = bundle["index.html"];
      if (page?.type !== "asset" || typeof page.source !== "string") {
        throw new Error("fixture build: index.html was not emitted");
      }
      let html = page.source;
      for (const [fileName, output] of Object.entries(bundle)) {
        if (fileName === "index.html") {
          continue;
        }
        const ref = escapeRegExp(`./${fileName}`);
        if (output.type === "chunk") {
          const code = output.code.replaceAll("</script", "<\\/script");
          html = replaceTag(
            html,
            new RegExp(`<script type="module"[^>]*src="${ref}"[^>]*></script>`),
            `<script type="module">${code}</script>`,
          );
        } else if (fileName.endsWith(".css") && typeof output.source === "string") {
          const css = output.source.replaceAll("</style", "<\\/style");
          html = replaceTag(html, new RegExp(`<link rel="stylesheet"[^>]*href="${ref}"[^>]*>`), `<style>${css}</style>`);
        } else {
          throw new Error(`fixture build: cannot inline ${fileName}`);
        }
        delete bundle[fileName];
      }
      page.source = html;
    },
  };
}

export default defineConfig(({ mode }) => ({
  base: "./",
  plugins: [solid(), ...(mode === "fixture" ? [fixtureShell(), inlineFixtureBundle()] : [])],
  build: {
    target: "es2022",
    // The preload polyfill would add an inline helper; nothing here is lazy.
    modulePreload: { polyfill: false },
  },
  // The dev server may read the recorded fixtures outside the client tree.
  server: { host: "127.0.0.1", fs: { allow: [repoRoot] } },
  preview: { host: "127.0.0.1" },
}));
