/**
 * `bun test` preload (bunfig.toml): a DOM, and Solid's JSX compiler.
 *
 * Bun's own JSX transform targets React's runtime; Solid needs
 * `babel-preset-solid`, the transform `vite-plugin-solid` runs in builds,
 * so `.tsx` files go through the same preset here. Bun also resolves
 * packages with its server conditions, which select Solid's server build
 * (no reactivity, no `render`). bunfig has no setting for export
 * conditions, and runtime plugins cannot redirect bare specifiers, so each
 * of Solid's non-browser builds loads its `browser` build's code instead,
 * as `bun test --conditions browser` would.
 */
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { transformAsync } from "@babel/core";
import { plugin } from "bun";
import { resolve } from "node:path";

GlobalRegistrator.register();

type ExportConditions = { readonly [condition: string]: { readonly import?: string } | string };

/** Maps each non-browser build of a Solid entry point to its browser build. */
async function solidBrowserRedirects(): Promise<Map<string, string>> {
  const root = `${import.meta.dir}/../node_modules/solid-js`;
  const exportsMap: Record<string, ExportConditions> = (await Bun.file(`${root}/package.json`).json()).exports;
  const redirects = new Map<string, string>();
  for (const conditions of Object.values(exportsMap)) {
    const browser = conditions["browser"];
    if (typeof browser !== "object" || browser.import === undefined) continue;
    for (const [condition, entry] of Object.entries(conditions)) {
      if (condition !== "browser" && typeof entry === "object" && entry.import !== undefined && entry.import !== browser.import) {
        redirects.set(resolve(root, entry.import), resolve(root, browser.import));
      }
    }
  }
  if (redirects.size === 0) {
    throw new Error("solid-js exports no browser builds to redirect to");
  }
  return redirects;
}

const solidRedirects = await solidBrowserRedirects();
const solidServerBuilds = new RegExp(`^(${[...solidRedirects.keys()].map((path) => path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})$`);

plugin({
  name: "solid-jsx",
  setup(build) {
    build.onLoad({ filter: solidServerBuilds }, async ({ path }) => {
      const browser = solidRedirects.get(path);
      if (browser === undefined) {
        throw new Error(`no browser build recorded for ${path}`);
      }
      return { contents: await Bun.file(browser).text(), loader: "js" };
    });
    build.onLoad({ filter: /\.tsx$/ }, async ({ path }) => {
      const source = await Bun.file(path).text();
      const result = await transformAsync(source, {
        filename: path,
        // Presets resolve by name from the client's own node_modules.
        cwd: `${import.meta.dir}/..`,
        babelrc: false,
        configFile: false,
        presets: [["babel-preset-solid", { generate: "dom" }], "@babel/preset-typescript"],
      });
      if (result?.code == null) {
        throw new Error(`Solid transform produced no code for ${path}`);
      }
      return { contents: result.code, loader: "js" };
    });
  },
});
