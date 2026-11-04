/**
 * The client's CSS rules (#2895; web/DESIGN.md "Rules").
 *
 * 1. Strict values: the properties in STRICT take a design token
 *    (`var(--…)`) or a CSS keyword, never a raw value. Raw values live only
 *    in the generated `src/tokens.css`. Custom properties may only alias
 *    tokens, so a raw value cannot hide in a local `--x: 13px` (the plugin
 *    skips custom properties, so a core rule covers them).
 * 2. Layout fencing: display/position allowlists and structural properties
 *    keep layout in src/layout/. The design-values rule resolves aliases,
 *    validates categories, and rejects fallbacks, unknown names, legacy
 *    colours in new CSS and @property outside generated tokens.
 *    Everything else composes those primitives.
 * 5. Reserved token names: only the generated `src/tokens.css` declares a
 *    custom property in a token category (`--color-*`, `--space-*`, …), so
 *    no component can rebind a token (`flock/no-token-redeclaration`,
 *    scripts/stylelint-reserved-tokens.js). It holds in LEGACY files too.
 *
 * LEGACY is the temporary ignore list: files written before the tokens.
 * They are still held to strict colours, but not yet to the other strict
 * values or the layout fence. Shrink it file by file; never add to it.
 */

/** Temporary ignore list (#2895 follow-up): remove a file once it passes the full rules. */
const LEGACY = ["src/styles.css", "src/workbench/workbench.css"];

const STRICT = [
  "/color$/",
  "/^background/",
  "/^border/",
  "/^font/",
  "line-height",
  "box-shadow",
  "text-shadow",
  "fill",
  "stroke",
  "/^-webkit-text-(stroke|fill)/",
  "z-index",
  "/gap$/",
  "/^margin/",
  "/^padding/",
];

/** Colour properties, enforced in LEGACY files too. */
const COLOR = ["/color$/", "background-color", "fill", "stroke"];

/** Keywords a strict property may take instead of a token. */
const KEYWORDS = [
  "0",
  "auto",
  "currentColor",
  "currentcolor",
  "inherit",
  "initial",
  "none",
  "normal",
  "revert",
  "transparent",
  "unset",
  "solid",
  "dashed",
  "italic",
  "underline",
];

const strict = (properties) => [
  properties,
  {
    ignoreFunctions: false,
    ignoreValues: KEYWORDS,
    expandShorthand: true,
    recurseLonghand: true,
    disableFix: true,
  },
];

const FENCE = {
  "property-disallowed-list": ["/gap$/", "/^margin/"],
};
const UNFENCED = Object.fromEntries(Object.keys(FENCE).map((rule) => [rule, null]));

/** A custom property holds one or more `var(--token)` references, or a keyword. */
const ALIAS = {
  "declaration-property-value-allowed-list": { "/^--/": ["/^(?:var\\(--[a-z0-9-]+\\)\\s*)+$/", ...KEYWORDS] },
};

export default {
  plugins: ["stylelint-declaration-strict-value", "./scripts/stylelint-reserved-tokens.js", "./scripts/stylelint-design-values.js"],
  rules: {
    "scale-unlimited/declaration-strict-value": strict(STRICT),
    "flock/no-token-redeclaration": true,
    "flock/design-values": true,
    ...ALIAS,
    ...FENCE,
  },
  overrides: [
    { files: LEGACY, rules: { "scale-unlimited/declaration-strict-value": strict(COLOR), "flock/design-values": { legacy: true }, ...UNFENCED } },
    { files: ["src/layout/**/*.css"], rules: { ...UNFENCED, "flock/design-values": { layout: true } } },
    {
      files: ["src/tokens.css"],
      rules: {
        "scale-unlimited/declaration-strict-value": null,
        "declaration-property-value-allowed-list": null,
        "flock/no-token-redeclaration": null,
        "flock/design-values": null,
        ...UNFENCED,
      },
    },
  ],
};
