// @ts-expect-error The internal handoff symbol is not exported to UI modules.
import { providerSessionHandoff } from "../src/wire/connection";
import type { WireConnection } from "../src/wire/connection";

// The UI's connection handle cannot bypass the user Connect event.
function uiConnection(connection: WireConnection): void {
  // @ts-expect-error Session replacement is private to the provider handoff.
  connection.replaceSession(() => {});
}
void uiConnection;

void providerSessionHandoff;
