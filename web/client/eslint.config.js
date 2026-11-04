/** Source and runtime design fences (#2933, web/DESIGN.md). */
import babelParser from "@babel/eslint-parser";
import { dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";
const client = dirname(fileURLToPath(import.meta.url));

const NO_STYLE = "Inline style bypasses the design tokens: use a class, or a primitive from src/layout/ (web/DESIGN.md).";
const NO_MARKUP = "HTML markup injection is disallowed; construct nodes and use textContent (web/DESIGN.md).";
const markupProp = (key) => /^(?:(?:prop|attr):)?(?:innerHTML|outerHTML)$/.test(key);
const name = (node) => node?.name ?? node?.value;
const designProps = new Set(["space", "gutters", "justify", "align", "side", "sideWidth", "contentMin", "threshold", "max", "min", "intrinsic", "as"]);

const design = {
  meta: { schema: [] },
  create(context) {
    const path = relative(client, context.filename).replaceAll("\\", "/");
    const layout = path.startsWith("src/layout/primitives/");
    const generated = path === "src/tokens.ts";
    const typeParameters = new Set();
    const designValues = [];
    const designTypes = new Set(["SpaceStep", "Token"]);
    function designReference(node) {
      if (!node) return false;
      if (node.type === "TSTypeReference" && designTypes.has(name(node.typeName.right ?? node.typeName))) return true;
      if (node.type === "TSIndexedAccessType" && designProps.has(name(node.indexType.literal))) return true;
      return (context.sourceCode.visitorKeys[node.type] ?? []).some((key) => {
        const child = node[key];
        return Array.isArray(child) ? child.some(designReference) : designReference(child);
      });
    }
    function forbiddenType(node, root = true) {
      if (!node) return false;
      if (node.type === "TSParenthesizedType") return forbiddenType(node.typeAnnotation, root);
      if (node.type === "TSLiteralType") return root && (typeof node.literal.value === "string" || node.literal.type === "TemplateLiteral");
      if (node.type === "TSTypeQuery") return true;
      if (node.type === "TSTypeReference" && typeParameters.has(name(node.typeName))) return true;
      return designReference(node) || (context.sourceCode.visitorKeys[node.type] ?? []).some((key) => {
        const child = node[key];
        return Array.isArray(child) ? child.some((node) => forbiddenType(node, false)) : forbiddenType(child, false);
      });
    }
    function staticKey(node) {
      if (node?.type === "BinaryExpression" && node.operator === "+") return staticKey(node.left) + staticKey(node.right);
      if (node?.type === "TemplateLiteral" && !node.expressions.length) return node.quasis[0].value.cooked;
      return name(node);
    }
    function tokenWrite(node, key) {
      if (/^--(?:color|space|font|size|radius|border|layer)-/.test(staticKey(key))) {
        context.report({ node, message: "Reserved design tokens may only be declared in tokens.css; primitives may only set local properties." });
      }
    }
    const report = (node, message = NO_STYLE) => context.report({ node, message });
    function paint(node, key, value) {
      if (!["fill", "stroke", "color"].includes(key)) return;
      const literal = value?.type === "JSXExpressionContainer" ? value.expression : value;
      if (literal?.type === "Literal" && /^(?:none|currentColor|transparent|inherit|initial|unset|revert|revert-layer)$/i.test(literal.value)) return;
      report(node, "JSX paint attributes require a static keyword; use a CSS class for token colours.");
    }
    return {
      Program(node) {
        for (const item of node.body) if (item.type === "ImportDeclaration") {
          for (const specifier of item.specifiers) if (designTypes.has(name(specifier.imported))) designTypes.add(specifier.local.name);
        }
        for (let pass = 0; pass < node.body.length; pass++) {
          for (const item of node.body) {
            const alias = item.declaration ?? item;
            if (alias.type === "TSTypeAliasDeclaration" && (designReference(alias.typeAnnotation) || alias.typeAnnotation.type === "TSLiteralType")) designTypes.add(alias.id.name);
          }
        }
        if (/\.[cm]?jsx?$/.test(context.filename)) report(node, "Client source must use TypeScript so design props are type checked.");
        for (const comment of context.sourceCode.getAllComments()) {
          if (/@ts-(?:expect-error|ignore|nocheck)\b/.test(comment.value)) report(node, "Type suppression bypasses typed design props.");
        }
      },
      "Program:exit"() {
        const seen = new Set();
        function check(node) {
          if (!node || seen.has(node)) return;
          seen.add(node);
          if ((node.type === "TSAsExpression" || node.type === "TSTypeAssertion") && name(node.typeAnnotation.typeName) !== "const") report(node, "Value casts bypass typed design props; keep the original type.");
          if (node.type === "Property" && name(node.key) === "style") report(node);
          if (node.type === "Property") paint(node, staticKey(node.key), node.value);
          if (node.type === "Identifier") {
            let scope = context.sourceCode.getScope(node);
            while (scope && !scope.set.has(node.name)) scope = scope.upper;
            for (const def of scope?.set.get(node.name)?.defs ?? []) {
              if (def.type === "Variable") check(def.node.init);
              if (def.type === "FunctionName") check(def.node.body);
            }
          }
          for (const key of context.sourceCode.visitorKeys[node.type] ?? []) {
            const child = node[key];
            if (Array.isArray(child)) child.forEach(check);
            else check(child);
          }
        }
        designValues.forEach(check);
      },
      JSXSpreadAttribute(node) { designValues.push(node.argument); },
      JSXAttribute(node) {
        if (markupProp(name(node.name.type === "JSXNamespacedName" ? node.name.name : node.name))) report(node, NO_MARKUP);
        paint(node, name(node.name.type === "JSXNamespacedName" ? node.name.name : node.name), node.value);
        if (node.name.type === "JSXNamespacedName" && name(node.name.namespace) === "style") tokenWrite(node, node.name.name);
        if (designProps.has(name(node.name))) designValues.push(node.value);
        if (!layout && name(node.name) === "style") report(node);
        if (!layout && node.name.type === "JSXNamespacedName" && name(node.name.name) === "style") report(node);
      },
      AssignmentExpression(node) {
        if (node.left.type === "MemberExpression" && markupProp(staticKey(node.left.property))) report(node, NO_MARKUP);
        if (node.left.type === "MemberExpression") tokenWrite(node, node.left.property);
      },
      MemberExpression(node) {
        const key = staticKey(node.property);
        if (["insertAdjacentHTML", "parseFromString", "createContextualFragment", "setHTMLUnsafe", "parseHTMLUnsafe"].includes(key) ||
          (["write", "writeln"].includes(key) && name(node.object.property ?? node.object) === "document")) report(node, NO_MARKUP);
      },
      CallExpression(node) {
        if (["setProperty", "removeProperty", "set", "delete"].includes(name(node.callee.property))) tokenWrite(node, node.arguments[0]);
      },
      Property(node) {
        // Includes imported JSX props and Object.assign payloads, not just literals
        // appearing directly in a JSX spread at the call site.
        if (markupProp(staticKey(node.key))) report(node, NO_MARKUP);
        tokenWrite(node, node.key);
      },
      TSAnyKeyword(node) { report(node, "Explicit any bypasses design types; use unknown and validate."); },
      TSTypeParameter(node) { typeParameters.add(node.name); },
      TSTypePredicate(node) { if (designReference(node.typeAnnotation)) report(node, "Design type predicates can forge branded tokens."); },
      TSDeclareFunction(node) { if (designReference(node.returnType)) report(node, "Design overloads can forge branded tokens."); },
      TSAsExpression(node) {
        if (!generated && (forbiddenType(node.typeAnnotation) || node.expression.type === "TSAsExpression" || node.expression.type === "TSTypeAssertion")) {
          report(node, "Value casts bypass typed design props; keep the original type.");
        }
      },
      TSTypeAssertion(node) {
        if (!generated && (forbiddenType(node.typeAnnotation) || node.expression.type === "TSAsExpression" || node.expression.type === "TSTypeAssertion")) report(node);
      },
      ImportDeclaration(node) {
        if (/\?(?:[^#]*&)?(?:raw|inline)(?:[&#]|$)/.test(node.source.value)) report(node, "Raw and inline imports bypass the shipped CSS gate.");
      },
      ImportExpression(node) {
        if (typeof node.source.value === "string" && /\?(?:[^#]*&)?(?:raw|inline)(?:[&#]|$)/.test(node.source.value)) report(node, "Raw and inline imports bypass the shipped CSS gate.");
      },
    };
  },
};

const KEY_EVENT = /^(?:on(?:capture)?:)?(?:on)?key(?:down|up|press)$/i;
const NO_KEYS = "Keys go through the keyboard layer (src/workbench/keys.ts, web/DESIGN.md \"Keyboard\"): bind a shortcut or push a layer, never a key listener of your own.";

/**
 * One keyboard layer (#2904): outside `src/workbench/keys.ts`, no key listener (`addEventListener`
 * or an `onkey*` property on any target) and no JSX key handler. The gallery's demo list widget is
 * the one exception (web/DESIGN.md "Keyboard").
 */
const keys = {
  meta: { schema: [] },
  create(context) {
    const path = relative(client, context.filename).replaceAll("\\", "/");
    if (path === "src/workbench/keys.ts") return {};
    const report = (node) => context.report({ node, message: NO_KEYS });
    const text = (node) => (node?.type === "Literal" ? String(node.value) : node?.type === "TemplateLiteral" && !node.expressions.length ? node.quasis[0].value.cooked : null);
    return {
      CallExpression(node) {
        if (name(node.callee.property ?? node.callee) === "addEventListener" && KEY_EVENT.test(text(node.arguments[0]) ?? "")) report(node);
      },
      AssignmentExpression(node) {
        if (node.left.type === "MemberExpression" && KEY_EVENT.test(name(node.left.property) ?? "")) report(node);
      },
      JSXAttribute(node) {
        const key = node.name.type === "JSXNamespacedName" ? `${name(node.name.namespace)}:${name(node.name.name)}` : name(node.name);
        if (KEY_EVENT.test(key ?? "") && !path.startsWith("src/gallery/")) report(node);
      },
    };
  },
};

export default [
  { ignores: ["node_modules/**", "dist/**", "artifacts/**", "tests/**", "e2e/**", "scripts/**", "test/**", "vite.config.ts", "eslint.config.js", "stylelint.config.js"] },
  {
    files: ["**/*.{js,jsx,ts,tsx,mts,mjs,cts,cjs}"],
    linterOptions: { noInlineConfig: true },
    languageOptions: {
      parser: babelParser,
      parserOptions: {
        requireConfigFile: false,
        babelOptions: { babelrc: false, configFile: false, presets: [["@babel/preset-typescript", { isTSX: true, allExtensions: true }]] },
      },
    },
    plugins: { design: { rules: { fence: design, keys } } },
    rules: { "design/fence": "error", "design/keys": "error", "no-eval": "error", "no-new-func": "error" },
  },
];
