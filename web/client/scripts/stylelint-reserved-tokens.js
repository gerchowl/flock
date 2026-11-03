/**
 * `flock/no-token-redeclaration` (#2933; web/DESIGN.md "Rules", rule 5):
 * only the generated `src/tokens.css` may declare a custom property in a
 * token category (`--color-*`, `--space-*`, `--font-*`, …), as a
 * declaration or an `@property`. A component that rebinds
 * `--color-label-primary` would change what renders while the tested token
 * values stay untouched.
 *
 * The categories are the first segment of every name in `src/tokens.css`,
 * so a new category is reserved as soon as it is generated. Names are
 * compared after resolving CSS escapes (`--colo\72-…` is `--color-…`).
 */
import { readFileSync } from "node:fs";

import stylelint from "stylelint";

const ruleName = "flock/no-token-redeclaration";

const TOKENS_CSS = readFileSync(new URL("../src/tokens.css", import.meta.url), "utf8");
const CATEGORIES = [...new Set([...TOKENS_CSS.matchAll(/^\s*--([a-z]+)-/gm)].map((match) => match[1]))];
if (CATEGORIES.length === 0) throw new Error(`${ruleName}: src/tokens.css declares no tokens`);
const RESERVED = new RegExp(`^--(?:${CATEGORIES.join("|")})-`);

const messages = stylelint.utils.ruleMessages(ruleName, {
  rejected: (name) => `${name} is a design token; only the generated src/tokens.css declares it. Use a name of your own (web/DESIGN.md "Rules").`,
});

/** Resolves CSS escapes: `\72 ` and `\r` both mean `r`. */
const unescape = (name) =>
  name.replace(/\\(?:([0-9a-fA-F]{1,6})[ \t\n\r\f]?|(.))/gs, (_all, hex, char) => (hex === undefined ? char : String.fromCodePoint(parseInt(hex, 16))));

const rule = (primary) => (root, result) => {
  if (!stylelint.utils.validateOptions(result, ruleName, { actual: primary, possible: [true] })) return;
  const check = (name, node) => {
    if (RESERVED.test(unescape(name))) {
      stylelint.utils.report({ result, ruleName, message: messages.rejected(name), node, word: name });
    }
  };
  root.walkDecls((decl) => check(decl.prop, decl));
  root.walkAtRules(/^property$/i, (atRule) => check(atRule.params.trim(), atRule));
};

rule.ruleName = ruleName;
rule.messages = messages;
rule.meta = { url: "https://github.com/gerchowl/flock/blob/dev/web/DESIGN.md#rules" };

export default stylelint.createPlugin(ruleName, rule);
