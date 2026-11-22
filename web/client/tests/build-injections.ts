import { build } from "vite";
import { transformAsync, types as t, type NodePath } from "@babel/core";
import { relative, resolve } from "node:path";
import { CLIENT_ROOT } from "./build";
import clientConfig from "../vite.config";
const outDir = process.argv[2];
if (!outDir) throw new Error("Expected an output directory");
const root = resolve(CLIENT_ROOT);
const config=clientConfig({command:"build",mode:"production"});
await build({ ...config, configFile:false, root, build: { ...config.build, outDir, emptyOutDir:true, minify:false }, plugins:[{
  name:"design-injection-seeds", enforce:"pre",
  transformIndexHtml: { order:"pre", handler(html) { return html.replace("</body>", '<script type="module" src="/tests/design-injections.tsx"></script></body>'); } },
  async transform(code,id) {
    // Tag only style objects created by actual primitives. The framework consumes
    // them after the getter returns, including on later reactive updates.
    if(relative(root,id).startsWith("src/layout/primitives/") && id.endsWith(".tsx")) {
      const result=await transformAsync(code,{filename:id,babelrc:false,configFile:false,parserOpts:{plugins:["typescript","jsx"]},plugins:[()=>({visitor:{JSXAttribute(path: NodePath<t.JSXAttribute>) {
        if(t.isJSXIdentifier(path.node.name,{name:"style"}) && t.isJSXExpressionContainer(path.node.value) && t.isExpression(path.node.value.expression)) {
          path.node.value.expression=t.callExpression(t.identifier("__designStyle"),[path.node.value.expression]);
        }
      }}})]});
      return `const __designStyle = value => { globalThis.__designCSSOM?.values.add(value); return value };\n${result!.code}`;
    }
    if(/\/solid-js\/web\/dist\/(?:web|dev)\.js$/.test(id)) {
      if(!code.includes("function style(node, value, prev) {")) throw new Error("Solid style boundary changed");
      return code.replace("function style(node, value, prev) {","function unguardedStyle(node, value, prev) {")+`
function style(node,value,prev) {
  const scope=globalThis.__designCSSOM;
  if(!scope?.values.has(value)) return unguardedStyle(node,value,prev);
  scope.depth++;
  try { return unguardedStyle(node,value,prev); } finally { scope.depth--; }
}`;
    }
  },
}, ...(config.plugins ?? [])] });
