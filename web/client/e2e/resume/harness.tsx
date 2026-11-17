/** Mounted browser client for resume probes; Playwright owns its wire peer and clock. */
import { render } from "solid-js/web";
import { App } from "../../src/App";
import "../../src/tokens.css";
import "../../src/styles.css";

const root = document.getElementById("root");
if (root === null) throw new Error("resume fixture root missing");
Reflect.set(globalThis, "disposeResume", render(() => <App config={{ ok: true, config: { transport: "wire", serverUrl: "https://example.invalid", token: "fixture-token" } }} />, root));
