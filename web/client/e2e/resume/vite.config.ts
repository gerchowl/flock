/** Build the mounted app used by the browser resume regression. */
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  root: fileURLToPath(new URL("../..", import.meta.url)),
  base: "./",
  plugins: [solid()],
  build: { target: "es2022", rollupOptions: { input: "e2e/resume/index.html" }, modulePreload: { polyfill: false } },
});
