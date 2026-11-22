import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { chromium, type Browser } from "playwright";
import { CLIENT_ROOT } from "./build";
import { installCSSOMGuard } from "./cssom-guard";

let root: string, browser: Browser, host: ReturnType<typeof Bun.spawn>, origin: string;
beforeAll(async () => {
  root = await mkdtemp(join(tmpdir(), "design-runtime-"));
  const build = Bun.spawn([process.execPath, "tests/build-injections.ts", root], {cwd:CLIENT_ROOT,stdout:"pipe",stderr:"pipe"});
  const output = await new Response(build.stdout).text()+await new Response(build.stderr).text();
  expect({code:await build.exited,output:output.includes("error") ? output : ""}).toEqual({code:0,output:""});
  // A real index.html attribute must be blocked by the served policy too.
  const index = join(root,"index.html");
  await writeFile(index,(await readFile(index,"utf8")).replace('<div id="root">','<div style="margin:13px" id="html-seed"></div><div id="root">'));
  host = Bun.spawn([process.execPath,"e2e/csp-server.ts",root,resolve(CLIENT_ROOT,"../../src/web/gui/document-policy.txt")], {cwd:CLIENT_ROOT,stdin:"pipe",stdout:"pipe",stderr:"inherit"});
  const ready = new Promise<number>(async (resolveReady,reject) => {
    host.exited.then(code=>reject(new Error(`design host exited ${code}`)));
    let buffer="";
    for await (const bytes of host.stdout as ReadableStream<Uint8Array>) {
      buffer+=new TextDecoder().decode(bytes);
      let end: number;
      while ((end=buffer.indexOf("\n"))>=0) { const event=JSON.parse(buffer.slice(0,end));buffer=buffer.slice(end+1);if(event.ready)resolveReady(event.ready.site); }
    }
  });
  origin=`http://127.0.0.1:${await Promise.race([ready,new Promise<never>((_,reject)=>setTimeout(()=>reject(new Error("design host timeout")),10000))])}`;
  browser=await chromium.launch();
},60000);
afterAll(async()=>{await browser?.close(); if(host){(host.stdin as import("bun").FileSink).end();await host.exited;}if(root)await rm(root,{recursive:true,force:true});});

const csp = ["setAttribute","computedAttribute","innerHTML","adjacentHTML","templateHTML","jsxHTML","styleElement","jsxStyle","attributeNode","dataLink"];
const cssom = ["styleProperty","styleSetter","elementStyle","assignStyle","computedAssign","doubleCast","setProperty"];
test.each([...csp,...cssom])("runtime layer rejects %s",async(name)=>{
  const page=await browser.newPage();
  try {
    if(!process.env.DESIGN_RUNTIME_BASELINE) await page.addInitScript(installCSSOMGuard);
    await page.goto(origin);
    await page.waitForFunction(()=>"designInjections" in window);
    if(csp.includes(name)) {
      const directive=await page.evaluate(async(name)=>{
        const event=new Promise<string>((resolve,reject)=>{document.addEventListener("securitypolicyviolation",e=>resolve(e.effectiveDirective),{once:true});setTimeout(()=>reject(new Error("missing CSP violation")),2000);});
        (window as unknown as {designInjections:Record<string,()=>void>}).designInjections[name]!();return event;
      },name);
      expect(directive).toMatch(/^style-src/);
    } else {
      const error=await page.evaluate((name)=>{try{(window as unknown as {designInjections:Record<string,()=>void>}).designInjections[name]!();return "accepted";}catch(error){return String(error);}},name);
      expect(error).toContain("design CSSOM: mutation outside");
    }
  } finally {await page.close();}
});
test("index style is blocked and a real primitive may style itself",async()=>{
  const page=await browser.newPage();
  try {
    await page.addInitScript(()=>{Object.assign(window,{designViolations:[]});document.addEventListener("securitypolicyviolation",e=>(window as unknown as {designViolations:string[]}).designViolations.push(e.effectiveDirective));});
    await page.addInitScript(installCSSOMGuard);
    await page.goto(origin);await page.waitForFunction(()=>"mountDesignPrimitive" in window);
    expect(await page.locator("#html-seed").evaluate(el=>getComputedStyle(el).marginTop)).toBe("0px");
    expect(await page.evaluate(()=>(window as unknown as {designViolations:string[]}).designViolations)).toContain("style-src-attr");
    await page.evaluate(()=>(window as unknown as {mountDesignPrimitive:()=>void}).mountDesignPrimitive());
    expect(await page.locator(".l-stack").last().evaluate(el=>(el as HTMLElement).style.getPropertyValue("--l-space"))).toBe("var(--space-4)");
    await page.evaluate(()=>(window as unknown as {changeDesignSpace:()=>void}).changeDesignSpace());
    expect(await page.locator(".l-stack").last().evaluate(el=>(el as HTMLElement).style.getPropertyValue("--l-space"))).toBe("var(--space-1)");
  } finally{await page.close();}
});
