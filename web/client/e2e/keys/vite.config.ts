/** Builds the keyboard tests' page (`e2e/keys/index.html`) from the client's sources; tests only. */
import { fileURLToPath } from "node:url";

import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  root: fileURLToPath(new URL("../..", import.meta.url)),
  base: "./",
  plugins: [solid()],
  build: { target: "es2022", rollupOptions: { input: "e2e/keys/index.html" }, modulePreload: { polyfill: false } },
});
