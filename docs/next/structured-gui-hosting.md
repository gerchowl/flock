# GUI bundle hosting

ADR-0038 §6 and ADR-0036 §5/§7 put the GUI bundle beside the server's structured
transport. Slice #1826 supplies the router. Server lifecycle integration, the
off-by-default listener and token-authenticated transport belong to #1828;
this slice does not expose a new CLI or listener by itself.

The GUI-owned #1731 build supplies `packages.<system>.flock-client` from
`nix/client.nix`: the Vite-built dist directory. Set the build-time
`FLOCK_GUI_DIST` environment variable to that output path when building the
`web` feature. A relative path is resolved against the Cargo manifest directory;
the default is `web/client/dist`. This build consumes the artifact without
fetching dependencies or running Vite. The assets, including debug builds, are
embedded with `rust-embed`; rebuild to consume a changed dist. A non-web build
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

HTTP Host must match the actual numeric loopback authority including port;
HTTP port 80 may be omitted in the standard browser form. An Origin, if present,
must be `http://` plus that authority, with the same default-port normalization. Opaque,
foreign, duplicate or malformed origins are refused with HTTP 403 and
`origin_mismatch`, before asset lookup or conditional caching. Browser
`Sec-Fetch-Site` values other than `none` and `same-origin` are refused too.
Requests without browser metadata are allowed when Host matches. This minimal
listener does not support hostname aliases, proxy origins or TLS termination.
Serving the static bundle does not grant token scopes or mutation authority.

## Paths and caching

- `/` and `/index.html` serve the entry document with `Cache-Control: no-cache`
  and a quoted SHA-256 ETag of its embedded bytes.
- Vite's standard `assets/<name>-<8-character-base64url-hash>.<extension>` files
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
bounded HTTP reads and cleanup. No live server or model is involved.
