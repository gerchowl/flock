/**
 * The bundle's entry: reads the shell configuration and mounts the app.
 *
 * In a fixture build (`vite build --mode fixture`) `src/fixture-shell.ts`
 * runs before this module and supplies a fixture configuration; otherwise
 * the hosting shell must have set it (see `src/config.ts`).
 *
 * @module
 */
import { render } from "solid-js/web";

import { ClientWorkbench } from "./layout/Workbench";
import { previewMode } from "./layout/phone";
import { readShellConfig } from "./config";
import "./styles.css";

const root = document.getElementById("root");
if (root === null) {
  throw new Error("index.html has no #root element");
}
// G2 mount: live configuration remains with G3 #1595.
render(() => <ClientWorkbench config={readShellConfig(globalThis)} serverKey="fixture" mode={previewMode(window.location.search)} />, root);
