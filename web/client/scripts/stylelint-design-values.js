/** Resolve every possible local alias before checking design categories and layout. */
import { readFileSync } from "node:fs";
import stylelint from "stylelint";

const ruleName = "flock/design-values";
const tokens = new Map([...readFileSync(new URL("../src/tokens.css", import.meta.url), "utf8")
  .matchAll(/^\s*(--[\w-]+):\s*([^;]+);/gm)].map((match) => [match[1], match[2]]));
const globals = ["inherit", "initial", "unset", "revert", "revert-layer"];
const display = new Set(["none", "block", "inline", "inline-block", ...globals]);
const position = new Set(["static", "relative", ...globals]);
// These are supplied by the branded primitive props, never by arbitrary CSS.
const primitiveInputs = new Map([
  ["--l-space", "--space-1"], ["--l-side", "--size-control"],
  ["--l-content-min", "--size-control"], ["--l-threshold", "--size-control"],
  ["--l-max", "--size-control"], ["--l-min", "--size-control"],
  ["--l-justify", "normal"], ["--l-align", "normal"],
]);
const decode = (value) => value.replace(/\\(?:([\da-f]{1,6})[ \t\r\n\f]?|([^\r\n\f]))/gi,
  (_all, hex, char) => hex ? String.fromCodePoint(Math.min(parseInt(hex, 16) || 0xfffd, 0x10ffff)) : char);

/** Category prefixes for the token references in each property. */
function categories(prop) {
  if (/^(padding|margin)(-|$)|(^|-)gap$/.test(prop)) return ["space-"];
  if (prop === "font-size" || prop === "line-height") return ["font-size-"];
  if (prop === "font-family") return ["font-family-"];
  if (prop === "font-weight") return ["font-weight-"];
  if (/radius$/.test(prop)) return ["radius-"];
  if (prop === "z-index") return ["layer-"];
  if (/^(?:min-|max-)?(?:width|height|inline-size|block-size)$/.test(prop)) return ["size-", "space-"];
  if (/color$/.test(prop) || ["fill", "stroke", "-webkit-text-fill-color"].includes(prop)) return ["color-"];
  if (/^background/.test(prop)) return ["color-"];
  if (prop === "outline-offset") return ["border-width-", "space-"];
  if (/^(?:border|outline|column-rule|text-decoration|text-emphasis)|text-stroke/.test(prop)) {
    if (/width$/.test(prop)) return ["border-width-"];
    if (/style$/.test(prop)) return [];
    return ["border-width-", "color-"];
  }
  if (/shadow$/.test(prop) || /filter$/.test(prop)) return ["space-", "border-width-", "color-"];
  if (/^font-/.test(prop)) return [];
  return null;
}

const rule = (options) => (root, result) => {
  if (!stylelint.utils.validateOptions(result, ruleName, { actual: options, possible: [true, Object] })) return;
  const { legacy = false, layout = false } = options === true ? {} : options;
  const aliases = new Map();
  const report = (node, message) => stylelint.utils.report({ result, ruleName, node, message });
  root.walkDecls((decl) => {
    const prop = decode(decl.prop);
    if (!prop.startsWith("--")) return;
    const values = aliases.get(prop) ?? [];
    values.push(decode(decl.value));
    aliases.set(prop, values);
  });
  root.walkAtRules((at) => {
    const name = decode(at.name).toLowerCase();
    if (name === "property") report(at, "@property is disallowed outside generated tokens.css.");
    if (!layout && !legacy && ["media", "container", "supports"].includes(name)) report(at, `@${name} is disallowed outside src/layout/.`);
    if (!layout && !legacy && name === "import" && !/^(?:url\([^)]*\)|"[^"]*"|'[^']*')\s*;?$/.test(at.params.trim())) {
      report(at, "Conditional @import is disallowed outside src/layout/.");
    }
  });

  root.walkDecls((decl) => {
    const prop = decode(decl.prop).toLowerCase();
    const value = decode(decl.value).replace(/\/\*[\s\S]*?\*\//g, " ");
    const fail = (message) => report(decl, message);
    const fenced = !layout && !legacy;
    const expected = legacy ? null : categories(prop);
    if (!legacy && prop === "font" && value.trim().toLowerCase() !== "inherit") fail("font shorthand is disallowed except font: inherit.");
    if (fenced && /^(float|clear|columns|column-count|column-width|column-span|column-fill|inset(?:-.+)?|top|right|bottom|left|translate)$/.test(prop)) {
      fail(`${prop} is disallowed outside src/layout/.`);
    }

    /** Check all definitions, regardless of selectors or source order: no cascade guess. */
    function resolve(text, seen = new Set()) {
      const replaced = text.replace(/var\(\s*(--[\w-]+)\s*\)/gi, (_all, name) => {
        if (seen.has(name)) { fail(`Cyclic custom property ${name}.`); return ""; }
        if (tokens.has(name)) {
          if (!legacy && name.startsWith("--color-legacy-")) fail("Legacy colour tokens are disallowed outside LEGACY files.");
          if (expected && !expected.some((prefix) => name.startsWith(`--${prefix}`))) {
            fail(`${prop} requires ${expected.join(" or ")} tokens, received ${name}.`);
          }
          return tokens.get(name);
        }
        const definitions = aliases.get(name) ?? (layout && primitiveInputs.has(name) ? [primitiveInputs.get(name).startsWith("--") ? `var(${primitiveInputs.get(name)})` : primitiveInputs.get(name)] : []);
        if (!definitions.length) { fail(`Undefined token or local alias ${name}.`); return ""; }
        const next = new Set([...seen, name]);
        let first;
        for (const definition of definitions) {
          const resolved = resolve(definition, next);
          first ??= resolved;
          if (fenced && prop === "display" && !display.has(resolved.trim().toLowerCase())) fail("Resolved display is disallowed outside src/layout/.");
          if (fenced && prop === "position" && !position.has(resolved.trim().toLowerCase())) fail("Resolved position is disallowed outside src/layout/.");
        }
        // Each branch was checked independently. Substitution is only needed for fences.
        return first;
      });
      if (/\bvar\s*\(/i.test(replaced)) { fail("var() fallbacks and malformed references are disallowed."); }
      return replaced;
    }
    if (!legacy && /filter$/.test(prop)) {
      const raw = value.replace(/var\(\s*--[\w-]+\s*\)/gi, "")
        .replace(/(?:drop-shadow|blur)\(|\)|\b(?:none|currentcolor|inherit|initial|unset|revert|revert-layer)\b|(?<![\w.-])0(?![\w.-])/gi, "");
      if (raw.trim()) fail("Filters require tokens inside blur()/drop-shadow(), or a keyword.");
    }
    const resolved = resolve(value);
    if (fenced && prop === "display" && !display.has(resolved.trim().toLowerCase())) fail("display value is disallowed outside src/layout/.");
    if (fenced && prop === "position" && !position.has(resolved.trim().toLowerCase())) fail("position value is disallowed outside src/layout/.");
  });
};
rule.ruleName = ruleName;
rule.messages = {};
export default stylelint.createPlugin(ruleName, rule);
