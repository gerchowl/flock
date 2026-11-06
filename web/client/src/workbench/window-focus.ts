/**
 * Whether this window is the active one (#2903). Selections are emphasised
 * (the accent) only in the active window; in an inactive one they are
 * unemphasised, as macOS draws them (HIG; web/DESIGN.md "Colour"). The
 * workbench writes the answer as `data-window="active"` or `"inactive"`,
 * which the stylesheets select on.
 *
 * @module
 */
import { createSignal, onCleanup } from "solid-js";

/** A signal that follows the window's focus, starting from `document.hasFocus()`; listeners end with the owner. */
export function createWindowActive(): () => boolean {
  const [active, setActive] = createSignal(document.hasFocus());
  const focus = (): void => void setActive(true);
  const blur = (): void => void setActive(false);
  window.addEventListener("focus", focus);
  window.addEventListener("blur", blur);
  onCleanup(() => {
    window.removeEventListener("focus", focus);
    window.removeEventListener("blur", blur);
  });
  return active;
}
