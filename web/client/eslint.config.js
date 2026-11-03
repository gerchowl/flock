/** Source and runtime design fences (#2933, web/DESIGN.md). */
import babelParser from "@babel/eslint-parser";

const NO_STYLE = "Inline style bypasses the design tokens: use a class, or a primitive from src/layout/ (web/DESIGN.md).";
const name = (node) => node?.name ?? node?.value;
const designProps = new Set(["space", "gutters", "justify", "align", "side", "sideWidth", "contentMin", "threshold", "max", "min", "intrinsic", "as"]);
const runtime = new Set(["style", "adoptedStyleSheets"]);

const design = {
  meta: { schema: [] },
  create(context) {
    const layout = /[/\\]src[/\\]layout[/\\]/.test(context.filename);
    const designValues = [];
    const report = (node, message = NO_STYLE) => context.report({ node, message });
    return {
      Program(node) {
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
        if (designProps.has(name(node.name))) designValues.push(node.value);
        if (!layout && name(node.name) === "style") report(node);
        if (!layout && node.name.type === "JSXNamespacedName" && name(node.name.name) === "style") report(node);
      },
      MemberExpression(node) {
        if (!layout && runtime.has(name(node.property))) report(node);
      },
      NewExpression(node) {
        if (!layout && name(node.callee.property ?? node.callee) === "CSSStyleSheet") report(node);
      },
      Property(node) {
        if (!layout && name(node.key) === "style") report(node);
      },
      TSAsExpression(node) {
        if (/^(?:any|SpaceStep|Token)$/.test(context.sourceCode.getText(node.typeAnnotation))) {
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
  {
    files: ["src/**/*.{js,jsx,ts,tsx,mts,mjs,cts,cjs}"],
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
