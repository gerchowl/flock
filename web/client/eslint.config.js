/** Source and runtime design fences (#2933, web/DESIGN.md). */
import babelParser from "@babel/eslint-parser";

const NO_STYLE = "Inline style bypasses the design tokens: use a class, or a primitive from src/layout/ (web/DESIGN.md).";
const name = (node) => node?.name ?? node?.value;
const designProps = new Set(["space", "gutters", "justify", "align", "side", "sideWidth", "contentMin", "threshold", "max", "min", "intrinsic", "as"]);
const runtime = new Set(["style", "adoptedStyleSheets", "attributeStyleMap", "styleSheets", "animate", "insertRule"]);

const design = {
  meta: { schema: [] },
  create(context) {
    const layout = /[/\\]src[/\\]layout[/\\]primitives[/\\]/.test(context.filename);
    const designValues = [];
    const designTypes = new Set(["SpaceStep", "Token"]);
    function forbiddenType(node) {
      if (!node) return false;
      if (node.type === "TSLiteralType") return typeof node.literal.value === "string" || node.literal.type === "TemplateLiteral";
      if (node.type === "TSTypeReference") return designTypes.has(name(node.typeName.right ?? node.typeName));
      if (node.type === "TSParenthesizedType") return forbiddenType(node.typeAnnotation);
      return (node.type === "TSUnionType" || node.type === "TSIntersectionType") && node.types.some(forbiddenType);
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
    return {
      Program(node) {
        for (const item of node.body) if (item.type === "ImportDeclaration") {
          for (const specifier of item.specifiers) if (designTypes.has(name(specifier.imported))) designTypes.add(specifier.local.name);
        }
        for (let pass = 0; pass < node.body.length; pass++) {
          for (const item of node.body) {
            const alias = item.declaration ?? item;
            if (alias.type === "TSTypeAliasDeclaration" && forbiddenType(alias.typeAnnotation)) designTypes.add(alias.id.name);
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
          if (node.type === "TSAsExpression" || node.type === "TSTypeAssertion") report(node, "Value casts bypass typed design props; keep the original type.");
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
        if (node.name.type === "JSXNamespacedName" && name(node.name.namespace) === "style") tokenWrite(node, node.name.name);
        if (designProps.has(name(node.name))) designValues.push(node.value);
        if (!layout && name(node.name) === "style") report(node);
        if (!layout && node.name.type === "JSXNamespacedName" && name(node.name.name) === "style") report(node);
      },
      AssignmentExpression(node) {
        if (node.left.type === "MemberExpression") tokenWrite(node, node.left.property);
      },
      CallExpression(node) {
        if (["setProperty", "removeProperty", "set", "delete"].includes(name(node.callee.property))) tokenWrite(node, node.arguments[0]);
      },
      MemberExpression(node) {
        if (!layout && runtime.has(name(node.property))) report(node);
      },
      NewExpression(node) {
        if (!layout && name(node.callee.property ?? node.callee) === "CSSStyleSheet") report(node);
      },
      Property(node) {
        tokenWrite(node, node.key);
        if (!layout && name(node.key) === "style") report(node);
      },
      TSAnyKeyword(node) { report(node, "Explicit any bypasses design types; use unknown and validate."); },
      TSAsExpression(node) {
        if (forbiddenType(node.typeAnnotation)) {
          report(node, "Value casts bypass typed design props; keep the original type.");
        }
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
    plugins: { design: { rules: { fence: design } } },
    rules: { "design/fence": "error" },
  },
];
