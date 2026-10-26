/**
 * Loopback host for `e2e/csp.test.ts` (#1848), run as a child process so
 * Bun's own `Response` is used instead of the test preload's happy-dom one.
 *
 * Usage: `bun e2e/csp-server.ts <dist> <policy-file>`. Serves `<dist>` with
 * the entry document's policy (`LISTENER` replaced by this server's
 * authority), answers `POST /v1/client/exchange` with a fixed token and
 * accepts `/v1/ws`. A second "foreign" server on another port only counts
 * requests. Every observation is one JSON line on stdout; the first line
 * carries both ports. The process ends when stdin closes.
 */
export {};

const [dist, policyFile] = process.argv.slice(2);
if (!dist || !policyFile) throw new Error("usage: csp-server.ts <dist> <policy-file>");

const report = (event: object) => process.stdout.write(`${JSON.stringify(event)}\n`);

function contentType(path: string): string {
  if (path.endsWith(".html")) return "text/html; charset=utf-8";
  if (path.endsWith(".js")) return "application/javascript; charset=utf-8";
  if (path.endsWith(".css")) return "text/css; charset=utf-8";
  return "application/octet-stream";
}

const template = (await Bun.file(policyFile).text()).trim();
let policy = "";

const foreign = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  fetch(request) {
    report({ foreign: new URL(request.url).pathname });
    return new Response("foreign");
  },
});

const site = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request, server) {
    const url = new URL(request.url);
    report({ request: `${request.method} ${url.pathname}` });
    if (url.pathname === "/v1/ws") {
      return server.upgrade(request) ? undefined : new Response("upgrade required", { status: 400 });
    }
    if (url.pathname === "/v1/client/exchange" && request.method === "POST") {
      return Response.json({ token: "csp-probe-token" });
    }
    const path = url.pathname === "/" ? "/index.html" : url.pathname;
    const file = Bun.file(`${dist}${path}`);
    if (path.includes("..") || !(await file.exists())) {
      return new Response("not found", { status: 404 });
    }
    const headers: Record<string, string> = { "content-type": contentType(path) };
    if (path === "/index.html") headers["content-security-policy"] = policy;
    return new Response(file, { headers });
  },
  websocket: {
    open() {
      report({ socket: "open" });
    },
    message(_socket, message) {
      report({ frame: String(message) });
    },
  },
});

policy = template.replace("LISTENER", `127.0.0.1:${site.port}`);
report({ ready: { site: site.port, foreign: foreign.port } });

// Hold the servers until the test closes stdin.
await Bun.stdin.stream().pipeTo(new WritableStream());
site.stop(true);
foreign.stop(true);
