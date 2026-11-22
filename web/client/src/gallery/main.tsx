/**
 * The gallery's entry (#2896): `gallery.html`, built by `vite build --mode
 * gallery` for the design-conformance suite and served by `vite` in
 * development. No production build includes it, and nothing links to it.
 *
 * @module
 */
import { render } from "solid-js/web";

import "../tokens.css";
import { Gallery } from "./Gallery";

const root = document.getElementById("root");
if (root === null) {
  throw new Error("gallery.html has no #root element");
}
render(() => <Gallery />, root);
