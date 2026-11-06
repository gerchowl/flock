import { expect, test } from "bun:test";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { CLIENT_ROOT } from "./build";

const violations = [
  ["generic assertion", 'function id<T>(x:unknown):T { return x as T }'],
  ["design predicate", 'function valid(x:unknown): x is SpaceStep { return true }'],
  ["overload lie", 'function lie(x:unknown):SpaceStep; function lie(x:unknown){return x}'],
  ["indexed assertion", 'const x = "14" as NonNullable<StackProps["space"]>'],
  ["typeof assertion", 'const ok = SPACE[4]; const x = "14" as typeof ok'],
  ["double assertion", 'const x = "14" as unknown as Record<string,string>'],
  ["eval", 'eval("1")'],
  ["Function", 'new Function("return 1")'],
] as const;
const positives = [
  ["style data", 'const config = {style:"compact"}'],
  ["plain animate", 'const object = {animate(){return 1}}; object.animate()'],
  ["unrelated union", 'type Mode = "compact"|"wide"; const mode = "compact" as Mode'],
  ["const branded props", 'const props = {space: SPACE[4]} as const; const view = <Stack {...props} />'],
] as const;
async function lint(code: string, file = "src/round3.tsx") {
  const child = Bun.spawn([process.execPath, "x", "eslint", "--config", "eslint.config.js", "--stdin", "--stdin-filename", file], {
    cwd: CLIENT_ROOT, stdin: new TextEncoder().encode(code), stdout: "pipe", stderr: "pipe",
  });
  return { code: await child.exited, output: await new Response(child.stdout).text() + await new Response(child.stderr).text() };
}
test.each(violations)("round 3 rejects %s", async (_name, source) => { expect((await lint(source)).code).toBe(1); });
test.each(positives)("round 3 accepts %s", async (_name, source) => { expect((await lint(source)).code).toBe(0); });
test("round 3 anchors primitive authority", async () => {
  expect((await lint('<div style={{"--l-space":"1px"}} />', "lib/src/layout/primitives/Fake.tsx")).code).toBe(1);
});
test("round 3 brands cannot be constructed from literals and plain assign stays typed", async () => {
  const root = await mkdtemp(join(CLIENT_ROOT, "node_modules/.cache/round3-"));
  try {
    await writeFile(join(root, "probe.ts"), `import { SPACE, type SpaceStep, type Token } from "${CLIENT_ROOT}/src/tokens";
const good: SpaceStep = SPACE[4];
const merged = Object.assign({}, {space:SPACE[4]}, {mode:"compact"});
const kept: SpaceStep = merged.space;
// @ts-expect-error only generated spacing constants carry the brand.
const bad: SpaceStep = "4";
// @ts-expect-error only generated token constants carry the brand.
const token: Token = "color-label-primary";
`);
    const child = Bun.spawn([process.execPath, "scripts/check-design-types.ts", join(root,"probe.ts")], {cwd:CLIENT_ROOT,stdout:"pipe",stderr:"pipe"});
    const output = await new Response(child.stdout).text()+await new Response(child.stderr).text();
    expect({code:await child.exited,output}).toEqual({code:0,output:""});
  } finally { await rm(root,{recursive:true,force:true}); }
});
