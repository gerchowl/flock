/** Browser resume signals belong to the mounted client, with matching cleanup. */
import type { WireConnection } from "./connection";

/** Probe only restores and hidden-to-visible transitions; duplicate signals coalesce in the connection. */
export function watchResume(connection: WireConnection, page: Window = window, doc: Document = document): () => void {
  let hidden = doc.visibilityState === "hidden";
  const pageshow = (event: PageTransitionEvent): void => {
    if (event.persisted) connection.resume();
  };
  const visibility = (): void => {
    if (doc.visibilityState === "hidden") hidden = true;
    else if (doc.visibilityState === "visible" && hidden) {
      hidden = false;
      connection.resume();
    }
  };
  page.addEventListener("pageshow", pageshow);
  doc.addEventListener("visibilitychange", visibility);
  return () => {
    page.removeEventListener("pageshow", pageshow);
    doc.removeEventListener("visibilitychange", visibility);
  };
}
