# GUI bundle hosting

ADR-0038 §6 and ADR-0036 §5/§7 put the GUI bundle beside the server's structured
transport. Slice #1826 supplies the router. Server lifecycle integration, the
off-by-default listener and token-authenticated transport belong to #1828;
this slice does not expose a new CLI or listener by itself.

The GUI-owned #1731 build supplies `packages.<system>.flock-client` from
`nix/client.nix`: the Vite-built dist directory. `nix build .#flock-web` passes
that package output to `nix/package.nix` as `guiDist`, which sets the build-time
`FLOCK_GUI_DIST` environment variable only for the web feature. This wiring
uses the bundle provider (#1835); until it lands, the web package has no GUI
bundle and returns the typed HTTP 503 described below. Direct Nix package
callers can supply `guiDist` themselves; Cargo callers set `FLOCK_GUI_DIST` to the dist output path.
A relative path is resolved against the Cargo manifest directory;
the default is `web/client/dist`. This build consumes the artifact without
fetching dependencies or running Vite. The assets, including debug builds, are
embedded with `rust-embed`; rebuild to consume a changed dist. Dist roots and
entries that are symlinks (including dangling links) are refused before
embedding. Build inputs must remain unchanged during compilation; immutable
Nix outputs provide that guarantee. Only existing dist paths are watched; when
supplying a previously absent bundle, set `FLOCK_GUI_DIST` to its new output path.
Unchanged embed declarations are not rewritten. A non-web build
never declares or embeds the GUI assets and has no GUI router.

If the artifact or `index.html` is absent, requests return HTTP 503 with registered
`server_unavailable` and an instruction to supply `FLOCK_GUI_DIST`. The response
is `no-store`. This explicit refusal permits web builds while the independent
GUI build is unavailable; it never substitutes the terminal-bridge page.

## Mount contract

`web::gui::router(bind: SocketAddr) -> io::Result<axum::Router<()>>` takes the
actual listener address, rejects wildcard/routable addresses, and never binds.
Slice #1828 merges this stateless router after installing its own state and
`/v1/*` routes. The caller owns off-by-default activation, loopback binding,
funnel refusal, transport authentication and shutdown. The terminal bridge's
listener and routes remain separate.

This router requires the HTTP/1 `Host` header. HTTP/2 `:authority` without
`Host` receives HTTP 403; #1828 must mount it on an HTTP/1 listener.

HTTP Host must match the actual numeric loopback authority including port;
HTTP port 80 may be omitted in the standard browser form. An Origin, if present,
must be `http://` plus that authority, with the same default-port normalization. Opaque,
foreign, duplicate or malformed origins are refused with HTTP 403 and
`origin_mismatch`, before asset lookup or conditional caching. Browser
`Sec-Fetch-Site` values other than `none` and `same-origin` are refused too.
Requests without browser metadata are allowed when Host matches. This minimal
listener does not support hostname aliases, proxy origins or TLS termination.
Serving the static bundle does not grant token scopes or mutation authority.

## Document security policy

The entry document (`/` and `/index.html`) sends an enforcing
`Content-Security-Policy` on GET, HEAD and conditional 304 responses:

```text
default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self' ws://<listener-authority>/v1/ws; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'
```

The explicit WebSocket source uses the actual numeric IPv4 listener address and
port, never the request's Host or Origin. It supplements `'self'` for browsers
whose same-origin matching does not include WebSockets. IPv6 documents omit
that extra source: literal IPv6 addresses are invalid CSP host sources, so they
rely on CSP3's same-host/port WebSocket matching for `'self'`. Browsers without
that matching fail closed on IPv6; use the IPv4 listener with those browsers.
The policy allows same-origin HTTP API requests and the loopback WebSocket;
it does not grant token scopes or replace transport origin/auth checks.

The production Vite bundle uses external same-origin module scripts and CSS.
Inline scripts, event handlers, styles and string evaluation are blocked;
there is no `unsafe-inline` or `unsafe-eval` exception. The standalone fixture
preview's inlined bundle is not a production hosting input. A browser-shell
bootstrap that injects configuration must use an external same-origin script
or add a hash for its exact inline bytes; this router does not inject one.
Objects, base-URL overrides, framing by another page and form submissions are
blocked. Artifact sandbox policies remain separate (ADR-0038 §7).

The [CSP3 source-matching rules](https://www.w3.org/TR/CSP3/#match-url-to-source-expression)
define WebSocket matching; [MDN's connect-src reference](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/connect-src)
notes the browser difference for `'self'`.

## Paths and caching

- `/` and `/index.html` serve the entry document with `Cache-Control: no-cache`
  and a quoted SHA-256 ETag of its embedded bytes.
- Vite's anchored flat `assets/<name>-<8-character-base64url-hash>.<extension>` files
  (nonempty ASCII alphanumeric/underscore/hyphen name, alphanumeric extension)
  get `public, max-age=31536000, immutable`, with an ETag. Other files use
  `no-cache`, so a nonstandard naming scheme conservatively revalidates.
- GET conditional requests support matching, weak/list and wildcard
  `If-None-Match`, returning 304 with the same cache policy and ETag and no body.
  HEAD returns the same metadata without a body.
- Unknown assets and navigation paths return 404. There is no SPA fallback:
  ADR-0038's presentation navigation does not require server-side routes.
  `/v1` and `/v1/*` are reserved for the parent transport and never get GUI HTML
  or a GUI missing-bundle refusal. The parent must install the real routes.
- Successful responses set `nosniff`, explicit MIME types and
  `Vary: Origin, Sec-Fetch-Site`. Refusals are uncached and add no CORS allowance.

Tests serve a committed fixture dist over owned loopback TCP listeners, with
bounded HTTP reads and cleanup. `just gui-check` explicitly supplies the fixture
at compile time in both feature variants: the web router must serve it and
the non-web executable must exclude its bytes and refuse the web command.
Build-generator tests cover unchanged-file timestamps and symlink refusal. No live server or model is involved.
