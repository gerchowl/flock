/**
 * ESLint for the client's TSX (#2895; web/DESIGN.md "Rules"). It enforces
 * one rule: no inline `style` outside `src/layout/`. Inline styles bypass
 * the token and layout checks stylelint runs on CSS files, so only the
 * layout primitives may set them (as custom properties on their own
 * element). Type checking stays with `tsc`.
 *
 * Babel parses the TSX: typescript-eslint needs TypeScript's JavaScript
 * API, which TypeScript 7 no longer ships.
 */
import babelParser from "@babel/eslint-parser";

const NO_STYLE = "Inline style bypasses the design tokens: use a class, or a primitive from src/layout/ (web/DESIGN.md).";

export default [
  {
    files: ["src/**/*.tsx"],
    languageOptions: {
      parser: babelParser,
      parserOptions: {
        requireConfigFile: false,
        babelOptions: { babelrc: false, configFile: false, presets: [["@babel/preset-typescript", { isTSX: true, allExtensions: true }]] },
      },
    },
    rules: {
      "no-restricted-syntax": [
        "error",
        // style={…}, and Solid's namespaced forms such as attr:style and prop:style.
        { selector: "JSXAttribute[name.name='style']", message: NO_STYLE },
        { selector: "JSXAttribute > JSXNamespacedName[name.name='style']", message: NO_STYLE },
      ],
    },
  },
  {
    files: ["src/layout/**/*.tsx"],
    rules: { "no-restricted-syntax": "off" },
  },
];
