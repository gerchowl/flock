import { describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";

import { absoluteUrls, buildClient } from "./build";

async function inTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = await mkdtemp(`${tmpdir()}/flock-client-dist-`);
  try {
    return await run(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

describe("built bundle", () => {
  test("absoluteUrls catches root, scheme-relative and http(s) loads", () => {
    expect(absoluteUrls('<script src="/assets/a.js"></script><link href="https://cdn/x.css">')).toHaveLength(2);
    expect(absoluteUrls("a{background:url(//cdn/x.png)} @import 'http://x/y.css';")).toHaveLength(2);
    expect(absoluteUrls('<script src="./assets/a.js"></script> "http://www.w3.org/2000/svg"')).toEqual([]);
  });

  test("a production build references its assets only by relative URL", async () => {
    await inTempDir(async (dir) => {
      const files = await buildClient("production", dir);
      expect(files).toContain("index.html");
      const html = await Bun.file(`${dir}/index.html`).text();
      expect(html).toMatch(/src="\.\/assets\/[^"]+\.js"/);
      for (const file of files.filter((name) => /\.(html|css)$/.test(name))) {
        expect({ file, absolute: absoluteUrls(await Bun.file(`${dir}/${file}`).text()) }).toEqual({ file, absolute: [] });
      }
      // The recorded session is bundled only into fixture builds.
      for (const file of files.filter((name) => name.endsWith(".js"))) {
        expect(await Bun.file(`${dir}/${file}`).text()).not.toContain("session-example");
      }
    });
  });

  test("a fixture build is one self-contained index.html", async () => {
    await inTempDir(async (dir) => {
      expect(await buildClient("fixture", dir)).toEqual(["index.html"]);
      const html = await Bun.file(`${dir}/index.html`).text();
      expect(absoluteUrls(html)).toEqual([]);
      expect(html).not.toMatch(/<script[^>]*\bsrc=|<link[^>]*\bstylesheet/);
      expect(html).toContain("session-example");
    });
  });
});
